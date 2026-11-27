//! Account management service.
//!
//! Password change, account deletion.

use std::sync::Arc;
use tokio_stream::StreamExt;

use crate::error::{CryptoError, NetworkError, StorageError};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::interceptors::AuthInterceptor;
use chatter_proto::chat_v1::account_deletion_event::Status as DeletionStatus;
use crate::services::auth_service::{AuthError, prove_password};
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};

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

    /// We could not leave this many private channels, so the account was not
    /// deleted: it would stay in their signed rosters.
    #[error("could not leave {0} private channel(s); the account was not deleted")]
    LeaveFailed(usize),

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

    /// Simplified wrapper for delete_account that creates the client internally.
    pub async fn delete_acct(&self, password: &str) -> Result<(), AccountError> {
        let mut auth_client = self.auth_client();
        self.delete_account(&mut auth_client, password).await
    }

    /// Check the password with a proof of its own, so a wrong one stops
    /// account deletion before anything else (leaving channels) is done.
    pub async fn verify_password(&self, password: &str) -> Result<(), AccountError> {
        let mut auth_client = self.auth_client();
        let proof = prove_password(&mut auth_client, password).await?;
        auth_client.verify_password(proof).await.map_err(|e| match e {
            NetworkError::Unauthenticated | NetworkError::PermissionDenied => AccountError::InvalidPassword,
            e => AccountError::Network(e),
        })
    }
}

/// The private channels to leave before deleting the account: a signed
/// roster changes only by a member's signature, so without our own LEFT a
/// deleted account stays a member that every post must leave out. Each
/// entry is a private channel with our own entry in its verified roster.
/// An active non-owner leaves; an owner cannot (`verified_state/rules.rs`),
/// and a channel with no verified state, or one we are no longer active in,
/// has nothing we could sign against.
pub(crate) fn channels_to_leave_before_deletion(
    channels: &[(
        crate::domain::ids::ChannelId,
        Option<(crate::domain::verified_state::ChannelRole, crate::domain::verified_state::MemberStatus)>,
    )],
) -> Vec<crate::domain::ids::ChannelId> {
    use crate::domain::verified_state::{ChannelRole, MemberStatus};
    channels
        .iter()
        .filter(|(_, me)| {
            me.is_some_and(|(role, status)| status == MemberStatus::Active && role != ChannelRole::Owner)
        })
        .map(|(id, _)| *id)
        .collect()
}

#[cfg(test)]
mod deletion_tests {
    use super::*;
    use crate::domain::ChannelId;

    #[test]
    fn deletion_leaves_private_channels_it_does_not_own() {
        use crate::domain::verified_state::{ChannelRole, MemberStatus};
        let (joined, owned, left, unverified) =
            (ChannelId::new(), ChannelId::new(), ChannelId::new(), ChannelId::new());
        assert_eq!(
            channels_to_leave_before_deletion(&[
                (joined, Some((ChannelRole::Member, MemberStatus::Active))),
                (owned, Some((ChannelRole::Owner, MemberStatus::Active))),
                (left, Some((ChannelRole::Member, MemberStatus::Invited))),
                (unverified, None),
            ]),
            vec![joined]
        );
    }
}
