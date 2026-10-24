use super::*;

impl AuthService {
    /// Register a new user account.
    ///
    /// Orchestrates:
    /// 1. Identity key generation
    /// 2. Prekey bundle generation
    /// 3. Device credential generation
    /// 4. Server registration
    /// 5. Local state persistence
    /// 6. Auth interceptor configuration
    ///
    /// # Arguments
    ///
    /// * `avatar` - Optional avatar image bytes (stored locally, encrypted when shared)
    pub async fn register(
        &self,
        auth_client: &mut AuthClient,
        username: &str,
        password: &str,
        display_name: &str,
        device_info: &str,
        avatar: Option<&[u8]>,
        pin: Option<&str>,
    ) -> Result<LoginResult, AuthError> {
        // Check not already logged in
        if self.is_logged_in() {
            return Err(AuthError::AlreadyLoggedIn);
        }

        // Recover from a prior aborted register: identity + prekeys may be
        // persisted locally without a user session (server call failed
        // after the crypto step). `generate_identity` refuses to overwrite,
        // so wipe the stale crypto state now. Safe because `is_logged_in`
        // is false — no real sessions to break.
        if self.identity_manager.has_identity()? {
            self.identity_manager.reset_for_new_registration()?;
        }

        // SECURITY: Profile key stays client-side only - never sent to server.
        // Signal-aligned: profile_key is 32 random bytes from CSPRNG, NOT
        // derived from password. See `crypto::auth_crypto::generate_profile_key`.
        // SVR backup uploads the wrapped profile_key after registration so
        // other devices can recover it via PIN.
        let profile_key = generate_profile_key();
        let username_hash = login_identifier(username);

        // Pick a Signal-style `nickname.discriminator` candidate. The random
        // discriminator binds into the Ristretto255 lookup hash so wordlist
        // enumeration of registered nicknames is infeasible. On uniqueness
        // collision the server rejects and the user re-runs registration.
        let full_username = username_candidates(username)
            .map_err(|e| AuthError::RegistrationFailed(format!("invalid nickname: {e}")))?
            .into_iter()
            .next()
            .ok_or_else(|| AuthError::RegistrationFailed("no username candidates".into()))?;

        // Derive SVR public key and registration lock - required for registration
        //
        // PERF: registration needs three PIN-derived values (SVR keypair,
        // registration lock, and the local PIN fingerprint set further down).
        // All three are HMAC/X25519 over the SAME Argon2id secret for the same
        // (pin, username), so the ~450ms/128 MiB KDF runs ONCE here and the
        // cheap derivations reuse it. It previously ran once per value —
        // identical inputs, bit-identical output, ~900ms of pure waste — and,
        // being called inline from async, it stalled the runtime's connection
        // task hard enough to kill the not-yet-issued challenge request. See
        // `derive_svr_secret_async` for that failure mode.
        let mut svr_secret: [u8; 32];
        let (svr_public_key, mut registration_lock) = if let Some(p) = pin {
            crate::crypto::svr::validate_pin(p)
                .map_err(|e| AuthError::RegistrationFailed(format!("invalid PIN: {e}")))?;
            let secret = crate::crypto::svr::derive_svr_secret_async(p, username).await?;
            let keypair = crate::crypto::svr::SvrKeypair::from_secret(&secret);
            // SECURITY: Registration lock prevents account takeover without PIN
            let reg_lock = crate::crypto::svr::registration_lock_from_secret(&secret);
            svr_secret = secret;
            (keypair.public_key_bytes().to_vec(), reg_lock)
        } else {
            return Err(AuthError::RegistrationFailed(
                "PIN is required for account recovery".to_string(),
            ));
        };

        // Generate new identity (always fresh for registration)
        let (registration_id, identity_key_pair) = self
            .identity_manager
            .generate_identity(&mut rand::rngs::OsRng.unwrap_err())?;

        // Generate signed prekey
        let signed_prekey = self.prekey_manager.generate_signed_prekey(
            &mut rand::rngs::OsRng.unwrap_err(),
            1,
            &identity_key_pair,
        )?;

        // Generate Kyber prekey for post-quantum resistance. This is the
        // medium-term LAST-RESORT PQSPK — it is reused across handshakes until
        // rotated, so it is a floor, not the primary PQ key material.
        let kyber_prekey = self
            .prekey_manager
            .generate_kyber_prekey(1, &identity_key_pair)?;

        // Generate one-time prekeys (100 for initial batch)
        let one_time_prekeys =
            self.prekey_manager
                .generate_prekeys(&mut rand::rngs::OsRng.unwrap_err(), 1, 100)?;

        // Generate the one-time Kyber prekey pool (PQOPK), same batch size as
        // the classical OPKs above. Per PQXDH §3.2 the server pops one PQOPK
        // for every classical OPK it hands out, so the two pools MUST be seeded
        // and refilled in lockstep — exactly as `SignalService::refresh_prekeys`
        // already does for the replenishment path.
        //
        // Seeding these at registration is what gives each session its OWN PQ
        // key. Without them the server has nothing to pop and every PQXDH
        // handshake falls back to the single last-resort PQSPK above, which is
        // spec-permitted but reuses one PQ key across every session until
        // rotation (see the warning at crypto/signal/identity.rs:38-42).
        //
        // IDs start at 2: the last-resort PQSPK holds id 1. `generate_kyber_prekey`
        // persists each record to the key store, so `get_next_kyber_prekey_id`
        // advances as we go — the same loop shape as the actor's
        // RefreshKyberPrekeys handler.
        let mut kyber_one_time_prekeys = Vec::with_capacity(100);
        for _ in 0..100 {
            let id = self.prekey_manager.get_next_kyber_prekey_id()?;
            kyber_one_time_prekeys.push(
                self.prekey_manager
                    .generate_kyber_prekey(id, &identity_key_pair)?,
            );
        }

        // Store profile key locally (needed for profile encryption)
        // Stored in SQLCipher encrypted database
        self.settings_repo
            .set_bytes(keys::PROFILE_KEY, &profile_key)?;
        // Persist the chosen nickname.discriminator so discovery / lookup
        // flows can reproduce the Ristretto255 hash without prompting.
        self.settings_repo
            .set_string(keys::FULL_USERNAME, &full_username)?;

        // Store local-only PIN fingerprint so re-auth gates (e.g. linked-device
        // approval) can verify the PIN without a server round-trip. Derived
        // from the same Argon2id SVR secret computed above (literally the same
        // bytes, not a second identical derivation); never sent to the server.
        if pin.is_some() {
            let mut fp = crate::crypto::svr::pin_fingerprint_from_secret(&svr_secret);
            self.settings_repo.set_bytes(keys::PIN_FINGERPRINT, &fp)?;
            fp.zeroize();
        }

        // SECURITY: the PIN-derived secret is done being used; drop it from
        // memory rather than letting it live to the end of registration.
        svr_secret.zeroize();

        // Store avatar locally if provided (raw bytes, encrypted when shared)
        // Avatar is NOT uploaded to server - only shared E2E encrypted during bootstraps
        if let Some(avatar_bytes) = avatar {
            self.settings_repo.set_bytes(keys::AVATAR, avatar_bytes)?;
        }

        // Derive UAK from profile key (16 bytes) - this is what we send to server
        let uak = crate::crypto::device_credentials::derive_uak(&profile_key);

        // SECURITY: Encrypt display_name with profile key before sending
        // Server never sees plaintext profile data - only the encrypted blob
        // Uses AES-256-GCM with padding to 128 bytes (Signal spec)
        let encrypted_display_name =
            crate::crypto::profile_crypto::encrypt_display_name(&profile_key, display_name)?;

        // Device ID is 1 for primary device during registration
        let device_id = 1u32;
        let credentials = DeviceCredentials::generate(device_id, &profile_key);

        // SECURITY: Request registration challenge from server
        // This prevents spoofing identity keys during registration
        let identity_public_key_full = identity_key_pair.identity_key().serialize();
        // Skip the first byte (type prefix) to get the raw 32-byte Curve25519 key
        let identity_public_key = &identity_public_key_full[1..];
        tracing::info!("Requesting registration challenge");
        // OPAQUE registration rides the challenge round trip: the server
        // answers our RegistrationRequest under a fresh credential identifier
        // bound to this challenge, and Register uploads the record.
        let opaque_reg = opaque::registration_start(password)?;
        let challenge = auth_client
            .request_registration_challenge(
                &username_hash,
                identity_public_key,
                &opaque_reg.request,
            )
            .await
            .map_err(|e| {
                tracing::error!("Challenge request failed: {}", e);
                AuthError::RegistrationFailed(format!("challenge request failed: {}", e))
            })?;
        let opaque_record = opaque::registration_finish_async(
            opaque_reg,
            password,
            challenge.opaque_registration_response.clone(),
        )
        .await?;

        // SECURITY: prove possession of the identity key we're uploading. The
        // signature is over a domain-separated message and a wrong-length nonce
        // is refused, so the server cannot use this as a prekey signing oracle
        // (see IdentityKeyManager::sign_registration_challenge).
        let challenge_response = self
            .identity_manager
            .sign_registration_challenge(&challenge.challenge_nonce)
            .map_err(AuthError::Crypto)?;

        // Proof of work the server set for this transport (required over the
        // onion service and rising with its load, 0 on clearnet), over the
        // challenge token. Solved on the blocking pool. Given
        // up after 4 min: the challenge lives 5 min on the server, and
        // measuring from our own clock (not `expires_at`) keeps a skewed
        // phone clock out of it.
        let pow_challenge = challenge.challenge.clone().ok_or_else(|| {
            AuthError::RegistrationFailed("challenge carries no proof-of-work token".to_string())
        })?;
        let pow_started = std::time::Instant::now();
        let pow_solution = crate::crypto::pow::solve_async(
            crate::crypto::pow::Purpose::Register,
            pow_challenge.token.clone(),
            pow_challenge.difficulty,
            pow_challenge.gate_bits,
            pow_started + std::time::Duration::from_secs(240),
        )
        .await
        .map_err(AuthError::Crypto)?;
        tracing::info!(
            pow_bits = pow_challenge.difficulty,
            gate_bits = pow_challenge.gate_bits,
            solve_ms = pow_started.elapsed().as_millis() as u64,
            "registration proof of work solved"
        );

        // Build Signal Protocol prekey bundle for server
        use crate::proto::chat_v1::SignalPreKeyBundle;
        use crate::proto::chat_v1::signal_pre_key_bundle::{
            KyberPreKeyRecord as ProtoKyberPreKey, PreKeyRecord as ProtoPreKey,
            SignedPreKeyRecord as ProtoSignedPreKey,
        };
        use libsignal_protocol::GenericSignedPreKey;

        // Extract values from signed prekey (these are infallible for freshly generated keys)
        let signed_prekey_id: u32 = signed_prekey
            .id()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .into();
        let signed_prekey_public = signed_prekey
            .public_key()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .serialize();
        let signed_prekey_sig = signed_prekey
            .signature()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;
        // Chatter-owned wire field is nanoseconds; libsignal-native Timestamp
        // stores milliseconds. Convert via ×1_000_000 on the wire.
        let signed_prekey_ts = signed_prekey
            .timestamp()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .epoch_millis()
            * 1_000_000;

        // Extract Kyber prekey values
        let kyber_id: u32 = kyber_prekey
            .id()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .into();
        let kyber_public = kyber_prekey
            .public_key()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?
            .serialize();
        let kyber_sig = kyber_prekey
            .signature()
            .map_err(|e| CryptoError::Protocol(e.to_string()))?;

        let prekey_bundle = SignalPreKeyBundle {
            identity_key: identity_public_key_full.to_vec(),
            registration_id,
            device_id,
            signed_prekey: Some(ProtoSignedPreKey {
                id: signed_prekey_id,
                public_key: signed_prekey_public.to_vec(),
                signature: signed_prekey_sig,
                timestamp: signed_prekey_ts,
            }),
            one_time_prekey: if let Some(pk) = one_time_prekeys.first() {
                let pk_id: u32 = pk
                    .id()
                    .map_err(|e| CryptoError::Protocol(e.to_string()))?
                    .into();
                let pk_public = pk
                    .public_key()
                    .map_err(|e| CryptoError::Protocol(e.to_string()))?
                    .serialize();
                Some(ProtoPreKey {
                    id: pk_id,
                    public_key: pk_public.to_vec(),
                })
            } else {
                None
            },
            kyber_prekey: Some(ProtoKyberPreKey {
                id: kyber_id,
                public_key: kyber_public.to_vec(),
                signature: kyber_sig,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("Time should move forward")
                    .as_nanos() as u64,
            }),
            // Per NDSS '21: Do NOT enable unrestricted access by default.
            // Initial contact uses anonymous credentials (blind-signed tokens) instead.
            // This prevents spam while preserving sender anonymity.
            unrestricted_unidentified_access: Some(false),
        };

        // Build initial one-time prekeys for server (skip first, it's in bundle)
        let mut initial_prekeys: Vec<ProtoPreKey> = Vec::new();
        for pk in one_time_prekeys.iter().skip(1) {
            let pk_id: u32 = pk
                .id()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .into();
            let pk_public = pk
                .public_key()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .serialize();
            initial_prekeys.push(ProtoPreKey {
                id: pk_id,
                public_key: pk_public.to_vec(),
            });
        }

        // Build the PQOPK batch for the server. Unlike the classical OPK, every
        // PQ prekey carries an XEdDSA signature over its public key (PQXDH §3.2);
        // the server verifies it via ValidatePreKeySignatures and rejects the
        // batch outright if it does not check out. None are held back for the
        // bundle: the bundle's `kyber_prekey` field is the last-resort PQSPK, and
        // the server picks a PQOPK from this pool at handout time.
        let mut initial_kyber_prekeys: Vec<ProtoKyberPreKey> = Vec::new();
        for kpk in &kyber_one_time_prekeys {
            let id: u32 = kpk
                .id()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .into();
            let public = kpk
                .public_key()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .serialize();
            let sig = kpk
                .signature()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?;
            // Chatter-owned wire field is nanoseconds; libsignal stores millis.
            let ts = kpk
                .timestamp()
                .map_err(|e| CryptoError::Protocol(e.to_string()))?
                .epoch_millis()
                * 1_000_000;
            initial_kyber_prekeys.push(ProtoKyberPreKey {
                id,
                public_key: public.to_vec(),
                signature: sig,
                timestamp: ts,
            });
        }

        // Call server registration (wait for completion)
        // PRIVACY: Send username_hash, NOT plaintext username
        // SECURITY: Send UAK (16 bytes), NOT profile_key (32 bytes)
        // Profile key stays client-side for profile encryption
        // SECURITY: display_name is encrypted with profile key (server sees only ciphertext)
        // SECURITY: the challenge token and response prove identity key possession
        //
        // Signal-style discovery hash: Ristretto255 commitment over the
        // already-chosen `nickname.discriminator` (see `full_username` above),
        // plus the zero-knowledge proof the server verifies it against.
        let (username_lookup, username_proof) = username_claim(&full_username)
            .map_err(|e| AuthError::RegistrationFailed(format!("lookup hash failed: {e}")))?;
        let result = auth_client
            .register_and_wait(
                &username_hash, // PRIVACY: Send hash, not plaintext username
                &opaque_record, // OPAQUE RegistrationRecord: nothing password-equivalent
                &encrypted_display_name,
                &svr_public_key, // Derived from PIN
                device_info,
                credentials.auth_token().as_bytes(),
                &uak,
                Some(prekey_bundle),
                initial_prekeys,
                &pow_challenge.token,
                &challenge_response,
                pow_solution,
                &registration_lock, // SECURITY: Prevents account takeover without PIN
                &username_lookup,
                &username_proof,
            )
            .await;

        // SECURITY: Zeroize registration lock immediately after use (both success and error paths)
        registration_lock.zeroize();

        let result = result.map_err(|e| AuthError::RegistrationFailed(e.to_string()))?;

        // Extract user_id from registration result (server-assigned)
        let user_id: UserId = result
            .user_id
            .parse()
            .map_err(|_| AuthError::RegistrationFailed("invalid user_id format".to_string()))?;

        // Persist the one-time server response before the rest of registration
        // setup, so an interrupted setup can still display these same codes.
        let recovery_codes = if result.recovery_codes.is_empty() {
            None
        } else {
            self.settings_repo.set(
                &keys::unacknowledged_recovery_codes_key(user_id),
                &PendingRecoveryCodes {
                    codes: result.recovery_codes.clone(),
                    regenerated: false,
                },
            )?;
            Some(result.recovery_codes.clone())
        };

        // Persist auth state (device_id is server-assigned)
        self.settings_repo
            .set_string(keys::CURRENT_USER_ID, &user_id.to_string())?;
        self.settings_repo
            .set(keys::CURRENT_DEVICE_ID, &result.device_id)?;
        self.settings_repo
            .set_string(keys::DISPLAY_NAME, display_name)?;
        // Salt for later PIN-derived flows (server message-backup from settings).
        self.settings_repo
            .set_string(keys::SVR_SALT_USERNAME, username)?;

        self.settings_repo
            .set_string(keys::PUSH_TOPIC, &result.push_topic)?;
        self.settings_repo
            .set_string(keys::DEVICE_WAKE_TOPIC, &result.device_wake_topic)?;

        // SECURITY: Store auth token in encrypted settings
        self.settings_repo
            .set_bytes(keys::DEVICE_AUTH_TOKEN, credentials.auth_token().as_bytes())?;
        // This device created the password: it is trusted for its next login
        // (server OPEN-ITEMS §134). Encrypted settings, like the auth token.
        if !result.trusted_device_token.is_empty() {
            self.settings_repo.set_bytes(
                &keys::trusted_device_token_key(&username_hash),
                &result.trusted_device_token,
            )?;
        }

        // Store identity public key for NDSS'21 private friend codes
        // This allows generating friend codes without accessing the SignalStore
        // Store the full serialized key (33 bytes) as it's used for Signal Protocol
        self.settings_repo
            .set_bytes(keys::IDENTITY_KEY_PUBLIC, identity_public_key_full.as_ref())?;

        // Configure auth interceptor for Basic auth
        // Client already has device_auth_token and UAK (generated locally)
        self.auth_interceptor.set_basic(
            credentials.auth_username(&user_id.to_string()),
            credentials.auth_token_base64(),
        );

        // Seed the server's PQOPK pool. This reuses the existing RefreshPreKeys
        // RPC rather than adding a registration field, which also fixes the
        // ordering constraints for free:
        //   - signal_kyber_one_time_prekeys has an FK on
        //     signal_prekey_bundles(user_id, device_id), so the bundle must
        //     already be stored — i.e. after register_and_wait.
        //   - RefreshPreKeys is authenticated, so this must follow set_basic.
        // The classical OPKs ride along inside RegisterRequest and are already
        // uploaded by this point; only the PQ pool is sent here.
        //
        // A failure here is logged, not fatal: the account is already registered
        // server-side, and PQXDH still succeeds against the last-resort PQSPK
        // (spec-permitted). Failing registration outright would be a worse
        // outcome than the degradation it avoids. The background replenisher
        // (app/core_services.rs) refills both pools together on its next pass.
        if !initial_kyber_prekeys.is_empty() {
            let pq_count = initial_kyber_prekeys.len();
            match auth_client
                .refresh_pre_keys_with_kyber_otpk(Vec::new(), initial_kyber_prekeys)
                .await
            {
                Ok(()) => {
                    tracing::info!(pq_opk = pq_count, "Seeded PQOPK pool at registration");
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "Failed to seed PQOPK pool at registration — PQXDH will fall back to \
                         the last-resort PQSPK until the replenisher runs"
                    );
                }
            }
        }

        // Store user locally
        let user = User {
            id: user_id,
            username: username.to_string(),
            svr_public_key: Vec::new(), // SVR key will be set during PIN setup
            last_active: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
        };
        self.user_repo.insert(&user)?;

        // PRIVACY: For registration, use the provided plaintext display_name.
        // Server returns encrypted version which we don't need to decrypt since we have the original.
        let server_display_name = display_name.to_string();

        let profile = Profile {
            user_id,
            display_name: server_display_name.clone(),
            avatar_bytes: None,
            status_message: None,
            presence: PresenceStatus::Online,
        };
        self.user_repo.upsert_profile(&profile)?;

        Ok(LoginResult {
            user_id,
            device_id: result.device_id,
            display_name: server_display_name,
            needs_prekey_upload: false, // Prekeys already generated (upload them next)
            keys_restored: false, // Not applicable for registration (keys are freshly generated)
            key_recovery: None,
            recovery_codes,
        })
    }

    /// Clean up partial state after a failed registration attempt.
    ///
    /// This removes any locally stored data that was created before the
    /// server registration failed (e.g., profile key, identity keys, prekeys).
    pub async fn cleanup_failed_registration(&self) -> Result<(), AuthError> {
        tracing::debug!("Cleaning up after failed registration");

        // Clear all crypto state (identity keys, prekeys, sessions)
        // This is the most important cleanup - prevents "UNIQUE constraint failed"
        if let Err(e) = self.key_store.clear_all_crypto_state() {
            tracing::warn!("Failed to clear crypto state: {}", e);
        }

        // Remove profile key if it was stored
        let _ = self.settings_repo.delete(keys::PROFILE_KEY);

        // Remove any partial auth state
        let _ = self.settings_repo.delete(keys::CURRENT_USER_ID);
        let _ = self.settings_repo.delete(keys::CURRENT_DEVICE_ID);
        let _ = self.settings_repo.delete(keys::DEVICE_AUTH_TOKEN);
        let _ = self.settings_repo.delete(keys::PUSH_TOPIC);
        let _ = self.settings_repo.delete(keys::DEVICE_WAKE_TOPIC);

        // Remove sender certificate if any
        let _ = self.settings_repo.delete(keys::SENDER_CERTIFICATE);
        let _ = self
            .settings_repo
            .delete(keys::SENDER_CERTIFICATE_REFRESH_AFTER);
        let _ = self
            .settings_repo
            .delete(keys::SENDER_CERTIFICATE_SERVER_KEY_ID);

        // Clear auth interceptor
        self.auth_interceptor.clear();

        Ok(())
    }
}
