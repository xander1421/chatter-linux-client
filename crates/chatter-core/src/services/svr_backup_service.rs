//! SVR Backup Service.
//!
//! Orchestrates the encrypted key backup lifecycle: create, upload, fetch,
//! restore, and delete. Coordinates crypto operations (encrypt/decrypt),
//! network calls (gRPC), and local settings (version tracking).
//!
//! # Security Model
//!
//! - The `svr_secret` (32 bytes, derived from PIN) is NEVER stored — only
//!   passed transiently into each method call.
//! - Backups are encrypted with AES-256-GCM keyed by `svr_secret`.
//! - The server indexes backups by HMAC-SHA256(svr_secret, "BackupId") and
//!   never sees the plaintext secret or PIN.
//! - Fetch is rate-limited server-side to prevent PIN brute-force attacks.

use std::sync::Arc;

use zeroize::Zeroize;

use crate::crypto::svr_backup::{
    KeyBackupPayload, decrypt_backup, derive_backup_id, encrypt_backup,
};
use crate::error::{CryptoError, NetworkError, StorageError};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Settings key for the monotonic SVR backup version counter.
const SVR_BACKUP_VERSION_KEY: &str = "crypto.svr_backup_version";

/// Errors from SVR backup operations.
#[derive(Debug, thiserror::Error)]
pub enum SvrBackupError {
    /// Encryption or decryption failed.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),

    /// gRPC network error.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Local storage error.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Server-side rate limit exhausted (remaining_attempts <= 0).
    #[error("rate limited: no fetch attempts remaining")]
    RateLimited,

    /// The account has no backup on the server (NOT_FOUND).
    #[error("backup not found")]
    BackupNotFound,

    /// A backup exists but the PIN-derived id does not match it
    /// (PERMISSION_DENIED). The attempt cost a server-side PIN strike.
    #[error("wrong PIN")]
    WrongPin,

    /// Backup permanently destroyed by server (too many failed PIN attempts).
    #[error("backup destroyed: re-register PIN to create a new backup")]
    BackupDestroyed,

    /// Free-form error for orchestration-level failures (identity store access,
    /// missing profile key, PIN format). Typed variants above cover the
    /// transport and crypto layers; this variant covers the glue.
    #[error("svr backup: {0}")]
    Generic(String),
}

/// Map the errors of a PIN-checked SVR RPC (FetchBackup, ChangePinBackup).
/// The server answers NOT_FOUND only when the account has no backup and
/// PERMISSION_DENIED when one exists under a different PIN-derived id.
fn classify_pin_rpc_error(e: NetworkError) -> SvrBackupError {
    match e {
        NetworkError::NotFound(_) => SvrBackupError::BackupNotFound,
        NetworkError::PermissionDenied => SvrBackupError::WrongPin,
        NetworkError::RateLimited => SvrBackupError::RateLimited,
        NetworkError::DataLoss(_) => {
            tracing::error!("SVR backup destroyed by server (too many failed PIN attempts)");
            SvrBackupError::BackupDestroyed
        }
        e => SvrBackupError::Network(e),
    }
}

/// Result of a successful backup restore.
pub struct RestoreResult {
    /// The recovered identity keypair (32-byte private || 32-byte public).
    pub identity_keypair: [u8; 64],
    /// The recovered 32-byte profile encryption key.
    pub profile_key: [u8; 32],
    /// The recovered `nickname.discriminator` handle (`keys::FULL_USERNAME`).
    pub full_username: String,
    /// How many fetch attempts remain before server-side lockout.
    pub remaining_attempts: i32,
}

/// SVR Backup Service.
///
/// Manages the encrypted key backup lifecycle. Created once at startup and
/// shared via `Arc<SvrBackupService>`.
///
/// # Example
///
/// ```ignore
/// let svc = SvrBackupService::new(grpc_pool.clone(), settings_repo.clone());
///
/// // Create and upload backup (after registration or key rotation)
/// svc.create_and_upload_backup(&svr_secret, &identity_keypair, &profile_key).await?;
///
/// // Restore backup (during new-device login)
/// let result = svc.fetch_and_restore_backup(&svr_secret).await?;
///
/// // Delete backup (account deletion or key rotation)
/// svc.delete_backup(&svr_secret).await?;
/// ```
pub struct SvrBackupService {
    grpc_pool: Arc<GrpcClientPool>,
    settings: Arc<SettingsRepository>,
    signal_service: Arc<crate::services::signal::SignalService>,
}

impl SvrBackupService {
    /// Create a new SvrBackupService.
    ///
    /// # Arguments
    ///
    /// * `grpc_pool` - Connection pool for creating gRPC clients
    /// * `settings` - Settings repository for persisting the backup version counter
    /// * `signal_service` - Access to the identity keypair for backup payload assembly
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        settings: Arc<SettingsRepository>,
        signal_service: Arc<crate::services::signal::SignalService>,
    ) -> Self {
        Self {
            grpc_pool,
            settings,
            signal_service,
        }
    }

    /// The account handle every backup carries (`keys::FULL_USERNAME`).
    ///
    /// Required: a backup without it restores an account with no handle
    /// (§117), and every later backup would carry that loss forward.
    pub fn local_full_username(&self) -> Result<String, SvrBackupError> {
        use crate::storage::repositories::settings_repo::keys;
        self.signal_service
            .settings_repo
            .get_string(keys::FULL_USERNAME)
            .map_err(|e| SvrBackupError::Generic(format!("failed to read username: {e}")))?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| SvrBackupError::Generic("no local username to back up".into()))
    }

    /// Run the full post-registration backup flow from raw credentials.
    ///
    /// Derives the SVR secret from PIN + username, fetches the identity
    /// keypair from the Signal store, reads the profile key from settings,
    /// and uploads an encrypted backup. All sensitive intermediates are
    /// zeroized. Failure is non-fatal to the caller — the most common cause
    /// is a transient network error at registration time.
    pub async fn backup_from_pin(&self, pin: &str, username: &str) -> Result<u32, SvrBackupError> {
        use crate::storage::repositories::settings_repo::keys;
        use zeroize::Zeroize;

        let mut profile_key = match self
            .signal_service
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)
        {
            Ok(Some(bytes)) if bytes.len() == 32 => {
                let mut pk = [0u8; 32];
                pk.copy_from_slice(&bytes);
                pk
            }
            Ok(_) => {
                return Err(SvrBackupError::Generic(
                    "profile key not found or invalid length".into(),
                ));
            }
            Err(e) => {
                return Err(SvrBackupError::Generic(format!(
                    "failed to read profile key: {e}"
                )));
            }
        };
        let result = self
            .backup_with_profile_key(pin, username, &profile_key)
            .await;
        profile_key.zeroize();
        result
    }

    /// [`Self::backup_from_pin`] for an explicit profile key — a rotation
    /// backs up the NEW key before it becomes the local one, so a restore can
    /// never bring back a key the server profile is no longer encrypted under.
    /// The caller must have verified `pin`: a wrong one would replace the
    /// account's backup with one the real PIN cannot open.
    pub async fn backup_with_profile_key(
        &self,
        pin: &str,
        username: &str,
        profile_key: &[u8; 32],
    ) -> Result<u32, SvrBackupError> {
        use crate::crypto::svr::derive_svr_secret_async;
        use zeroize::Zeroize;

        // CORRECTNESS: off the async worker — see `derive_svr_secret_async`.
        // Registration calls this immediately after the account is created,
        // while the client's event stream and h2 connection are live; blocking
        // the runtime here for ~450ms stalls both.
        let mut svr_secret = derive_svr_secret_async(pin, username)
            .await
            .map_err(|e| SvrBackupError::Crypto(e))?;

        let mut private_bytes = self
            .signal_service
            .actor
            .get_identity_private_key()
            .await
            .map_err(|e| {
                svr_secret.zeroize();
                SvrBackupError::Generic(format!("failed to get identity private key: {e}"))
            })?;
        let public_bytes_33 = self
            .signal_service
            .actor
            .get_identity_public_key()
            .await
            .map_err(|e| {
                svr_secret.zeroize();
                private_bytes.zeroize();
                SvrBackupError::Generic(format!("failed to get identity public key: {e}"))
            })?;
        let mut identity_keypair = [0u8; 64];
        identity_keypair[..32].copy_from_slice(&private_bytes);
        identity_keypair[32..].copy_from_slice(&public_bytes_33[1..]);
        private_bytes.zeroize();

        let result = match self.local_full_username() {
            Ok(full_username) => {
                store_retrying_version_conflict(
                    || {
                        self.create_and_upload_backup(
                            &svr_secret,
                            &identity_keypair,
                            profile_key,
                            &full_username,
                        )
                    },
                    || self.refresh_backup_version(&svr_secret),
                )
                .await
            }
            Err(e) => Err(e),
        };

        svr_secret.zeroize();
        identity_keypair.zeroize();

        result
    }

    /// Prove `pin` opens the account's backup by fetching and decrypting it,
    /// for a device with no local PIN fingerprint (a linked one). Nothing is
    /// written, so a wrong PIN leaves the backup as it was. Costs one of the
    /// server's metered attempts, which the backup write that follows resets.
    ///
    /// Returns the local PIN fingerprint, derived from the same secret, for the
    /// caller to persist so later checks stay local. Also syncs the local
    /// version counter to the server's.
    pub async fn verify_pin_against_backup(
        &self,
        pin: &str,
        username: &str,
    ) -> Result<[u8; 32], SvrBackupError> {
        use crate::crypto::svr::{derive_svr_secret_async, pin_fingerprint_from_secret};

        let mut svr_secret = derive_svr_secret_async(pin, username).await?;
        let result = self.refresh_backup_version(&svr_secret).await;
        let fingerprint = pin_fingerprint_from_secret(&svr_secret);
        svr_secret.zeroize();
        result.map(|()| fingerprint)
    }

    /// Fetch + decrypt the backup only to learn the server's version, zeroizing
    /// the recovered keys.
    async fn refresh_backup_version(&self, svr_secret: &[u8; 32]) -> Result<(), SvrBackupError> {
        let mut restored = self.fetch_and_restore_backup(svr_secret).await?;
        restored.identity_keypair.zeroize();
        restored.profile_key.zeroize();
        Ok(())
    }

    /// Public retry: load profile_key from local settings, then run the same
    /// flow as [`backup_from_pin`]. Used when the initial backup at
    /// registration failed and the user is re-authenticating.
    ///
    /// Profile_key is random (Signal-aligned) and stored locally at
    /// registration; this retry just reads it. The caller must already have
    /// gated the operation behind a password / PIN re-auth check (e.g.
    /// `LinkDeviceService::verify_password_or_pin`).
    pub async fn retry_from_credentials(
        &self,
        pin: &str,
        username: &str,
    ) -> Result<(), SvrBackupError> {
        use crate::crypto::svr::derive_svr_secret;
        use crate::storage::repositories::settings_repo::keys;
        use zeroize::Zeroize;

        crate::crypto::svr::validate_pin_shape(pin)
            .map_err(|e| SvrBackupError::Generic(e.to_string()))?;

        let stored = self
            .signal_service
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)
            .map_err(|e| SvrBackupError::Generic(format!("settings read failed: {e}")))?
            .ok_or_else(|| SvrBackupError::Generic("no local profile_key to back up".into()))?;
        if stored.len() != 32 {
            return Err(SvrBackupError::Generic(
                "local profile_key has wrong length".into(),
            ));
        }
        let mut profile_key = [0u8; 32];
        profile_key.copy_from_slice(&stored);

        let mut private_bytes = self
            .signal_service
            .actor
            .get_identity_private_key()
            .await
            .map_err(|e| {
                profile_key.zeroize();
                SvrBackupError::Generic(format!("failed to get identity private key: {e}"))
            })?;
        let public_bytes_33 = self
            .signal_service
            .actor
            .get_identity_public_key()
            .await
            .map_err(|e| {
                profile_key.zeroize();
                private_bytes.zeroize();
                SvrBackupError::Generic(format!("failed to get identity public key: {e}"))
            })?;
        let mut identity_keypair = [0u8; 64];
        identity_keypair[..32].copy_from_slice(&private_bytes);
        identity_keypair[32..].copy_from_slice(&public_bytes_33[1..]);
        private_bytes.zeroize();

        let mut svr_secret = derive_svr_secret(pin, username).map_err(|e| {
            profile_key.zeroize();
            identity_keypair.zeroize();
            SvrBackupError::Crypto(e)
        })?;

        let result = match self.local_full_username() {
            Ok(full_username) => {
                self.create_and_upload_backup(
                    &svr_secret,
                    &identity_keypair,
                    &profile_key,
                    &full_username,
                )
                .await
            }
            Err(e) => Err(e),
        };

        svr_secret.zeroize();
        identity_keypair.zeroize();
        profile_key.zeroize();

        result.map(|_| ())
    }

    /// Create an encrypted backup and upload it to the server.
    ///
    /// Encrypts the identity keypair and profile key with the SVR secret,
    /// derives a deterministic backup ID, increments the version counter,
    /// and uploads the result. On success, stores the confirmed version locally.
    ///
    /// # Arguments
    ///
    /// * `svr_secret` - 32-byte secret derived from user's PIN (never stored)
    /// * `identity_keypair` - 64-byte Signal identity keypair (private || public)
    /// * `profile_key` - 32-byte profile encryption key
    /// * `full_username` - the account handle (see [`Self::local_full_username`])
    ///
    /// # Returns
    ///
    /// The version number confirmed by the server.
    pub async fn create_and_upload_backup(
        &self,
        svr_secret: &[u8; 32],
        identity_keypair: &[u8; 64],
        profile_key: &[u8; 32],
        full_username: &str,
    ) -> Result<u32, SvrBackupError> {
        // 1. Build and encrypt the payload
        let payload = KeyBackupPayload {
            identity_keypair: *identity_keypair,
            profile_key: *profile_key,
            full_username: full_username.to_owned(),
        };
        let encrypted = encrypt_backup(&payload, svr_secret)?;

        // 2. Derive deterministic backup ID
        let backup_id_hash = derive_backup_id(svr_secret);

        // 3. Read current version and increment
        let current_version: u32 = self
            .settings
            .get::<u32>(SVR_BACKUP_VERSION_KEY)?
            .unwrap_or(0);
        let next_version = current_version + 1;

        // 4. Upload
        let mut client = self.grpc_pool.svr_client();
        let confirmed_version = client
            .store_backup(encrypted, backup_id_hash.to_vec(), next_version)
            .await?;

        // 5. Persist confirmed version locally
        self.settings
            .set(SVR_BACKUP_VERSION_KEY, &confirmed_version)?;

        tracing::info!(
            version = confirmed_version,
            "SVR backup uploaded successfully"
        );

        Ok(confirmed_version)
    }

    /// Fetch and decrypt a backup from the server.
    ///
    /// Derives the backup ID from the SVR secret, fetches the encrypted backup,
    /// decrypts it, and returns the recovered key material. Stores the server
    /// version locally for future increments.
    ///
    /// # Arguments
    ///
    /// * `svr_secret` - 32-byte secret derived from user's PIN (never stored)
    ///
    /// # Returns
    ///
    /// The decrypted identity keypair, profile key, and remaining fetch attempts.
    ///
    /// # Errors
    ///
    /// - `BackupNotFound` if no backup exists for this secret
    /// - `RateLimited` if no fetch attempts remain
    /// - `Crypto` if decryption fails (wrong PIN or corrupted backup)
    /// Whether the account has an SVR key backup at all, independent of the PIN.
    ///
    /// Used to classify a failed restore: with no backup on file, "restore
    /// failed" means there is nothing to recover and a new identity is the only
    /// way forward; with one on file it means the PIN was wrong and minting
    /// would destroy a live key.
    pub async fn has_key_backup(&self) -> Result<bool, SvrBackupError> {
        let mut client = self.grpc_pool.svr_client();
        client
            .has_key_backup()
            .await
            .map_err(SvrBackupError::Network)
    }

    pub async fn fetch_and_restore_backup(
        &self,
        svr_secret: &[u8; 32],
    ) -> Result<RestoreResult, SvrBackupError> {
        // 1. Derive backup ID
        let backup_id_hash = derive_backup_id(svr_secret);

        // 2. Fetch from server
        let mut client = self.grpc_pool.svr_client();
        let result = client
            .fetch_backup(backup_id_hash.to_vec())
            .await
            .map_err(classify_pin_rpc_error)?;

        // 3. Check remaining attempts
        if result.remaining_attempts <= 0 {
            tracing::warn!(
                remaining = result.remaining_attempts,
                "SVR fetch rate limit exhausted"
            );
            return Err(SvrBackupError::RateLimited);
        }

        // 4. Decrypt
        let payload = decrypt_backup(&result.encrypted_backup, svr_secret)?;

        // 5. Store server version locally for future increments
        self.settings.set(SVR_BACKUP_VERSION_KEY, &result.version)?;

        tracing::info!(
            version = result.version,
            remaining_attempts = result.remaining_attempts,
            "SVR backup restored successfully"
        );

        Ok(RestoreResult {
            identity_keypair: payload.identity_keypair,
            profile_key: payload.profile_key,
            full_username: payload.full_username.clone(),
            remaining_attempts: result.remaining_attempts,
        })
    }

    /// Re-encrypt a backup with a new PIN-derived key (PIN change).
    ///
    /// Derives backup IDs for both old and new secrets, encrypts the payload
    /// with the new secret, increments the version, and calls the server's
    /// atomic ChangePinBackup RPC. On success, stores the confirmed version.
    ///
    /// # Arguments
    ///
    /// * `old_svr_secret` - 32-byte secret derived from current PIN
    /// * `new_svr_secret` - 32-byte secret derived from new PIN
    /// * `identity_keypair` - 64-byte Signal identity keypair (private || public)
    /// * `profile_key` - 32-byte profile encryption key
    /// * `full_username` - the account handle (see [`Self::local_full_username`])
    /// * `new_registration_lock` - 32-byte registration lock from new PIN
    /// * `new_svr_public_key` - 32-byte SVR public key from new PIN
    pub async fn change_pin_backup(
        &self,
        old_svr_secret: &[u8; 32],
        new_svr_secret: &[u8; 32],
        identity_keypair: &[u8; 64],
        profile_key: &[u8; 32],
        full_username: &str,
        new_registration_lock: &[u8; 32],
        new_svr_public_key: &[u8; 32],
    ) -> Result<(), SvrBackupError> {
        // 1. Derive old backup ID (proves ownership of current backup)
        let old_backup_id_hash = derive_backup_id(old_svr_secret);

        // 2. Build and encrypt the payload with the NEW secret
        let mut payload = KeyBackupPayload {
            identity_keypair: *identity_keypair,
            profile_key: *profile_key,
            full_username: full_username.to_owned(),
        };
        let encrypted = encrypt_backup(&payload, new_svr_secret)?;

        // Zeroize payload immediately after encryption
        payload.zeroize();

        // 3. Derive new backup ID
        let new_backup_id_hash = derive_backup_id(new_svr_secret);

        // 4. Read current version and increment
        let current_version: u32 = self
            .settings
            .get::<u32>(SVR_BACKUP_VERSION_KEY)?
            .unwrap_or(0);
        let next_version = current_version + 1;

        // 5. Call server's atomic change-pin RPC
        let mut client = self.grpc_pool.svr_client();
        let confirmed_version = client
            .change_pin_backup(
                encrypted,
                new_backup_id_hash.to_vec(),
                old_backup_id_hash.to_vec(),
                next_version,
                new_registration_lock.to_vec(),
                new_svr_public_key.to_vec(),
            )
            .await
            .map_err(classify_pin_rpc_error)?;

        // 6. Persist confirmed version locally
        self.settings
            .set(SVR_BACKUP_VERSION_KEY, &confirmed_version)?;

        tracing::info!(
            version = confirmed_version,
            "SVR backup re-encrypted with new PIN successfully"
        );

        Ok(())
    }

    /// Clear the local backup version counter.
    ///
    /// Called when the SERVER's row has gone away, so the two counters agree
    /// again. `StoreBackup` accepts a version of exactly stored+1, so a client
    /// whose counter outlives the row it was counting is rejected on every
    /// subsequent backup — a permanent `ErrVersionConflict` that surfaces only
    /// as a "backup failed (non-fatal)" log line.
    ///
    /// The one caller that matters is the recovery-code login: it replaces the
    /// PIN, and the server drops the backup that was keyed to the old one.
    ///
    /// Associated (not `&self`) so the recovery path, which runs before a
    /// session exists, can clear it with only the settings repository.
    pub fn clear_local_backup_version(
        settings: &Arc<SettingsRepository>,
    ) -> Result<(), SvrBackupError> {
        settings.delete(SVR_BACKUP_VERSION_KEY)?;
        tracing::info!(
            "Local SVR backup version counter cleared — the next backup will be version 1"
        );
        Ok(())
    }

    /// Delete a backup from the server.
    ///
    /// Derives the backup ID and requests deletion. Also clears the local
    /// version counter.
    ///
    /// # Arguments
    ///
    /// * `svr_secret` - 32-byte secret derived from user's PIN (never stored)
    pub async fn delete_backup(&self, svr_secret: &[u8; 32]) -> Result<(), SvrBackupError> {
        // 1. Derive backup ID
        let backup_id_hash = derive_backup_id(svr_secret);

        // 2. Delete from server
        let mut client = self.grpc_pool.svr_client();
        client.delete_backup(backup_id_hash.to_vec()).await?;

        // 3. Clear local version counter
        let _ = self.settings.delete(SVR_BACKUP_VERSION_KEY);

        tracing::info!("SVR backup deleted successfully");

        Ok(())
    }
}

/// `StoreBackup` accepts exactly stored+1, and the local counter only tracks
/// what THIS device last wrote or read — another device's write leaves it
/// behind. On that conflict, `refresh` re-reads the server's version and the
/// store runs once more. The caller must have verified the PIN: `refresh`
/// fetches under it, a metered attempt.
async fn store_retrying_version_conflict<S, SF, R, RF>(
    mut store: S,
    refresh: R,
) -> Result<u32, SvrBackupError>
where
    S: FnMut() -> SF,
    SF: std::future::Future<Output = Result<u32, SvrBackupError>>,
    R: FnOnce() -> RF,
    RF: std::future::Future<Output = Result<(), SvrBackupError>>,
{
    match store().await {
        Err(e) if is_version_conflict(&e) => {
            tracing::info!("SVR backup version was stale; re-reading it and retrying once");
            refresh().await?;
            store().await
        }
        r => r,
    }
}

/// The server's `ErrVersionConflict` is its only `FailedPrecondition` on the
/// backup RPCs (svr/handlers `mapError`), which `NetworkError::from_status`
/// turns into this prefix.
fn is_version_conflict(e: &SvrBackupError) -> bool {
    matches!(e, SvrBackupError::Network(NetworkError::InvalidRequest(m)) if m.starts_with("precondition failed"))
}

#[cfg(test)]
mod conflict_tests {
    use super::*;
    use std::cell::Cell;

    fn conflict() -> SvrBackupError {
        SvrBackupError::Network(NetworkError::InvalidRequest(
            "precondition failed: backup version conflict".into(),
        ))
    }

    #[tokio::test]
    async fn a_stale_version_is_refreshed_and_the_store_retried_once() {
        let stores = Cell::new(0);
        let refreshes = Cell::new(0);
        let v = store_retrying_version_conflict(
            || {
                stores.set(stores.get() + 1);
                let n = stores.get();
                async move { if n == 1 { Err(conflict()) } else { Ok(8) } }
            },
            || {
                refreshes.set(refreshes.get() + 1);
                async { Ok(()) }
            },
        )
        .await
        .expect("the retry after a refresh must succeed");
        assert_eq!((v, stores.get(), refreshes.get()), (8, 2, 1));
    }

    #[tokio::test]
    async fn a_second_conflict_is_returned_not_looped() {
        let stores = Cell::new(0);
        let r = store_retrying_version_conflict(
            || {
                stores.set(stores.get() + 1);
                async { Err::<u32, _>(conflict()) }
            },
            || async { Ok(()) },
        )
        .await;
        assert!(r.is_err_and(|e| is_version_conflict(&e)));
        assert_eq!(stores.get(), 2, "exactly one retry");
    }

    #[tokio::test]
    async fn other_failures_neither_refresh_nor_retry() {
        let stores = Cell::new(0);
        let r = store_retrying_version_conflict(
            || {
                stores.set(stores.get() + 1);
                async { Err::<u32, _>(SvrBackupError::Network(NetworkError::Unavailable)) }
            },
            || async { panic!("refresh is a metered PIN attempt; only a conflict earns it") },
        )
        .await;
        assert!(r.is_err());
        assert_eq!(stores.get(), 1);
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::storage::Database;
    use tempfile::TempDir;

    fn settings() -> (TempDir, Arc<SettingsRepository>) {
        let dir = TempDir::new().unwrap();
        let db = Database::open(&dir.path().join("test.db"), "test_passphrase").unwrap();
        db.run_migrations().unwrap();
        (dir, Arc::new(SettingsRepository::new(Arc::new(db))))
    }

    /// A recovery-code login replaces the PIN, so the server drops the backup
    /// row that was keyed to the old one (auth `UseRecoveryCode` step 9b). The
    /// server's stored version therefore goes back to 0, and `StoreBackup`
    /// accepts exactly stored+1 — so the only version that will ever be
    /// accepted afterwards is 1.
    ///
    /// A client that keeps its old counter posts N+1 and is rejected forever
    /// with `ErrVersionConflict`, which is precisely the "backup failed
    /// (non-fatal)" line the user never sees. The counter has to be cleared on
    /// the client at the same moment it is cleared on the server.
    #[test]
    fn a_recovery_login_clears_the_local_backup_version_counter() {
        let (_d, settings) = settings();

        // This device had backed up several times before the account was lost.
        settings.set(SVR_BACKUP_VERSION_KEY, &7u32).unwrap();

        SvrBackupService::clear_local_backup_version(&settings)
            .expect("clearing the counter must not fail");

        let stored: Option<u32> = settings.get::<u32>(SVR_BACKUP_VERSION_KEY).unwrap();
        assert!(
            stored.is_none() || stored == Some(0),
            "the local counter stayed at {stored:?}; the next backup would post \
             {}, and the server — whose row was just deleted — only accepts 1",
            stored.unwrap_or(0) + 1
        );
    }

    /// And the counter that results must actually produce version 1, not just
    /// "something small". This is the assertion that ties the reset to the
    /// server's `stored+1` rule.
    #[test]
    fn the_next_backup_after_a_recovery_login_is_version_one() {
        let (_d, settings) = settings();
        settings.set(SVR_BACKUP_VERSION_KEY, &7u32).unwrap();

        SvrBackupService::clear_local_backup_version(&settings).unwrap();

        let next = settings
            .get::<u32>(SVR_BACKUP_VERSION_KEY)
            .unwrap()
            .unwrap_or(0)
            + 1;
        assert_eq!(
            next, 1,
            "the server deleted the row, so its stored version is 0 and only 1 is \
             accepted; posting {next} is a deterministic ErrVersionConflict"
        );
    }
}
