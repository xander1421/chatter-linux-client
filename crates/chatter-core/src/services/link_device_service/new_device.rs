use super::*;

impl LinkDeviceService {
    /// Start a provisioning session as a new device.
    ///
    /// Generates the ephemeral X25519 keypair locally, calls the server,
    /// builds the QR URL. The returned session must be kept in memory until
    /// the envelope arrives — its private key cannot be persisted safely
    /// without compromising secrecy.
    pub async fn start_as_new_device(
        &self,
        device_info: &str,
        device_name: &str,
    ) -> Result<NewDeviceSession, LinkDeviceError> {
        let ephemeral = ProvisioningKeypair::generate();
        let pub_b64 = base64_url(&ephemeral.public_bytes());

        let mut client = self.provisioning_client()?;
        let response = client.start_provisioning(device_info).await?;

        let server_url = self.grpc_pool.endpoint();
        let qr_url = build_link_url(
            &response.provisioning_uuid,
            &pub_b64,
            Some(&server_url),
            Some(device_name),
        );

        Ok(NewDeviceSession {
            uuid: response.provisioning_uuid,
            qr_url,
            expires_at: response.expires_at,
            ephemeral,
            device_info: device_info.to_string(),
            device_name: device_name.to_string(),
        })
    }

    /// Poll until the primary uploads the envelope.
    ///
    /// Returns the decrypted [`ProvisionMessage`] on success. Caller is then
    /// responsible for [`complete_linking`] to finalize the device on the
    /// server.
    pub async fn poll_until_envelope(
        &self,
        session: &NewDeviceSession,
    ) -> Result<ProvisionMessage, LinkDeviceError> {
        let mut client = self.provisioning_client()?;

        loop {
            let result = client.poll_provisioning_message(&session.uuid).await?;
            match result {
                ProvisioningPollResult::Envelope(env) => {
                    return decrypt_envelope(&session.ephemeral, &env)
                        .map_err(LinkDeviceError::Crypto);
                }
                ProvisioningPollResult::Pending { retry_after_ms } => {
                    let delay = retry_after_ms.clamp(1_000, 10_000);
                    tokio::time::sleep(Duration::from_millis(delay as u64)).await;
                }
                ProvisioningPollResult::Expired { message }
                | ProvisioningPollResult::Cancelled { message } => {
                    return Err(LinkDeviceError::SessionEnded(message));
                }
            }
        }
    }

    /// Complete the link using the decrypted [`ProvisionMessage`] and an
    /// already-generated prekey bundle.
    ///
    /// On success this:
    /// 1. Imports the primary's identity keypair into the local key store.
    /// 2. Persists current_user_id, device_id, profile key, auth token.
    /// 3. Configures the auth interceptor for Basic auth.
    ///
    /// The caller is responsible for: generating the prekey bundle (use
    /// [`PreKeyManager`] against a fresh registration ID), kicking off
    /// background services, and routing the UI to the main view.
    pub async fn complete_linking(
        &self,
        session: NewDeviceSession,
        provision: ProvisionMessage,
    ) -> Result<CompletedLink, LinkDeviceError> {
        if provision.aci_identity_key_private.len() != 32 {
            return Err(LinkDeviceError::Invalid(
                "ProvisionMessage.aci_identity_key_private must be 32 bytes".into(),
            ));
        }
        if provision.profile_key.len() != 32 {
            return Err(LinkDeviceError::Invalid(
                "ProvisionMessage.profile_key must be 32 bytes".into(),
            ));
        }
        if provision.aci.is_empty() {
            return Err(LinkDeviceError::Invalid(
                "ProvisionMessage.aci is required".into(),
            ));
        }
        // Without the salt this device can never derive the PIN's SVR secret
        // (check the PIN, rewrite the backup); without the handle it shows none.
        if provision.svr_salt_username.is_empty() || provision.full_username.is_empty() {
            return Err(LinkDeviceError::Invalid(
                "ProvisionMessage.svr_salt_username and full_username are required".into(),
            ));
        }

        let user_id: crate::domain::ids::UserId = provision
            .aci
            .parse()
            .map_err(|e| LinkDeviceError::Invalid(format!("invalid aci: {e}")))?;

        // Linking imports the primary's history (Link'n'Sync), Signal's
        // "Transfer" relink, which deletes all local data first
        // (DialogRelink.dom.tsx -> DeleteDataAndRelinkConfirmationDialog ->
        // removeAllData). Only an install the server unlinked has an old
        // history here to delete; it goes before any key of the new link is
        // generated, since generating them already writes the key store.
        if crate::services::auth_service::unlinked_account(&self.settings_repo).is_some() {
            crate::services::auth_service::wipe_unlinked_install(&self.database)?;
        }

        // Reconstruct identity keypair from primary's private key.
        let priv_bytes = Zeroizing::new(provision.aci_identity_key_private.clone());
        let private = PrivateKey::deserialize(priv_bytes.as_slice())
            .map_err(|e| CryptoError::Protocol(format!("identity priv parse: {e}")))?;
        let public = private
            .public_key()
            .map_err(|e| CryptoError::Protocol(format!("derive identity pub: {e}")))?;
        let identity_key = IdentityKey::new(public);
        let identity_pair = IdentityKeyPair::new(identity_key, private);

        // Generate fresh registration ID and keys synchronously, keeping RNG
        // state out of the async state machine.
        let prekey_manager = PreKeyManager::new(self.key_store.clone());
        #[allow(clippy::type_complexity)]
        fn gen_keys(
            prekey_manager: &PreKeyManager,
            identity_pair: &IdentityKeyPair,
        ) -> Result<
            (
                u32,
                libsignal_protocol::SignedPreKeyRecord,
                libsignal_protocol::KyberPreKeyRecord,
                Vec<libsignal_protocol::PreKeyRecord>,
                Vec<libsignal_protocol::KyberPreKeyRecord>,
            ),
            CryptoError,
        > {
            let mut rng = rand::rngs::OsRng.unwrap_err();
            let reg_id = (rand::Rng::random::<u32>(&mut rng) % 0x3FFF) + 1;
            let signed = prekey_manager.generate_signed_prekey(&mut rng, 1, identity_pair)?;
            // The medium-term LAST-RESORT PQSPK (id 1) — reused across
            // handshakes until rotated, so it is a floor, not the primary PQ
            // key material.
            let kyber = prekey_manager.generate_kyber_prekey(1, identity_pair)?;
            let one_time = prekey_manager.generate_prekeys(&mut rng, 1, 100)?;

            // The one-time Kyber prekey pool (PQOPK), same batch size as the
            // classical OPKs above. Per PQXDH §3.2 the server pops one PQOPK for
            // every classical OPK it hands out, so the two pools MUST be seeded
            // in lockstep. A linked device is a full recipient of every message
            // the account receives; without its own pool the server has nothing
            // to pop for it and every PQXDH handshake to this device falls back
            // to the single last-resort PQSPK above — spec-permitted, but one
            // reused PQ secret across every session (see the warning at
            // crypto/signal/identity.rs:38-42).
            //
            // This mirrors `services/auth_service.rs::register`, which seeds the
            // same pool for the device created by registration. IDs start at 2:
            // the last-resort PQSPK holds id 1, and `generate_kyber_prekey`
            // persists each record, so `get_next_kyber_prekey_id` advances as we
            // go.
            let mut kyber_one_time = Vec::with_capacity(100);
            for _ in 0..100 {
                let id = prekey_manager.get_next_kyber_prekey_id()?;
                kyber_one_time.push(prekey_manager.generate_kyber_prekey(id, identity_pair)?);
            }
            Ok((reg_id, signed, kyber, one_time, kyber_one_time))
        }
        let (registration_id, signed, kyber, one_time, kyber_one_time) =
            gen_keys(&prekey_manager, &identity_pair)?;

        // Identity-overwrite guard. If a local identity already exists,
        // refuse to import a *different* one — overwriting would silently
        // break every established session on this device. Same identity is
        // idempotent (lets a user retry a half-completed link).
        let identity_pair_serialized = identity_pair.serialize();
        if let Some((_existing_reg, existing_pair)) =
            IdentityKeyManager::new(self.key_store.clone()).get_identity()?
        {
            let existing_serialized = existing_pair.serialize();
            if existing_serialized.as_ref() != identity_pair_serialized.as_ref() {
                return Err(LinkDeviceError::Invalid(
                    "local identity already exists and differs from incoming \
                     ProvisionMessage — refuse to overwrite (sign out + clear \
                     local data first if you really mean to re-link)"
                        .into(),
                ));
            }
        }

        // NOTE: identity import is *deferred* until the server confirms the
        // device-linking RPC succeeded. Any failure between here and
        // `complete_device_linking` returning Ok leaves local key_store +
        // settings untouched, so a retry can proceed cleanly. Prekey
        // generation above is in-memory only.

        // Profile key + auth token (server-side token is bootstrapped via
        // device_credentials::generate; the server also expects it in
        // CompleteDeviceLinkingRequest, supplied below).
        let mut profile_key = [0u8; 32];
        profile_key.copy_from_slice(&provision.profile_key);
        let credentials = DeviceCredentials::generate(/* placeholder */ 1, &profile_key);
        let device_auth_token = credentials.auth_token().as_bytes();

        use crate::proto::chat_v1::SignalPreKeyBundle;
        use crate::proto::chat_v1::signal_pre_key_bundle::{
            KyberPreKeyRecord as ProtoKyberPreKey, PreKeyRecord as ProtoPreKey,
            SignedPreKeyRecord as ProtoSignedPreKey,
        };
        use libsignal_protocol::GenericSignedPreKey;

        let signed_id: u32 = signed
            .id()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .into();
        let signed_pub = signed
            .public_key()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .serialize()
            .to_vec();
        let signed_sig = signed
            .signature()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;
        // Chatter-owned wire field is NANOSECONDS; libsignal stores millis.
        // The server reads this with time.Unix(0, ts), so sending millis dated
        // the linked device's signed prekey to 1970-01-01T00:29:44Z — ~56 years
        // "old". Registration already converts (see auth_service.rs); this path
        // did not, so every linked bundle was stored pre-aged and would be
        // treated as long expired by any age check.
        let signed_ts = signed
            .timestamp()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .epoch_millis()
            * 1_000_000;

        let kyber_id: u32 = kyber
            .id()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .into();
        let kyber_pub = kyber
            .public_key()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .serialize()
            .to_vec();
        let kyber_sig = kyber
            .signature()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;

        let identity_pub_full = identity_pair.identity_key().serialize().to_vec();

        let one_time_first = if let Some(pk) = one_time.first() {
            let id: u32 = pk
                .id()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .into();
            let pub_bytes = pk
                .public_key()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .serialize()
                .to_vec();
            Some(ProtoPreKey {
                id,
                public_key: pub_bytes,
            })
        } else {
            None
        };

        let prekey_bundle = SignalPreKeyBundle {
            identity_key: identity_pub_full.clone(),
            registration_id,
            // Placeholder. The server assigns the real device_id in
            // CreateLinkedDevice (MAX(device_id)+1) and ignores this field
            // entirely — the new device cannot know its id before the server
            // allocates it. This said 0, but the shared SignalPreKeyBundle
            // contract requires device_id >= 1 (common.proto:167, correct for
            // every other use of the message: a real device is never 0), so
            // the server rejected every CompleteDeviceLinking with
            // "prekey_bundle.device_id: must be greater than or equal to 1"
            // and linking could never complete. 1 matches the placeholder
            // already used for DeviceCredentials::generate above.
            device_id: 1,
            signed_prekey: Some(ProtoSignedPreKey {
                id: signed_id,
                public_key: signed_pub,
                signature: signed_sig,
                timestamp: signed_ts,
            }),
            one_time_prekey: one_time_first,
            kyber_prekey: Some(ProtoKyberPreKey {
                id: kyber_id,
                public_key: kyber_pub,
                signature: kyber_sig,
                // Nanoseconds, as above — this field had the same millis bug.
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or_default(),
            }),
            unrestricted_unidentified_access: Some(false),
        };

        let mut initial_one_time: Vec<ProtoPreKey> = Vec::new();
        for pk in one_time.iter().skip(1) {
            let id: u32 = pk
                .id()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .into();
            let pub_bytes = pk
                .public_key()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .serialize()
                .to_vec();
            initial_one_time.push(ProtoPreKey {
                id,
                public_key: pub_bytes,
            });
        }

        // Build the PQOPK batch for the server. Unlike the classical OPK, every
        // PQ prekey carries an XEdDSA signature over its public key (PQXDH §3.2);
        // the server rejects the batch outright if it does not verify. None are
        // held back for the bundle: the bundle's `kyber_prekey` field is the
        // last-resort PQSPK, and the server picks a PQOPK from this pool at
        // handout time.
        let mut initial_kyber_one_time: Vec<ProtoKyberPreKey> = Vec::new();
        for kpk in &kyber_one_time {
            let id: u32 = kpk
                .id()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .into();
            let public = kpk
                .public_key()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .serialize()
                .to_vec();
            let sig = kpk
                .signature()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?;
            // Chatter-owned wire field is nanoseconds; libsignal stores millis.
            let ts = kpk
                .timestamp()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .epoch_millis()
                * 1_000_000;
            initial_kyber_one_time.push(ProtoKyberPreKey {
                id,
                public_key: public,
                signature: sig,
                timestamp: ts,
            });
        }

        // Encrypted to the account identity key we were just handed, so only
        // the account's own devices can read it (Signal's encryptDeviceName).
        let encrypted_name = crate::crypto::device_name::encrypt_device_name(
            &session.device_name,
            identity_pair.identity_key(),
        )?;

        let mut client = self.provisioning_client()?;
        let response = client
            .complete_device_linking(
                &provision.provisioning_code,
                device_auth_token,
                &session.device_info,
                encrypted_name,
                prekey_bundle,
                initial_one_time,
            )
            .await?;

        let device_id = response.device_id;

        // The server sends our display name encrypted with the provisioned
        // profile key. Decrypt before the local commit so its write is covered
        // by the same server cleanup on failure.
        let display_name = response
            .user_profile
            .as_ref()
            .map(|p| p.display_name.clone())
            .filter(|s| !s.is_empty())
            .and_then(|encrypted_b64| {
                use base64::Engine;
                let pk: [u8; 32] = profile_key[..].try_into().ok()?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&encrypted_b64)
                    .ok()?;
                crate::crypto::profile_crypto::decrypt_display_name(&pk, &bytes)
                    .ok()
                    .filter(|n| !n.is_empty())
            });

        // Server confirmed — NOW commit identity + auth state. Earlier
        // failure modes (network, validation, server reject) leave the
        // local key store unmodified so a retry starts from clean state.
        // The overwrite guard above already refused a different identity, so
        // an existing row here is this same keypair (a re-link after revoke).
        let local_commit = (|| -> Result<(), LinkDeviceError> {
            if self.key_store.has_local_identity()? {
                self.key_store
                    .update_local_registration_id(registration_id)?;
            } else {
                self.key_store
                    .set_local_identity(registration_id, &identity_pair_serialized)?;
            }

            self.settings_repo
                .set_string(keys::CURRENT_USER_ID, &user_id.to_string())?;
            self.settings_repo
                .set(keys::CURRENT_DEVICE_ID, &device_id)?;
            self.settings_repo
                .set_bytes(keys::PROFILE_KEY, &profile_key)?;
            self.settings_repo
                .set_bytes(keys::DEVICE_AUTH_TOKEN, device_auth_token)?;
            self.settings_repo
                .set_bytes(keys::IDENTITY_KEY_PUBLIC, &identity_pub_full)?;
            self.settings_repo
                .set_string(keys::SVR_SALT_USERNAME, &provision.svr_salt_username)?;
            self.settings_repo
                .set_string(keys::FULL_USERNAME, &provision.full_username)?;
            adopt_provisioned_settings(&self.settings_repo, &user_id, &provision)?;
            if let Some(ref name) = display_name {
                self.settings_repo.set_string(keys::DISPLAY_NAME, name)?;
            }
            Ok(())
        })();

        if let Err(original_error) = local_commit {
            // Logout of the newly authenticated device retires its server row,
            // prekeys and queued messages. Use a private interceptor so a
            // failed local commit never authenticates the app as this device.
            let cleanup_auth = AuthInterceptor::new();
            let linked_credentials = DeviceCredentials::from_stored(
                device_id,
                *credentials.auth_token().as_bytes(),
                *credentials.unidentified_access_key(),
            );
            cleanup_auth.set_basic(
                linked_credentials.auth_username(&user_id.to_string()),
                linked_credentials.auth_token_base64(),
            );
            let mut cleanup_client = AuthClient::new(self.grpc_pool.channel(), cleanup_auth);
            match cleanup_client.logout(device_id).await {
                Ok(()) => {
                    tracing::info!(device_id, "Retired linked device after local commit failed")
                }
                Err(_) => tracing::error!(
                    device_id,
                    "Could not retire linked device after local commit failed"
                ),
            }
            return Err(original_error);
        }

        // Configure auth interceptor for Basic auth.
        //
        // Rebind the credentials to the device_id the SERVER just assigned.
        // `credentials` above was built with a placeholder of 1 because this
        // device cannot know its own id until CompleteDeviceLinking returns, and
        // auth_username() formats `{user_id}.{device_id}` — so authenticating
        // with the placeholder claims to be device 1, the PRIMARY. The server
        // verifies the token against (user_id, device_id), and this device's
        // token is registered under its real id, so every authenticated request
        // from a linked device was rejected. The auth token and UAK are
        // unchanged; only the id they are presented under is corrected.
        let credentials = DeviceCredentials::from_stored(
            device_id,
            *credentials.auth_token().as_bytes(),
            *credentials.unidentified_access_key(),
        );
        self.grpc_pool.auth().set_basic(
            credentials.auth_username(&user_id.to_string()),
            credentials.auth_token_base64(),
        );

        // Seed this device's PQOPK pool on the server. Reuses the existing
        // RefreshPreKeys RPC rather than adding a field to
        // CompleteDeviceLinking, which also satisfies both ordering
        // constraints for free:
        //   - signal_kyber_one_time_prekeys has an FK on
        //     signal_prekey_bundles(user_id, device_id), so this device's
        //     bundle must already exist — i.e. after complete_device_linking.
        //   - RefreshPreKeys is authenticated, so this must follow set_basic
        //     directly above.
        // The classical OPKs rode along inside CompleteDeviceLinkingRequest and
        // are already uploaded by this point; only the PQ pool is sent here.
        // This is the same shape as the registration-side seeding in
        // `services/auth_service.rs::register`.
        //
        // A failure here is logged, not fatal: the device is already linked
        // server-side, and PQXDH still succeeds against the last-resort PQSPK
        // (spec-permitted). Failing the link outright — after the identity and
        // auth state above are committed — would strand a half-linked device,
        // a worse outcome than the degradation it avoids. The background
        // replenisher refills both pools together on its next pass.
        if !initial_kyber_one_time.is_empty() {
            let pq_count = initial_kyber_one_time.len();
            let mut auth_client = self.grpc_pool.auth_client();
            match auth_client
                .refresh_pre_keys_with_kyber_otpk(Vec::new(), initial_kyber_one_time)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        device_id,
                        pq_opk = pq_count,
                        "Seeded PQOPK pool for linked device"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        device_id,
                        error = %e,
                        "Failed to seed PQOPK pool for linked device — PQXDH will fall back \
                         to the last-resort PQSPK until the replenisher runs"
                    );
                }
            }
        }

        // Link'n'Sync import (§30): if the primary exported a transfer archive,
        // fetch it, decrypt with the ephemeral-key-derived KEK, and import the
        // message history + social graph. Best-effort, exactly like the PQOPK
        // seed above: the device is already linked, so a poll/decrypt failure
        // leaves it linked-without-history, never stranded. Absent key = the
        // primary offered no history, which is a legal link.
        if let Some(eph) = provision.ephemeral_backup_key.as_deref() {
            if eph.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(eph);
                self.import_transfer_archive(
                    &session.uuid,
                    &key,
                    &provision.aci,
                    provision.primary_device_registration_id,
                )
                .await;
                key.zeroize();
            } else {
                tracing::warn!(
                    "provision ephemeral_backup_key is not 32 bytes; linking without history"
                );
            }
        }

        Ok(CompletedLink {
            user_id,
            device_id,
            display_name,
        })
    }
}

/// Drive the full new-device flow: start → render QR → poll → complete.
/// Emits progress on `tx`. Terminal event is always `Linked`, `Failed`, or
/// `Cancelled`. Cleans up `active_uuid` before returning regardless.
pub(super) async fn run_flow(
    svc: Arc<LinkDeviceService>,
    device_info: String,
    device_name: String,
    tx: mpsc::UnboundedSender<LinkProgress>,
) {
    // 1. Start session + render QR.
    let session = match svc.start_as_new_device(&device_info, &device_name).await {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(LinkProgress::Failed(format!("start failed: {e}")));
            return;
        }
    };
    let uuid = session.uuid.clone();
    let url = session.qr_url.clone();
    let expires_at = session.expires_at;

    let qr_png = match qr_png_for_link_url(&url, 512) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(LinkProgress::Failed(format!("QR render: {e}")));
            return;
        }
    };

    *svc.active_uuid.lock() = Some(uuid.clone());

    let _ = tx.send(LinkProgress::QrReady {
        url,
        qr_png,
        expires_at,
        verification_code: verification_code_for_pub(&session.ephemeral.public_bytes()),
    });
    let _ = tx.send(LinkProgress::WaitingForApproval);

    // 2. Poll for envelope.
    let provision = match svc.poll_until_envelope(&session).await {
        Ok(p) => p,
        Err(LinkDeviceError::SessionEnded(msg)) => {
            *svc.active_uuid.lock() = None;
            let _ = tx.send(LinkProgress::Cancelled);
            tracing::info!(reason = %msg, "Provisioning session ended");
            return;
        }
        Err(e) => {
            *svc.active_uuid.lock() = None;
            let _ = tx.send(LinkProgress::Failed(format!("poll failed: {e}")));
            return;
        }
    };

    *svc.active_uuid.lock() = None;
    let _ = tx.send(LinkProgress::Approved);

    // 3. Complete linking.
    match svc.complete_linking(session, provision).await {
        Ok(c) => {
            let _ = tx.send(LinkProgress::Linked(c));
        }
        Err(e) => {
            let _ = tx.send(LinkProgress::Failed(format!("complete failed: {e}")));
        }
    }
}

/// Adopt the primary's privacy settings a [`ProvisionMessage`] carries, as
/// from a `ConfigurationSync` of our own: call once `user_id` is stored as the
/// current user.
pub(super) fn adopt_provisioned_settings(
    settings: &SettingsRepository,
    user_id: &crate::domain::ids::UserId,
    provision: &ProvisionMessage,
) -> Result<(), StorageError> {
    crate::services::configuration_sync::apply(
        settings,
        user_id,
        &crate::proto::content_v1::ConfigurationSync {
            read_receipts: provision.read_receipts,
            typing_indicators: provision.typing_indicators,
        },
    )?;
    Ok(())
}
