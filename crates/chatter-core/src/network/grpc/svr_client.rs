//! SVR (Secure Value Recovery) gRPC client.
//!
//! Client for SVR backup service operations including store, fetch, and delete.

use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    svr_service_client::SvrServiceClient, ChangePinBackupRequest, DeleteBackupRequest,
    DeleteMessageBackupRequest, FetchBackupRequest, GetMessageBackupRequest,
    HasKeyBackupRequest,
    SetMessageBackupRequest, StoreBackupRequest,
};

/// Result of a successful message-backup registration.
pub struct MessageBackupUpload {
    /// Presigned PUT URL — upload the encrypted blob here directly.
    pub upload_url: String,
}

/// Result of a successful message-backup lookup.
pub struct MessageBackupDownload {
    /// Presigned GET URL for the encrypted blob.
    pub download_url: String,
    /// Stored ciphertext size in bytes.
    pub size: u64,
}

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

    /// Whether this account has an SVR key backup AT ALL, independent of any
    /// PIN.
    ///
    /// A failed [`Self::fetch_backup`] cannot tell a wrong PIN from an account
    /// that never had a backup: the backup id is derived from the PIN, so both
    /// simply miss. The two demand opposite responses — retype the PIN, versus
    /// accept a new identity key and lose every existing conversation — so the
    /// client must not guess. This is the bit that decides.
    pub async fn has_key_backup(&mut self) -> Result<bool, NetworkError> {
        self.inner
            .has_key_backup(HasKeyBackupRequest {})
            .await
            .map(|r| r.into_inner().exists)
            .map_err(|s| NetworkError::from_status(&s))
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

    /// Register/replace the encrypted message-history backup.
    ///
    /// Returns a presigned PUT URL; the caller uploads the ciphertext blob
    /// there directly (same model as attachment uploads).
    ///
    /// # Arguments
    ///
    /// * `backup_id_hash` - HMAC-SHA256(svr_secret, "MessageBackupId")
    /// * `size` - ciphertext size in bytes the caller will PUT
    pub async fn set_message_backup(
        &mut self,
        backup_id_hash: Vec<u8>,
        size: u64,
    ) -> Result<MessageBackupUpload, NetworkError> {
        let request = SetMessageBackupRequest {
            backup_id_hash,
            size,
        };

        self.inner
            .set_message_backup(request)
            .await
            .map(|r| MessageBackupUpload {
                upload_url: r.into_inner().upload_url,
            })
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Fetch a presigned GET URL for the stored message-history backup.
    pub async fn get_message_backup(
        &mut self,
        backup_id_hash: Vec<u8>,
    ) -> Result<MessageBackupDownload, NetworkError> {
        let request = GetMessageBackupRequest { backup_id_hash };

        self.inner
            .get_message_backup(request)
            .await
            .map(|r| {
                let r = r.into_inner();
                MessageBackupDownload {
                    download_url: r.download_url,
                    size: r.size,
                }
            })
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Delete the message-history backup (metadata row + stored object).
    pub async fn delete_message_backup(
        &mut self,
        backup_id_hash: Vec<u8>,
    ) -> Result<(), NetworkError> {
        let request = DeleteMessageBackupRequest { backup_id_hash };

        self.inner
            .delete_message_backup(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }
}
