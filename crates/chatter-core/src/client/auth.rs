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
    /// If `pin` is provided and this is a new device (no local identity keys),
    /// the method attempts to restore keys from an SVR backup. The result
    /// fields `keys_restored` / `keys_missing` indicate the outcome.
    #[cfg(not(target_os = "android"))]
    pub fn oidc(&self) -> Arc<crate::services::oidc_service::OidcService> {
        self.oidc_service.get_or_init(|| {
            Arc::new(crate::services::oidc_service::OidcService::new(self.ui.auth_service.settings_repo.clone()))
        }).clone()
    }

    pub async fn login_with_oidc_token(
        &self,
        access_token: &str,
        _device_name: &str,
    ) -> Result<LoginResult, ClientError> {
        self.auth()
            .login_with_oidc_token(access_token)
            .await
            .map_err(ClientError::Auth)
    }

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

        // Attempt SVR key restore if this is a new device (no local identity keys)
        if let Some(p) = pin {
            self.attempt_svr_restore(p, username, &mut result).await;
        }

        self.start().await?;
        Ok(result)
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
        Ok(result)
    }

    /// Use a one-time recovery code to reset password and SVR credentials.
    ///
    /// This is called when the user has lost their password but has a recovery code
    /// from registration. It resets the password and SVR key material on the server.
    ///
    /// After this succeeds, the user should login with `new_password` + `new_pin`.
    /// The SVR backup will be created automatically during that login flow.
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
        Ok(RecoveryResult { remaining_codes })
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

    /// Retry SVR backup creation if it failed during registration.
    ///
    /// Delegates to `SvrBackupService::retry_from_credentials` which reads
    /// the locally-stored profile_key and uploads it under a PIN-derived
    /// SVR secret. Caller must have already password-re-auth-gated the user.
    pub async fn retry_svr_backup(
        &self,
        pin: &str,
        username: &str,
    ) -> Result<(), ClientError> {
        self.svr_backup()
            .retry_from_credentials(pin, username)
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
    pub async fn change_pin(
        &self,
        old_pin: &str,
        new_pin: &str,
        username: &str,
    ) -> Result<(), ClientError> {
        use crate::crypto::svr::{
            derive_registration_lock, derive_svr_secret, validate_pin, SvrKeypair,
        };
        use crate::storage::repositories::settings_repo::keys;
        use zeroize::Zeroize;

        validate_pin(old_pin).map_err(|e| ClientError::Validation(format!("old PIN: {e}")))?;
        validate_pin(new_pin).map_err(|e| ClientError::Validation(format!("new PIN: {e}")))?;
        if old_pin == new_pin {
            return Err(ClientError::Validation(
                "New PIN must be different from current PIN".into(),
            ));
        }

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
                &new_reg_lock,
                &new_svr_public_key,
            )
            .await;

        let final_result = match &result {
            Err(crate::services::svr_backup_service::SvrBackupError::BackupNotFound)
            | Err(crate::services::svr_backup_service::SvrBackupError::Network(
                crate::error::NetworkError::NotFound(_),
            )) => {
                // No existing backup — create one with the new PIN instead
                tracing::info!("No existing SVR backup found, creating new backup with new PIN");
                self.svr_backup()
                    .create_and_upload_backup(&new_svr_secret, &identity_keypair, &profile_key)
                    .await
                    .map(|_| ())
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
    /// On failure, logs a warning and sets `keys_missing = true` so the UI
    /// can inform the user. Restore failure NEVER blocks login.
    async fn attempt_svr_restore(
        &self,
        pin: &str,
        username: &str,
        result: &mut LoginResult,
    ) {
        use crate::crypto::svr::derive_svr_secret;
        use crate::storage::repositories::settings_repo::keys;
        use zeroize::Zeroize;

        // 1. Check if local identity keys already exist
        let has_identity = match self.signal().key_store.has_local_identity() {
            Ok(exists) => exists,
            Err(e) => {
                tracing::warn!("SVR restore: failed to check local identity: {}", e);
                // Assume keys exist to avoid accidentally overwriting
                return;
            }
        };

        if has_identity {
            tracing::debug!("SVR restore skipped: local identity keys already exist");
            return;
        }

        tracing::info!("New device detected (no local identity keys) — attempting SVR restore");

        // 2. Derive svr_secret transiently from PIN + username
        let mut svr_secret = match derive_svr_secret(pin, username) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("SVR restore failed: could not derive svr_secret: {}", e);
                result.keys_missing = true;
                return;
            }
        };

        // 3. Fetch and decrypt backup from server
        let restore_result = match self.svr_backup().fetch_and_restore_backup(&svr_secret).await {
            Ok(r) => r,
            Err(crate::services::svr_backup_service::SvrBackupError::BackupDestroyed) => {
                svr_secret.zeroize();
                tracing::error!("SVR backup permanently destroyed (too many failed PIN attempts). User must re-register PIN.");
                result.keys_missing = true;
                result.backup_destroyed = true;
                return;
            }
            Err(e) => {
                svr_secret.zeroize();
                tracing::warn!("SVR restore failed (non-fatal): {}", e);
                result.keys_missing = true;
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
            result.keys_missing = true;
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
            result.keys_missing = false;
            return;
        }

        // 6. Zeroize sensitive intermediates
        restored_keypair.zeroize();
        restored_profile_key.zeroize();

        result.keys_restored = true;
        result.keys_missing = false;

        tracing::info!(
            remaining_attempts = restore_result.remaining_attempts,
            "SVR key restore completed successfully"
        );
    }

    /// Log out and shut down all background services.
    pub async fn logout(&self) -> Result<(), ClientError> {
        self.auth().do_logout().await?;
        self.shutdown().await;
        self.auth_state().set_logged_out();
        Ok(())
    }

}
