//! DM Bootstrap service for managing DM channel establishment and session recovery.

use std::sync::Arc;

use crate::domain::channel::Channel;
use crate::domain::ids::UserId;
use crate::error::StorageError;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::proto::chat_v1::channel::ChannelMember;
use crate::services::channel::converters::proto_to_domain_membership;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::storage::repositories::channel_repo::ChannelRepository;
#[derive(Debug, thiserror::Error)]
pub enum DmBootstrapError {
    #[error("session error: {0}")]
    Session(String),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("channel error: {0}")]
    Channel(String),
}

/// Service for DM channel establishment and session recovery.
///
/// Consolidates DM bootstrap logic from UI handlers into a dedicated service layer,
/// ensuring consistent handling of:
/// - Session recovery after decryption failures
/// - Bootstrap completion and pending message sending
/// - Channel/membership persistence for new DM channels
pub struct DmBootstrapService {
    message_service: Arc<MessageService>,
    channel_repo: Arc<ChannelRepository>,
    grpc_pool: Arc<GrpcClientPool>,
    mailbox_manager: Arc<MailboxManager>,
}

impl DmBootstrapService {
    pub fn new(
        message_service: Arc<MessageService>,
        channel_repo: Arc<ChannelRepository>,
        grpc_pool: Arc<GrpcClientPool>,
        mailbox_manager: Arc<MailboxManager>,
    ) -> Self {
        Self {
            message_service,
            channel_repo,
            grpc_pool,
            mailbox_manager,
        }
    }

    /// Handle session convergence failure by resetting and re-establishing session.
    pub fn handle_convergence_failure(&self, peer_id: UserId) {
        let message_service = self.message_service.clone();
        let grpc_pool = self.grpc_pool.clone();

        tokio::spawn(async move {
            let mut auth_client = AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            // Reset session and establish fresh one (device_id = 1 for primary)
            match message_service
                .reset_and_establish_session(&mut auth_client, &peer_id, 1)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        peer_id = %peer_id,
                        "Session reset complete - ready for fresh messages"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "Failed to reset session after convergence failure"
                    );
                }
            }
        });
    }

    /// Handle DM bootstrap completion by sending any pending messages.
    pub fn handle_bootstrap_completed(&self, peer_id: UserId) {
        let message_service = self.message_service.clone();
        let mailbox_manager = self.mailbox_manager.clone();
        let grpc_pool = self.grpc_pool.clone();

        tracing::info!(
            peer_id = %peer_id,
            "BOOTSTRAP[4/4] handle_dm_bootstrap_completed called - spawning task to send pending"
        );

        tokio::spawn(async move {
            tracing::debug!(
                peer_id = %peer_id,
                "BOOTSTRAP[4/4] Creating auth client for pending message send"
            );

            let mut auth_client = AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            tracing::info!(
                peer_id = %peer_id,
                "BOOTSTRAP[4/4] Calling send_pending_messages (initiator side)"
            );

            match message_service
                .send_pending_messages(&mut auth_client, &peer_id, &mailbox_manager)
                .await
            {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(
                            peer_id = %peer_id,
                            count = count,
                            "BOOTSTRAP[4/4] SUCCESS - Sent {} pending message(s) after bootstrap!",
                            count
                        );
                    } else {
                        tracing::info!(
                            peer_id = %peer_id,
                            "BOOTSTRAP[4/4] No pending messages to send (initiator side)"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "BOOTSTRAP[4/4] FAILED - Could not send pending messages"
                    );
                }
            }
        });
    }

    /// Save a DM channel to local storage if it doesn't already exist.
    pub fn save_dm_channel(&self, channel: &Channel) -> Result<(), DmBootstrapError> {
        self.channel_repo.insert_or_ignore(channel)?;
        Ok(())
    }

    /// Sync DM memberships to local storage.
    pub fn sync_dm_memberships(
        &self,
        channel: &Channel,
        proto_members: &[ChannelMember],
    ) -> Result<(), DmBootstrapError> {
        for proto_member in proto_members {
            match proto_to_domain_membership(&channel.id, proto_member) {
                Ok(membership) => {
                    if let Err(e) = self.channel_repo.insert_or_ignore_membership(&membership) {
                        tracing::warn!(
                            channel_id = %channel.id,
                            user_id = %membership.user_id,
                            error = %e,
                            "Failed to sync DM membership"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        channel_id = %channel.id,
                        error = %e,
                        "Failed to convert proto DM membership"
                    );
                }
            }
        }
        Ok(())
    }
}
