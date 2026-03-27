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
    decrypt_backup, derive_backup_id, encrypt_backup, KeyBackupPayload,
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

    /// Backup not found on server.
    #[error("backup not found")]
    BackupNotFound,
}

/// Result of a successful backup restore.
pub struct RestoreResult {
    /// The recovered identity keypair (32-byte private || 32-byte public).
    pub identity_keypair: [u8; 64],
    /// The recovered 32-byte profile encryption key.
    pub profile_key: [u8; 32],
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
}

impl SvrBackupService {
    /// Create a new SvrBackupService.
    ///
    /// # Arguments
    ///
    /// * `grpc_pool` - Connection pool for creating gRPC clients
    /// * `settings` - Settings repository for persisting the backup version counter
    pub fn new(grpc_pool: Arc<GrpcClientPool>, settings: Arc<SettingsRepository>) -> Self {
        Self {
            grpc_pool,
            settings,
        }
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
    ///
    /// # Returns
    ///
    /// The version number confirmed by the server.
    pub async fn create_and_upload_backup(
        &self,
        svr_secret: &[u8; 32],
        identity_keypair: &[u8; 64],
        profile_key: &[u8; 32],
    ) -> Result<u32, SvrBackupError> {
        // 1. Build and encrypt the payload
        let payload = KeyBackupPayload {
            identity_keypair: *identity_keypair,
            profile_key: *profile_key,
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
    pub async fn fetch_and_restore_backup(
        &self,
        svr_secret: &[u8; 32],
    ) -> Result<RestoreResult, SvrBackupError> {
        // 1. Derive backup ID
        let backup_id_hash = derive_backup_id(svr_secret);

        // 2. Fetch from server
        let mut client = self.grpc_pool.svr_client();
        let result = client.fetch_backup(backup_id_hash.to_vec()).await.map_err(|e| {
            match &e {
                NetworkError::NotFound(_) => SvrBackupError::BackupNotFound,
                NetworkError::RateLimited => SvrBackupError::RateLimited,
                _ => SvrBackupError::Network(e),
            }
        })?;

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
        self.settings
            .set(SVR_BACKUP_VERSION_KEY, &result.version)?;

        tracing::info!(
            version = result.version,
            remaining_attempts = result.remaining_attempts,
            "SVR backup restored successfully"
        );

        Ok(RestoreResult {
            identity_keypair: payload.identity_keypair,
            profile_key: payload.profile_key,
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
    /// * `new_registration_lock` - 32-byte registration lock from new PIN
    /// * `new_svr_public_key` - 32-byte SVR public key from new PIN
    pub async fn change_pin_backup(
        &self,
        old_svr_secret: &[u8; 32],
        new_svr_secret: &[u8; 32],
        identity_keypair: &[u8; 64],
        profile_key: &[u8; 32],
        new_registration_lock: &[u8; 32],
        new_svr_public_key: &[u8; 32],
    ) -> Result<(), SvrBackupError> {
        // 1. Derive old backup ID (proves ownership of current backup)
        let old_backup_id_hash = derive_backup_id(old_svr_secret);

        // 2. Build and encrypt the payload with the NEW secret
        let mut payload = KeyBackupPayload {
            identity_keypair: *identity_keypair,
            profile_key: *profile_key,
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
            .await?;

        // 6. Persist confirmed version locally
        self.settings
            .set(SVR_BACKUP_VERSION_KEY, &confirmed_version)?;

        tracing::info!(
            version = confirmed_version,
            "SVR backup re-encrypted with new PIN successfully"
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
