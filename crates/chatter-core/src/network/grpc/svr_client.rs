//! SVR (Secure Value Recovery) gRPC client.
//!
//! Client for SVR backup service operations including store, fetch, and delete.

use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    svr_service_client::SvrServiceClient, ChangePinBackupRequest, DeleteBackupRequest,
    FetchBackupRequest, StoreBackupRequest,
};

/// Result of a successful backup fetch.
pub struct FetchBackupResult {
    /// The encrypted backup ciphertext (nonce || ciphertext || tag).
    pub encrypted_backup: Vec<u8>,
    /// Backup version stored on server.
    pub version: u32,
    /// How many fetch attempts remain before server-side lockout.
    pub remaining_attempts: i32,
}

/// SVR backup service client wrapper.
///
/// Wraps the generated SvrServiceClient with ergonomic APIs and error mapping.
/// Used for encrypted key backup lifecycle: store, fetch, delete.
pub struct SvrClient {
    inner: SvrServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl SvrClient {
    /// Create a new SvrClient from pool components.
    ///
    /// # Arguments
    ///
    /// * `channel` - Cloned channel from GrpcClientPool
    /// * `interceptor` - Cloned auth interceptor for credential injection
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: SvrServiceClient::with_interceptor(channel, interceptor),
        }
    }

    /// Upload an encrypted backup to the server.
    ///
    /// Upserts the backup: creates if new, updates if exists. The server
    /// validates that the version is monotonically increasing.
    ///
    /// # Arguments
    ///
    /// * `encrypted_backup` - AES-256-GCM ciphertext (nonce || ciphertext || tag)
    /// * `backup_id_hash` - HMAC-SHA256(svr_secret, "BackupId") 32-byte identifier
    /// * `version` - Monotonic version counter to prevent replay attacks
    ///
    /// # Returns
    ///
    /// The version confirmed by the server.
    pub async fn store_backup(
        &mut self,
        encrypted_backup: Vec<u8>,
        backup_id_hash: Vec<u8>,
        version: u32,
    ) -> Result<u32, NetworkError> {
        let request = StoreBackupRequest {
            encrypted_backup,
            backup_id_hash,
            version,
        };

        self.inner
            .store_backup(request)
            .await
            .map(|r| r.into_inner().version)
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Fetch an encrypted backup from the server.
    ///
    /// Rate-limited server-side to prevent PIN brute-force attacks.
    /// Returns the encrypted ciphertext, version, and remaining attempts.
    ///
    /// # Arguments
    ///
    /// * `backup_id_hash` - HMAC-SHA256(svr_secret, "BackupId") 32-byte identifier
    pub async fn fetch_backup(
        &mut self,
        backup_id_hash: Vec<u8>,
    ) -> Result<FetchBackupResult, NetworkError> {
        let request = FetchBackupRequest { backup_id_hash };

        let response = self
            .inner
            .fetch_backup(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(FetchBackupResult {
            encrypted_backup: response.encrypted_backup,
            version: response.version,
            remaining_attempts: response.remaining_attempts,
        })
    }

    /// Delete an encrypted backup from the server.
    ///
    /// Permanently removes the backup identified by the given hash.
    /// This is irreversible.
    ///
    /// # Arguments
    ///
    /// * `backup_id_hash` - HMAC-SHA256(svr_secret, "BackupId") 32-byte identifier
    pub async fn delete_backup(&mut self, backup_id_hash: Vec<u8>) -> Result<(), NetworkError> {
        let request = DeleteBackupRequest { backup_id_hash };

        self.inner
            .delete_backup(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Atomically re-encrypt a backup with a new PIN-derived key.
    ///
    /// The server verifies that the `old_backup_id_hash` matches the current
    /// backup before accepting the change. On success, the server replaces the
    /// backup with the new ciphertext and updates the registration lock and
    /// SVR public key.
    ///
    /// # Arguments
    ///
    /// * `encrypted_backup` - New AES-256-GCM ciphertext (nonce || ciphertext || tag)
    /// * `new_backup_id_hash` - HMAC-SHA256(new_svr_secret, "BackupId")
    /// * `old_backup_id_hash` - HMAC-SHA256(old_svr_secret, "BackupId")
    /// * `version` - Monotonic version counter (must be > current)
    /// * `new_registration_lock` - Registration lock derived from new PIN
    /// * `new_svr_public_key` - SVR public key derived from new PIN
    ///
    /// # Returns
    ///
    /// The version confirmed by the server.
    pub async fn change_pin_backup(
        &mut self,
        encrypted_backup: Vec<u8>,
        new_backup_id_hash: Vec<u8>,
        old_backup_id_hash: Vec<u8>,
        version: u32,
        new_registration_lock: Vec<u8>,
        new_svr_public_key: Vec<u8>,
    ) -> Result<u32, NetworkError> {
        let request = ChangePinBackupRequest {
            encrypted_backup,
            new_backup_id_hash,
            old_backup_id_hash,
            version,
            new_registration_lock,
            new_svr_public_key,
        };

        self.inner
            .change_pin_backup(request)
            .await
            .map(|r| r.into_inner().version)
            .map_err(|s| NetworkError::from_status(&s))
    }
}
