//! Account management service.
//!
//! Password change, account deletion.

use std::sync::Arc;
use tokio_stream::StreamExt;

use crate::error::{CryptoError, NetworkError, StorageError};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::account_deletion_event::Status as DeletionStatus;
use crate::services::auth_service::{change_password_with, prove_password, AuthError};
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

    /// Password-proof cryptography failed.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
}

impl From<AuthError> for AccountError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::InvalidCredentials | AuthError::Network(NetworkError::Unauthenticated) => {
                AccountError::InvalidPassword
            }
            AuthError::Network(n) => AccountError::Network(n),
            AuthError::Crypto(c) => AccountError::Crypto(c),
            AuthError::Storage(s) => AccountError::Storage(s),
            AuthError::NotLoggedIn => AccountError::NotLoggedIn,
            other => AccountError::Crypto(CryptoError::Protocol(other.to_string())),
        }
    }
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
        self.grpc_pool.auth_client()
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
    /// 2. OPAQUE proof of the current password + OPAQUE registration of the
    ///    new one (`auth_service::change_password_with`)
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

        change_password_with(auth_client, current_password, new_password).await?;
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

        // A fresh OPAQUE proof of the password, spent by DeleteAccount.
        let proof = prove_password(auth_client, password).await?;

        // Call server to delete account - returns event stream
        let stream = auth_client
            .delete_account(proof)
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
