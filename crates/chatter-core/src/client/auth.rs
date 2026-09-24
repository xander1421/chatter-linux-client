//! Authentication high-level API (login, registration, recovery, PINs).

#![allow(unused_imports)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::app::context::{AppContext, ContextError};
use crate::app::core_services::CoreServices;
use crate::domain::attachment::Attachment;
use crate::domain::channel::{Channel, ChannelType};
use crate::domain::message::{ConversationType, Message};
use crate::error::NetworkError;
use crate::events::{DomainEvent, EventBus};
use crate::services::auth_service::{AuthError, LoginResult};
use crate::services::key_recovery::KeyRecoveryNeed;
use crate::storage::repositories::settings_repo::keys;
use crate::inbox::DeviceInboxProvider;
use crate::mailbox::group::GroupFanout;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::auth_service::AuthService;
#[cfg(feature = "media")]
use crate::services::call_worker::CallWorkerHandle;
use crate::services::channel::ChannelService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::gateway::outgoing::{
    OwnedOutgoingServices, OutgoingIntent, QuoteIntent, SendError, SendResult,
    route_outgoing_owned,
};
use crate::services::ai::LocalAIService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::pin_service::PinService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal_service::SignalService;
use crate::services::section_service::SectionService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, MessageState, OfflineState,
    PrivacyState, UserState,
};
use crate::state::chat_state::PendingAttachment;
use crate::storage::settings_keys;
use crate::system::Directories;
use super::{ChatterClient, ClientError, RecoveryResult};

impl ChatterClient {
    /// Ensure the gRPC pool is connected before running an auth RPC.
    ///
    /// Called from [`Self::login`] and [`Self::register`]. The startup
    /// `connect()` may have run before the user supplied a server URL
    /// (e.g. first-run, where the URL is entered in the register form
    /// itself), leaving the connection state at `Disconnected`. Without
    /// this self-heal, every auth RPC bounces with "Not connected to
    /// server" until the user manually retries `connect()`.
    async fn ensure_connected_for_auth(self: &Arc<Self>) -> Result<(), ClientError> {
        if self.is_connected() {
            return Ok(());
        }
        self.connect().await
    }

    // ── Auth (high-level) ───────────────────────────────────────────

    /// Log in and start all background services.
    ///
    /// This is the one-call entry point: authenticates, sets auth state,
    /// and starts inbox polling + event handlers.
    ///
    /// On a device with no local identity keys (a reinstall), the SVR restore
    /// runs BEFORE any identity key is minted, so the user recovers their real
    /// key instead of silently being given a fresh, unreachable one. If the
    /// restore cannot establish an identity — no PIN, wrong PIN, unreachable
    /// server, or no backup at all — this returns
    /// [`ClientError::IdentityRecoveryRequired`] carrying the reason, and the
    /// user must either retry or explicitly call
    /// [`Self::accept_new_identity`].
    pub async fn login(
        self: &Arc<Self>,
        username: &str,
        password: &str,
        device_name: &str,
        pin: Option<&str>,
    ) -> Result<LoginResult, ClientError> {
        // If startup `connect()` failed (or hasn't run yet), the gRPC
        // pool will reject auth RPCs with "Not connected to server".
        // Self-heal here so callers don't have to remember to retry —
        // common UX hazard when the user fixes the server URL after
        // initial offline boot.
        self.ensure_connected_for_auth().await?;
        let mut result = self.auth().do_login(username, password, device_name, pin).await?;
        self.auth_state().set_logged_in(
            result.user_id,
            result.device_id,
            result.display_name.clone(),
        );

        // ORDER IS THE FIX. `do_login` no longer mints an identity key, so a
        // reinstall arrives here with none and the SVR restore below actually
        // runs. It used to be the other way round: the mint happened first,
        // this restore saw a local identity, skipped, and the user's real key
        // was destroyed by the act of logging in to recover it.
        if result.key_recovery.is_some() {
            match pin {
                Some(p) => self.attempt_svr_restore(p, username, &mut result).await,
                // Nothing to attempt. Left as NoPinProvided so the caller is
                // told to ask for the PIN rather than being handed a working
                // -looking session with no keys.
                None => tracing::warn!("no local identity and no PIN supplied — cannot restore keys"),
            }
        }

        // Refuse to report success while this device has no identity key.
        //
        // The previous design returned Ok with `keys_missing = true`, and every
        // caller in the tree — including the only UI login handler — discarded
        // the result. That is what made total, permanent key loss look like a
        // successful login. An Err cannot be discarded.
        if let Some(need) = result.key_recovery {
            return Err(ClientError::IdentityRecoveryRequired(need));
        }

        // A device that just obtained its identity from a backup holds none of
        // the prekey private keys the account previously published — those died
        // with the old device. Replace the account's key material with this
        // device's, or peers keep fetching prekeys that can never complete a
        // handshake.
        // Also retried when a previous attempt was interrupted after the local
        // keys landed: without that, the next login sees a local identity,
        // skips the restore, and the account is left serving prekeys nobody
        // holds the private half of.
        if result.keys_restored || self.key_republish_pending() {
            self.republish_identity_keys().await?;
        } else {
            // The server issued this login a device id it holds no bundle for
            // (every login gets a new one). Without this, a Log Out -> Log In
            // left peers encrypting to the previous, signed-out device and
            // every incoming DM was delivered to nobody. Failing the login is
            // better than reporting success for an unreachable device.
            let mut auth_client = self
                .auth()
                .try_auth_client()
                .ok_or(ClientError::NotConnected)?;
            self.signal()
                .publish_device_bundle(&mut auth_client)
                .await
                .map_err(|e| ClientError::Validation(format!("failed to publish this device's keys: {e}")))?;
        }

        // Our name, bio and avatar live on the server encrypted with our
        // profile key. A new device has none of them locally — the avatar
        // showed as initials while friends saw the photo — so restore them now
        // that the profile key is in place. Best-effort: a failed fetch must
        // not fail a login that otherwise worked.
        match self.users().get_user_profile(&result.user_id).await {
            Ok(profile) => {
                result.display_name = profile.display_name;
                self.auth_state().set_logged_in(
                    result.user_id,
                    result.device_id,
                    result.display_name.clone(),
                );
            }
            Err(e) => tracing::warn!(error = %e, "login: own profile restore failed"),
        }

        self.start().await?;
        Ok(result)
    }

    /// Accept a NEW Signal identity key after key recovery has been ruled out.
    ///
    /// **Destructive and irreversible.** It abandons every existing session,
    /// makes past messages undecryptable, and changes the safety number every
    /// contact has verified. Only ever reach this from an explicit user
    /// decision, never from a retry loop or a background task — which is why
    /// `login` returns an error rather than doing this for the user.
    ///
    /// Refused while [`KeyRecoveryNeed::is_retryable`] holds: a wrong PIN, an
    /// unreachable SVR server or a PIN that was never entered all still lead
    /// back to the real key, and minting over it is exactly the silent
    /// destruction this whole change exists to stop.
    ///
    /// On success the new key is published atomically with prekeys signed by
    /// it, so peers see one coherent bundle and get the safety-number-change
    /// prompt (`pending_identity_changes`) instead of an account that quietly
    /// stopped working.
    pub async fn accept_new_identity(
        self: &Arc<Self>,
        need: KeyRecoveryNeed,
    ) -> Result<(), ClientError> {
        if need.is_retryable() {
            return Err(ClientError::Validation(format!(
                "refusing to mint a new identity while the original may still be recoverable: {}",
                need.user_message()
            )));
        }

        if self
            .signal()
            .key_store
            .has_local_identity()
            .map_err(|e| ClientError::Validation(format!("failed to read local identity: {e}")))?
        {
            return Err(ClientError::Validation(
                "this device already has an identity key; refusing to overwrite it".into(),
            ));
        }

        self.auth()
            .generate_identity_for_recovery()
            .map_err(ClientError::Auth)?;

        self.republish_identity_keys().await?;

        tracing::warn!("minted and published a NEW identity key — every contact's safety number has changed");
        Ok(())
    }

    /// Replace the account's server-side key material with this device's.
    ///
    /// One RPC, applied atomically by the server: identity key, signed prekey,
    /// Kyber prekey and both one-time pools land together, and every other
    /// device's stale bundle is dropped. Publishing them separately is what
    /// produced the original corruption — an identity key from one generation
    /// served next to a signed prekey from another.
    async fn republish_identity_keys(self: &Arc<Self>) -> Result<(), ClientError> {
        let mut auth_client = self
            .auth()
            .try_auth_client()
            .ok_or(ClientError::NotConnected)?;

        // Mark the debt BEFORE incurring it. If this call dies halfway, the
        // next login retries; the alternative is a device whose local keys are
        // fine while the server serves prekeys for a device that is gone.
        self.set_key_republish_pending(true);

        let outcome = self
            .signal()
            .republish_identity_bundle(&mut auth_client)
            .await
            .map_err(|e| ClientError::Validation(format!("failed to publish key material: {e}")))?;

        self.set_key_republish_pending(false);

        tracing::info!(
            identity_key_changed = outcome.identity_key_changed,
            devices_reset = outcome.devices_reset,
            "published this device's key material"
        );
        Ok(())
    }

    /// Whether a previous republish was interrupted after the local keys landed.
    fn key_republish_pending(&self) -> bool {
        self.signal()
            .settings_repo
            .get::<bool>(keys::KEY_REPUBLISH_PENDING)
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    fn set_key_republish_pending(&self, pending: bool) {
        if let Err(e) = self
            .signal()
            .settings_repo
            .set(keys::KEY_REPUBLISH_PENDING, &pending)
        {
            // Not fatal, but worth saying: losing this flag is how an
            // interrupted republish becomes a permanent one.
            tracing::warn!("failed to record pending key republish: {}", e);
        }
    }

    /// Register a new account and start all background services.
    ///
    /// PIN must satisfy [`crypto::svr::validate_pin`] if provided
    /// (8-64 printable ASCII chars).
    pub async fn register(
        self: &Arc<Self>,
        username: &str,
        password: &str,
        display_name: &str,
        device_name: &str,
        avatar: Option<&[u8]>,
        pin: Option<&str>,
    ) -> Result<LoginResult, ClientError> {
        if let Some(p) = pin {
            crate::crypto::svr::validate_pin(p)
                .map_err(|e| ClientError::Validation(e.to_string()))?;
        }
        // Self-heal a stale Disconnected state (see `login` for context).
        self.ensure_connected_for_auth().await?;
        let result = self.auth().do_register(
            username, password, display_name, device_name, avatar, pin,
        ).await?;
        self.auth_state().set_logged_in(
            result.user_id,
            result.device_id,
            result.display_name.clone(),
        );

        // Create SVR backup after successful registration (non-blocking on failure)
        if let Some(p) = pin {
            self.perform_svr_backup(p, username).await;
        }

        self.start().await?;

        // Push the encrypted avatar to the server profile so friends can fetch
        // + decrypt it via the profile-key handshake (same path display_name
        // already uses). `do_register` stored the raw bytes in `keys::AVATAR`;
        // `update_user_profile` reads them, encrypts with the profile key, and
        // uploads as `encrypted_avatar`. Best-effort — a failure here must not
        // fail registration.
        if avatar.is_some_and(|a| !a.is_empty()) {
            if let Err(e) = self.users().update_user_profile(None, None).await {
                tracing::warn!(error = %e, "Registration: encrypted-avatar upload failed");
            }
        }

        Ok(result)
    }

    /// Use a one-time recovery code to reset password and SVR credentials.
    ///
    /// This is called when the user has lost their password but has a recovery code
    /// from registration. It resets the password and SVR key material on the server.
    ///
    /// **The user's SVR key backup does not survive this.** The backup is
    /// encrypted with, and indexed by, a secret derived from the PIN alone
    /// (`Argon2id(PIN, username)`), so setting a new PIN leaves the stored row
    /// unreadable by everyone. The server drops it (auth `UseRecoveryCode`
    /// step 9b) rather than orphaning it, and this call clears the matching
    /// local version counter so the next backup can be written at version 1.
    ///
    /// If this account's identity keys existed only in that backup, they are
    /// gone: [`RecoveryResult::key_backup_invalidated`] says so, and the caller
    /// must tell the user rather than discarding it.
    ///
    /// (This used to claim "the SVR backup will be created automatically
    /// during that login flow". It is not: `login` only ever calls
    /// `attempt_svr_restore`, and the sole caller of `perform_svr_backup` is
    /// `register`.)
    ///
    /// # Arguments
    ///
    /// * `username` - The plaintext username (will be hashed before sending)
    /// * `recovery_code` - 12-character alphanumeric recovery code (A-Z, 2-9)
    /// * `new_password` - The new password to set
    /// * `new_pin` - The new PIN (8+ chars) for SVR key derivation
    pub async fn use_recovery_code(
        &self,
        username: &str,
        recovery_code: &str,
        new_password: &str,
        new_pin: &str,
    ) -> Result<RecoveryResult, ClientError> {
        let mut auth_client = self.auth().try_auth_client()
            .ok_or_else(|| ClientError::NotConnected)?;
        let remaining_codes = self
            .auth()
            .use_recovery_code(&mut auth_client, username, recovery_code, new_password, new_pin)
            .await
            .map_err(ClientError::Auth)?;

        // The server just deleted the backup that was keyed to the OLD PIN, so
        // its stored version is back to 0 and `StoreBackup` will only accept 1.
        // A stale local counter would post N+1 and be rejected forever — the
        // deterministic ErrVersionConflict that this whole path used to end in.
        let settings = std::sync::Arc::new(
            crate::storage::repositories::settings_repo::SettingsRepository::new(
                self.services.database.clone(),
            ),
        );
        if let Err(e) =
            crate::services::svr_backup_service::SvrBackupService::clear_local_backup_version(
                &settings,
            )
        {
            // Not fatal to the recovery itself — the password IS reset — but it
            // must not be silent: every later backup on this device fails.
            tracing::error!(
                error = %e,
                "Recovery succeeded but the local SVR version counter could not be \
                 cleared; backups from this device will be rejected until it is"
            );
        }

        Ok(RecoveryResult {
            remaining_codes,
            key_backup_invalidated: true,
        })
    }

    /// Regenerate recovery codes for the authenticated user.
    ///
    /// Deletes all existing codes and generates 8 new ones.
    /// Requires password confirmation for security.
    ///
    /// # Arguments
    ///
    /// * `password` - The user's current password (will be hashed before sending)
    pub async fn regenerate_recovery_codes(&self, password: &str) -> Result<Vec<String>, ClientError> {
        let mut auth_client = self.auth().try_auth_client()
            .ok_or_else(|| ClientError::NotConnected)?;
        self.auth()
            .regenerate_recovery_codes(&mut auth_client, password)
            .await
            .map_err(ClientError::Auth)
    }

    /// Create an SVR encrypted backup after registration.
    ///
    /// Orchestration lives in `SvrBackupService::backup_from_pin`; this is a
    /// non-failing wrapper that just logs outcome — registration success must
    /// not block on backup upload.
    async fn perform_svr_backup(&self, pin: &str, username: &str) {
        match self.svr_backup().backup_from_pin(pin, username).await {
            Ok(version) => tracing::info!(version, "SVR backup created successfully after registration"),
            Err(e) => tracing::warn!("SVR backup failed (non-fatal): {}", e),
        }
    }

    /// The username every PIN derivation is salted with: exactly what was
    /// typed at register/login (`keys::SVR_SALT_USERNAME`). Not the display
    /// name and not the `.discriminator` handle — a different salt derives a
    /// different secret, and a backup written under it cannot be restored.
    fn svr_salt_username(&self) -> Result<String, ClientError> {
        self.signal()
            .settings_repo
            .get_string(keys::SVR_SALT_USERNAME)
            .map_err(|e| ClientError::Validation(format!("failed to read login username: {e}")))?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ClientError::Validation("no stored login username — log out and log in once, then retry".into())
            })
    }

    /// Whether the server holds a key backup for this account.
    pub async fn has_key_backup(&self) -> Result<bool, ClientError> {
        self.svr_backup()
            .has_key_backup()
            .await
            .map_err(|e| ClientError::Validation(format!("backup status check failed: {e}")))
    }

    /// Create the SVR key backup if registration failed to upload it.
    ///
    /// Gated on the local PIN fingerprint, and refuses when a backup already
    /// exists: the upload is an unconditional replace, so without both checks
    /// anyone holding the unlocked device could re-key the backup to a PIN of
    /// their choosing.
    pub async fn retry_svr_backup(&self, pin: &str) -> Result<(), ClientError> {
        if !self.link_device().verify_pin(pin).unwrap_or(false) {
            return Err(ClientError::Validation("Incorrect PIN".into()));
        }
        if self.has_key_backup().await? {
            return Ok(());
        }
        let username = self.svr_salt_username()?;
        self.svr_backup()
            .retry_from_credentials(pin, &username)
            .await
            .map_err(|e| ClientError::Validation(format!("SVR backup upload failed: {}", e)))?;
        tracing::info!("SVR backup retry succeeded");
        Ok(())
    }

    /// Change the user's SVR PIN.
    ///
    /// Re-encrypts the identity key backup with a new PIN-derived secret and
    /// atomically updates the server-side backup, registration lock, and SVR
    /// public key. Both PINs must satisfy [`crypto::svr::validate_pin`].
    ///
    /// All sensitive intermediates (secrets, keys) are zeroized on every path.
    ///
    /// # Arguments
    ///
    /// * `old_pin` - Current PIN.
    /// * `new_pin` - New PIN (must differ from `old_pin`).
    /// * `username` - Plaintext username (salt for Argon2id derivation).
    pub async fn change_pin(&self, old_pin: &str, new_pin: &str) -> Result<(), ClientError> {
        use crate::crypto::svr::{
            derive_registration_lock, derive_svr_secret, validate_pin, validate_pin_shape, SvrKeypair,
        };
        use zeroize::Zeroize;

        // Old PIN is the EXISTING one being verified — shape-only, or a user
        // whose current PIN is now blocklisted could never change it (§10).
        // The NEW PIN is being chosen, so it gets full strength validation.
        validate_pin_shape(old_pin).map_err(|e| ClientError::Validation(format!("old PIN: {e}")))?;
        validate_pin(new_pin).map_err(|e| ClientError::Validation(format!("new PIN: {e}")))?;
        if old_pin == new_pin {
            return Err(ClientError::Validation(
                "New PIN must be different from current PIN".into(),
            ));
        }

        let username = self.svr_salt_username()?;
        let username = username.as_str();
        // The handle the re-encrypted backup carries, so a restore after this
        // PIN change still recovers it (§117).
        let full_username = self
            .svr_backup()
            .local_full_username()
            .map_err(|e| ClientError::Validation(e.to_string()))?;

        // 2. Get identity keypair from signal store (private 32 bytes + public 33 bytes)
        let mut private_bytes = self.signal().actor.get_identity_private_key().await
            .map_err(|e| ClientError::Validation(format!("failed to get identity private key: {}", e)))?;
        let public_bytes_33 = self.signal().actor.get_identity_public_key().await
            .map_err(|e| {
                private_bytes.zeroize();
                ClientError::Validation(format!("failed to get identity public key: {}", e))
            })?;
        // Strip the 0x05 type prefix to get raw 32-byte Curve25519 public key
        let mut identity_keypair = [0u8; 64];
        identity_keypair[..32].copy_from_slice(&private_bytes);
        identity_keypair[32..].copy_from_slice(&public_bytes_33[1..]);
        private_bytes.zeroize();

        // 3. Get profile key from settings
        let mut profile_key = match self.signal().settings_repo.get_bytes(keys::PROFILE_KEY) {
            Ok(Some(bytes)) if bytes.len() == 32 => {
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&bytes);
                pk
            }
            Ok(_) => {
                identity_keypair.zeroize();
                return Err(ClientError::Validation("profile key not found or invalid length".into()));
            }
            Err(e) => {
                identity_keypair.zeroize();
                return Err(ClientError::Validation(format!("failed to read profile key: {}", e)));
            }
        };

        // 4. Derive old and new SVR secrets
        let mut old_svr_secret = derive_svr_secret(old_pin, username).map_err(|e| {
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive old svr_secret: {}", e))
        })?;
        let mut new_svr_secret = derive_svr_secret(new_pin, username).map_err(|e| {
            old_svr_secret.zeroize();
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive new svr_secret: {}", e))
        })?;

        // 5. Derive new registration lock
        let mut new_reg_lock = derive_registration_lock(new_pin, username).map_err(|e| {
            old_svr_secret.zeroize();
            new_svr_secret.zeroize();
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive registration lock: {}", e))
        })?;

        // 6. Derive new SVR public key
        let svr_keypair = SvrKeypair::new_from_pin(new_pin, username).map_err(|e| {
            old_svr_secret.zeroize();
            new_svr_secret.zeroize();
            new_reg_lock.zeroize();
            identity_keypair.zeroize();
            profile_key.zeroize();
            ClientError::Validation(format!("failed to derive SVR keypair: {}", e))
        })?;
        let new_svr_public_key = svr_keypair.public_key_bytes();
        // svr_keypair is dropped here (zeroizes secret via Drop impl)

        // 7. Call service to re-encrypt and upload
        //    If no backup exists yet, fall back to creating a new one with the new PIN.
        let result = self
            .svr_backup()
            .change_pin_backup(
                &old_svr_secret,
                &new_svr_secret,
                &identity_keypair,
                &profile_key,
                &full_username,
                &new_reg_lock,
                &new_svr_public_key,
            )
            .await;

        let final_result = match &result {
            Err(crate::services::svr_backup_service::SvrBackupError::BackupNotFound)
            | Err(crate::services::svr_backup_service::SvrBackupError::Network(
                crate::error::NetworkError::NotFound(_),
            )) => {
                // The server answers a WRONG old PIN with the same NotFound it
                // uses for "no backup" (svr repository ChangePinBackup). Only
                // the second may fall back to creating one: doing it for the
                // first replaced a live backup without the current PIN.
                match self.svr_backup().has_key_backup().await {
                    Ok(false) => {
                        tracing::info!("No existing SVR backup found, creating new backup with new PIN");
                        self.svr_backup()
                            .create_and_upload_backup(&new_svr_secret, &identity_keypair, &profile_key, &full_username)
                            .await
                            .map(|_| ())
                    }
                    Ok(true) => {
                        old_svr_secret.zeroize();
                        new_svr_secret.zeroize();
                        identity_keypair.zeroize();
                        profile_key.zeroize();
                        new_reg_lock.zeroize();
                        return Err(ClientError::Validation("Current PIN is incorrect".into()));
                    }
                    Err(e) => Err(e),
                }
            }
            _ => result.map(|_| ()),
        };

        // 8. Zeroize ALL sensitive intermediates (both success and error paths)
        old_svr_secret.zeroize();
        new_svr_secret.zeroize();
        identity_keypair.zeroize();
        profile_key.zeroize();
        new_reg_lock.zeroize();

        final_result.map_err(|e| ClientError::Validation(format!("PIN change failed: {}", e)))?;

        // Refresh local PIN fingerprint so re-auth gates (linked-device approve)
        // accept the new PIN immediately. Best-effort: a stored fingerprint
        // mismatch only blocks the linked-device flow, not message send.
        match crate::crypto::svr::derive_pin_fingerprint(new_pin, username) {
            Ok(mut fp) => {
                if let Err(e) = self
                    .signal()
                    .settings_repo
                    .set_bytes(keys::PIN_FINGERPRINT, &fp)
                {
                    tracing::warn!(error = %e, "Failed to persist new PIN fingerprint");
                }
                fp.zeroize();
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to derive new PIN fingerprint");
            }
        }

        tracing::info!("PIN changed successfully");
        Ok(())
    }

    /// Attempt to restore identity keys from SVR backup on a new device.
    ///
    /// Checks whether local identity keys exist. If they do, this is an
    /// existing device and no restore is needed. If they are missing, derives
    /// the SVR secret from PIN + username and fetches the encrypted backup.
    ///
    /// On success, hydrates the local identity key store and profile key.
    /// On failure, sets `result.key_recovery` to the classified reason. That
    /// is not advisory: `login` turns it into an error, because a device with
    /// no identity key cannot send or receive anything and reporting success
    /// is what made the loss invisible.
    async fn attempt_svr_restore(
        &self,
        pin: &str,
        username: &str,
        result: &mut LoginResult,
    ) {
        use crate::crypto::svr::derive_svr_secret;
        use zeroize::Zeroize;

        // 1. Never overwrite an identity this device already holds.
        match self.signal().key_store.has_local_identity() {
            Ok(false) => {}
            Ok(true) => {
                tracing::debug!("SVR restore skipped: local identity keys already exist");
                result.key_recovery = None;
                return;
            }
            Err(e) => {
                tracing::warn!("SVR restore: failed to check local identity: {}", e);
                result.key_recovery = Some(KeyRecoveryNeed::Unavailable);
                return;
            }
        }

        tracing::info!("New device detected (no local identity keys) — attempting SVR restore");

        // 2. Derive svr_secret transiently from PIN + username
        let mut svr_secret = match derive_svr_secret(pin, username) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("SVR restore failed: could not derive svr_secret: {}", e);
                result.key_recovery = Some(KeyRecoveryNeed::Unavailable);
                return;
            }
        };

        // 3. Fetch and decrypt backup from server
        let restore_result = match self.svr_backup().fetch_and_restore_backup(&svr_secret).await {
            Ok(r) => r,
            Err(crate::services::svr_backup_service::SvrBackupError::BackupDestroyed) => {
                svr_secret.zeroize();
                tracing::error!("SVR backup permanently destroyed (too many failed PIN attempts). User must re-register PIN.");
                result.key_recovery = Some(KeyRecoveryNeed::BackupDestroyed);
                return;
            }
            Err(e) => {
                svr_secret.zeroize();
                // Classify rather than collapse. Which failure this was decides
                // whether the user retypes a PIN, waits, or accepts losing
                // their conversations — see `key_recovery::classify_restore_failure`.
                let mut need = crate::services::key_recovery::classify_restore_failure(&e);
                // A miss is ambiguous by construction — the backup id is
                // PIN-derived, so "wrong PIN" and "no backup at all" both land
                // here. `classify_restore_failure` takes the cautious reading
                // (never authorise minting on a typo); ask the server, which is
                // the only party that can actually tell, before settling.
                if need == KeyRecoveryNeed::WrongPin {
                    match self.svr_backup().has_key_backup().await {
                        Ok(false) => need = KeyRecoveryNeed::NoBackup,
                        Ok(true) => {}
                        // Unknown: keep the cautious reading rather than
                        // upgrading a network blip into permission to destroy
                        // the user's identity key.
                        Err(probe_err) => tracing::warn!(
                            "could not determine whether a key backup exists: {}",
                            probe_err
                        ),
                    }
                }
                tracing::warn!(?need, "SVR restore failed: {}", e);
                result.key_recovery = Some(need);
                return;
            }
        };

        svr_secret.zeroize();

        // 4. Hydrate identity keypair into local key store
        //    Reconstruct libsignal IdentityKeyPair from raw 32-byte private + 32-byte public
        let mut restored_keypair = restore_result.identity_keypair; // [u8; 64]
        let mut restored_profile_key = restore_result.profile_key;  // [u8; 32]

        let hydrate_ok = (|| -> std::result::Result<(), String> {
            use libsignal_protocol::{IdentityKey, IdentityKeyPair, PrivateKey};

            let private_key = PrivateKey::deserialize(&restored_keypair[..32])
                .map_err(|e| format!("invalid private key: {}", e))?;

            // IdentityKey expects 33 bytes: 0x05 type prefix + 32-byte public key
            let mut public_with_prefix = [0u8; 33];
            public_with_prefix[0] = 0x05;
            public_with_prefix[1..].copy_from_slice(&restored_keypair[32..]);
            let identity_key = IdentityKey::try_from(public_with_prefix.as_slice())
                .map_err(|e| format!("invalid public key: {}", e))?;

            let key_pair = IdentityKeyPair::new(identity_key, private_key);
            let serialized = key_pair.serialize();

            // Generate a registration ID for this new device (14-bit, 1-16383)
            let registration_id = (rand::random::<u32>() % 0x3FFF) + 1;

            self.signal()
                .key_store
                .set_local_identity(registration_id, &serialized)
                .map_err(|e| format!("failed to store identity: {}", e))?;

            // Store identity public key in settings for NDSS'21 friend codes
            let identity_public_full = key_pair.identity_key().serialize();
            self.signal()
                .settings_repo
                .set_bytes(keys::IDENTITY_KEY_PUBLIC, identity_public_full.as_ref())
                .map_err(|e| format!("failed to store identity public key: {}", e))?;

            Ok(())
        })();

        if let Err(e) = hydrate_ok {
            tracing::warn!("SVR restore: failed to hydrate identity keypair: {}", e);
            restored_keypair.zeroize();
            restored_profile_key.zeroize();
            result.key_recovery = Some(KeyRecoveryNeed::Unavailable);
            return;
        }

        // 5. Hydrate profile key into settings
        if let Err(e) = self
            .signal()
            .settings_repo
            .set_bytes(keys::PROFILE_KEY, &restored_profile_key)
        {
            tracing::warn!("SVR restore: failed to store profile key: {}", e);
            // Identity was stored but profile key failed — partial restore
            restored_keypair.zeroize();
            restored_profile_key.zeroize();
            result.keys_restored = true;
            result.key_recovery = None;
            return;
        }

        // 6. Hydrate the account handle. It was written only at registration,
        //    so a reinstall had none and showed no username (§117). Keys are
        //    already restored, so a failure here does not undo them.
        if let Err(e) = self
            .signal()
            .settings_repo
            .set_string(keys::FULL_USERNAME, &restore_result.full_username)
        {
            tracing::warn!("SVR restore: failed to store username: {}", e);
        }

        // 7. Zeroize sensitive intermediates
        restored_keypair.zeroize();
        restored_profile_key.zeroize();

        result.keys_restored = true;
        result.key_recovery = None;

        tracing::info!(
            remaining_attempts = restore_result.remaining_attempts,
            "SVR key restore completed successfully"
        );
    }

    /// Log out and shut down all background services.
    /// Sign out: tell the server, wipe local credentials, and stop every
    /// background loop. The teardown runs even if the first step fails, so a
    /// signed-out client never keeps syncing and a later login can `start()`.
    pub async fn logout(&self) -> Result<(), ClientError> {
        let result = self.auth().do_logout().await;
        self.shutdown().await;
        self.auth_state().set_logged_out();
        result?;
        Ok(())
    }

}
