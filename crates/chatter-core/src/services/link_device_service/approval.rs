use super::*;

impl LinkDeviceService {
    /// Parse a `chatter://link?uuid=…&pub_key=…&server=…&name=…` URL.
    ///
    /// `pub(crate)` — external callers use [`Self::prepare_link_from_url`]
    /// which wraps this in a [`PendingDeviceLink`] gated by `approve_link`.
    pub(crate) fn parse_link(url: &str) -> Result<LinkRequest, LinkDeviceError> {
        let parsed = Url::parse(url).map_err(|e| LinkDeviceError::InvalidLinkUrl(e.to_string()))?;
        if parsed.scheme() != LINK_URL_SCHEME {
            return Err(LinkDeviceError::InvalidLinkUrl(format!(
                "expected scheme {LINK_URL_SCHEME}, got {}",
                parsed.scheme()
            )));
        }
        if parsed.host_str() != Some(LINK_URL_HOST) {
            return Err(LinkDeviceError::InvalidLinkUrl(format!(
                "expected host {LINK_URL_HOST}, got {:?}",
                parsed.host_str()
            )));
        }

        let mut uuid = None;
        let mut pub_key_b64 = None;
        let mut server_url = None;
        let mut device_name = None;
        for (k, v) in parsed.query_pairs() {
            match k.as_ref() {
                "uuid" => uuid = Some(v.to_string()),
                "pub_key" => pub_key_b64 = Some(v.to_string()),
                "server" => server_url = Some(v.to_string()),
                "name" => device_name = Some(v.to_string()),
                _ => {}
            }
        }

        let uuid = uuid.ok_or_else(|| LinkDeviceError::InvalidLinkUrl("missing uuid".into()))?;
        let pub_key_b64 =
            pub_key_b64.ok_or_else(|| LinkDeviceError::InvalidLinkUrl("missing pub_key".into()))?;
        let pub_key_bytes = base64_url_decode(&pub_key_b64)
            .map_err(|e| LinkDeviceError::InvalidLinkUrl(format!("pub_key decode: {e}")))?;
        if pub_key_bytes.len() != 32 {
            return Err(LinkDeviceError::InvalidLinkUrl(format!(
                "pub_key must be 32 bytes, got {}",
                pub_key_bytes.len()
            )));
        }
        let mut peer_pub = [0u8; 32];
        peer_pub.copy_from_slice(&pub_key_bytes);

        Ok(LinkRequest {
            uuid,
            peer_pub,
            server_url,
            device_name,
        })
    }

    /// Build + encrypt + upload the provision envelope for a parsed link.
    ///
    /// **`pub(crate)` only** — the only public path to this code is via
    /// [`Self::approve_link`], which requires a [`PendingDeviceLink`]
    /// (constructed only by [`Self::prepare_link_from_url`]) and a
    /// re-auth check on a [`SecretString`]. Direct external invocation
    /// would skip the user-confirmation gate.
    ///
    /// Builds a [`ProvisionMessage`] from the local identity, profile key,
    /// and registration ID; encrypts it for the new device's pubkey;
    /// uploads via `provision_device`. The new device picks it up via its
    /// poll loop.
    pub(crate) async fn install_envelope(&self, req: &LinkRequest) -> Result<(), LinkDeviceError> {
        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)?
            .ok_or(LinkDeviceError::NotLoggedIn)?;

        let identity_manager = IdentityKeyManager::new(self.key_store.clone());
        let (registration_id, identity_pair) = identity_manager
            .get_identity()?
            .ok_or(CryptoError::NoIdentity)?;

        let priv_bytes = identity_pair.private_key().serialize().to_vec();

        let profile_key = self
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)?
            .ok_or_else(|| LinkDeviceError::Invalid("profile key missing".into()))?;
        if profile_key.len() != 32 {
            return Err(LinkDeviceError::Invalid("profile key wrong length".into()));
        }

        // Server expects this code echoed back from new device in
        // CompleteDeviceLinking. Today server validation of the code is
        // limited (no out-of-band channel between server and primary), so
        // we treat it as a freshness marker and put a random value here.
        let provisioning_code = random_provisioning_code();

        // What the new device needs for PIN-derived flows and to show our
        // handle. Both are set on every device that registered, logged in or
        // was itself linked.
        let required = |key: &str| -> Result<String, LinkDeviceError> {
            self.settings_repo
                .get_string(key)?
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    LinkDeviceError::Invalid(format!(
                        "{key} missing — log out and in once, then link"
                    ))
                })
        };
        let svr_salt_username = required(keys::SVR_SALT_USERNAME)?;
        let full_username = required(keys::FULL_USERNAME)?;

        // Mint the single-use Link'n'Sync archive key (§30). It rides ONLY
        // inside the E2E envelope; the server never sees it. The new device
        // derives the same KEK from it and decrypts the transfer archive we
        // export below. Keep the ACI for that same derivation before the
        // struct build moves `user_id_str`.
        let aci = user_id_str.clone();
        let mut ephemeral_backup_key = Zeroizing::new([0u8; 32]);
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(ephemeral_backup_key.as_mut())
                .expect("OS CSPRNG must not fail");
        }

        let settings = crate::services::configuration_sync::current(&self.settings_repo);
        let message = ProvisionMessage {
            aci_identity_key_private: priv_bytes,
            pni_identity_key_private: vec![],
            aci: user_id_str,
            pni: String::new(),
            number: String::new(),
            provisioning_code,
            profile_key,
            master_key: vec![],
            // The privacy settings as they stand here, which the new device
            // adopts (`complete_linking`), as a ConfigurationSync would.
            read_receipts: settings.read_receipts,
            typing_indicators: settings.typing_indicators,
            // Chatter has no link-preview setting (see `configuration_sync`):
            // left at the proto3 default, which is not encoded, and never read.
            provisioning_user_agent: format!("chatter-linux/{}", env!("CARGO_PKG_VERSION"),),
            primary_device_registration_id: registration_id,
            ephemeral_backup_key: Some(ephemeral_backup_key.to_vec()),
            svr_salt_username,
            full_username,
        };

        let envelope: ProvisionEnvelope = encrypt_envelope(&req.peer_pub, &message)?;

        let mut client = self.provisioning_client()?;
        // Send the same provisioning_code in cleartext alongside the envelope
        // — server stores it for `complete_device_linking` to validate when
        // the new device echoes it back from the decrypted envelope.
        client
            .provision_device(&req.uuid, envelope, &message.provisioning_code)
            .await
            .map_err(|e| match e {
                // The new device's session is gone (Redis TTL) or closed: the
                // QR is stale, not a network fault.
                NetworkError::NotFound(m) => LinkDeviceError::SessionEnded(m),
                NetworkError::InvalidRequest(m) if m.contains("provisioning session") => {
                    LinkDeviceError::SessionEnded(m)
                }
                e => e.into(),
            })?;

        // Keys are on their way. Now export this account's message history +
        // social graph as an encrypted archive keyed to the ephemeral key the
        // new device just received, and hand the server a blob pointer via
        // SetTransferArchive. Best-effort: a failure here signals the new
        // device to link WITHOUT history rather than hang — linking already
        // succeeded above.
        self.export_transfer_archive(&req.uuid, &ephemeral_backup_key, &aci, registration_id)
            .await;
        Ok(())
    }
    pub async fn cancel(&self, uuid: &str) -> Result<(), LinkDeviceError> {
        let mut client = self.provisioning_client()?;
        client.cancel_provisioning(uuid).await?;
        Ok(())
    }

    /// Links this account approved that the new device has not completed yet
    /// (see [`pending_links`]).
    pub async fn list_pending(&self) -> Result<Vec<PendingLink>, LinkDeviceError> {
        let mut client = self.provisioning_client()?;
        let response = client.list_provisioning_sessions().await?;
        Ok(pending_links(
            response.sessions,
            crate::util::time::TimestampNanos::now().as_u64(),
        ))
    }

    /// Step 1 of the primary-side device-link flow. Pure parse + preview —
    /// **no network call, no key install, no session state mutation**.
    ///
    /// Returns a [`PendingDeviceLink`] holding the parsed payload plus a
    /// short verification code derived from the new device's ephemeral
    /// public key.
    ///
    /// # Contract — the caller MUST:
    ///
    /// 1. Display [`PendingDeviceLink::verification_code`] (and ideally
    ///    the device name / server URL) to the user.
    /// 2. Obtain explicit confirmation that the code matches the value
    ///    shown on the new device.
    /// 3. **Only then** pass the value to [`Self::approve_link`] together
    ///    with the user's password / PIN.
    ///
    /// The returned type is `!Clone` and is consumed (`self`) by
    /// `approve_link`, so it cannot be silently re-used or fanned out.
    /// This is the in-SDK enforcement of the user-approval gate — a UI
    /// bug or alt-UI cannot complete a link by calling some other
    /// "install" entrypoint without going through this function.
    pub fn prepare_link_from_url(&self, url: &str) -> Result<PendingDeviceLink, LinkDeviceError> {
        let req = Self::parse_link(url)?;
        let verification_code = verification_code_for_pub(&req.peer_pub);
        Ok(PendingDeviceLink {
            inner: req,
            verification_code,
        })
    }

    /// Step 2 of the primary-side device-link flow. Consumes a
    /// [`PendingDeviceLink`] (so it cannot be replayed), re-authenticates
    /// the user against local material, then encrypts and uploads the
    /// provision envelope.
    ///
    /// # Contract
    ///
    /// **Caller MUST display the verification code from
    /// [`PendingDeviceLink`] to the user and obtain confirmation before
    /// calling `approve_link`.** The SDK has no way to verify the user
    /// actually saw and approved the code — this is the trust boundary.
    /// In return, the SDK guarantees that no install / session-storage
    /// path is reachable without first owning a `PendingDeviceLink`
    /// (see [`Self::install_envelope`] which is `pub(crate)`).
    ///
    /// `secret` accepts either the user's password or a PIN
    /// (auto-routed by [`Self::verify_password_or_pin`]). The
    /// [`SecretString`] wrapper zeroizes its contents on drop.
    ///
    /// On success, emits [`DomainEvent::DeviceLinked`] with
    /// `approved_by_self = true` so any open Devices modal / toast
    /// banner can react.
    pub async fn approve_link(
        &self,
        pending: PendingDeviceLink,
        secret: SecretString,
    ) -> Result<ApprovedDevice, LinkDeviceError> {
        let req = pending.inner;
        if !self.verify_password_or_pin(secret.expose_secret()).await? {
            return Err(LinkDeviceError::ReauthFailed);
        }
        self.install_envelope(&req).await?;

        // Server doesn't return the new device_id at provision_device time
        // (that's known only after the new device calls complete_device_linking).
        // Use 0 as a placeholder; the Devices modal will refresh from
        // ListDevices on the event handler.
        self.event_bus.emit_domain(DomainEvent::DeviceLinked {
            device_id: 0,
            device_name: req.device_name.clone(),
            device_info: None,
            approved_by_self: true,
        });

        // No OS toast here: the approve screen confirms it, and the server's
        // DeviceLinked fan-out reaches this device too — a second toast here
        // posted two notifications for one link.

        Ok(ApprovedDevice {
            uuid: req.uuid,
            device_name: req.device_name,
            server_url: req.server_url,
        })
    }
}
