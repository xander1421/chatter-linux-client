//! Account management service.
//!
//! Password change, account deletion.

use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio_stream::StreamExt;

use crate::error::{NetworkError, StorageError};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::account_deletion_event::Status as DeletionStatus;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// Account management errors.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    /// Invalid current password provided.
    #[error("invalid password")]
    InvalidPassword,

    /// Operation requires user to be logged in.
    #[error("not logged in")]
    NotLoggedIn,

    /// Account deletion failed.
    #[error("deletion failed: {0}")]
    DeletionFailed(String),

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Account management service.
///
/// Orchestrates password change and account deletion flows by coordinating
/// network, and storage layers.
pub struct AccountService {
    grpc_pool: Arc<GrpcClientPool>,
    settings_repo: Arc<SettingsRepository>,
    auth_interceptor: AuthInterceptor,
}

impl AccountService {
    /// Create a new AccountService with all dependencies.
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        settings_repo: Arc<SettingsRepository>,
        auth_interceptor: AuthInterceptor,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            auth_interceptor,
        }
    }

    /// Internal helper to create an auth client.
    fn auth_client(&self) -> AuthClient {
        AuthClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
    }

    /// Check if a user is currently logged in.
    fn is_logged_in(&self) -> bool {
        self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .ok()
            .flatten()
            .is_some()
    }

    /// Change the current user's password.
    ///
    /// Orchestrates:
    /// 1. Login state verification
    /// 2. Password hashing (SHA256 - server expects pre-hashed)
    /// 3. Server password change request
    ///
    /// # Arguments
    ///
    /// * `auth_client` - Mutable reference to auth client for server communication
    /// * `current_password` - Current password for verification
    /// * `new_password` - New password to set
    pub async fn change_password(
        &self,
        auth_client: &mut AuthClient,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), AccountError> {
        // Check is logged in
        if !self.is_logged_in() {
            return Err(AccountError::NotLoggedIn);
        }

        // Hash both passwords with SHA256 (server expects pre-hashed)
        let current_hash = Sha256::digest(current_password.as_bytes());
        let new_hash = Sha256::digest(new_password.as_bytes());

        // Call server to change password
        auth_client
            .change_password(current_hash.as_slice(), new_hash.as_slice())
            .await
            .map_err(|e| match &e {
                NetworkError::Unauthenticated => AccountError::InvalidPassword,
                _ => AccountError::Network(e),
            })?;

        Ok(())
    }

    /// Delete the current user's account.
    ///
    /// This is an irreversible operation that:
    /// 1. Verifies login state
    /// 2. Sends deletion request to server
    /// 3. Consumes deletion event stream for completion/error
    /// 4. Clears local auth state on success
    ///
    /// # Arguments
    ///
    /// * `auth_client` - Mutable reference to auth client for server communication
    /// * `password` - Password for confirmation
    pub async fn delete_account(
        &self,
        auth_client: &mut AuthClient,
        password: &str,
    ) -> Result<(), AccountError> {
        // Check is logged in
        if !self.is_logged_in() {
            return Err(AccountError::NotLoggedIn);
        }

        // Hash password with SHA256 (server expects pre-hashed)
        let password_hash = Sha256::digest(password.as_bytes());

        // Call server to delete account - returns event stream
        let stream = auth_client
            .delete_account(password_hash.as_slice())
            .await
            .map_err(|e| match &e {
                NetworkError::Unauthenticated => AccountError::InvalidPassword,
                _ => AccountError::Network(e),
            })?;

        tokio::pin!(stream);

        // Consume deletion event stream
        while let Some(result) = stream.next().await {
            let event = result?;

            match DeletionStatus::try_from(event.status) {
                Ok(DeletionStatus::Completed) => {
                    // Success - clear local auth state
                    self.auth_interceptor.clear();
                    self.settings_repo.delete(keys::CURRENT_USER_ID)?;
                    self.settings_repo.delete(keys::CURRENT_DEVICE_ID)?;
                    return Ok(());
                }
                Ok(DeletionStatus::Failed) => {
                    // Deletion failed - return error with message
                    let message = event
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| event.message);
                    return Err(AccountError::DeletionFailed(message));
                }
                Ok(DeletionStatus::Started)
                | Ok(DeletionStatus::DataRemovalInProgress)
                | Ok(DeletionStatus::SpicedbCleanupInProgress) => {
                    // Progress events - continue waiting
                }
                Ok(DeletionStatus::Unspecified) | Err(_) => {
                    // Unknown status - continue waiting
                }
            }
        }

        // Stream ended without completion event
        Err(AccountError::DeletionFailed(
            "deletion stream ended without result".to_string(),
        ))
    }

    // ==================== Simplified Wrappers ====================

    /// Simplified wrapper for change_password that creates the client internally.
    pub async fn change_pwd(
        &self,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), AccountError> {
        let mut auth_client = self.auth_client();
        self.change_password(&mut auth_client, current_password, new_password)
            .await
    }

    /// Simplified wrapper for delete_account that creates the client internally.
    pub async fn delete_acct(&self, password: &str) -> Result<(), AccountError> {
        let mut auth_client = self.auth_client();
        self.delete_account(&mut auth_client, password).await
    }
}
