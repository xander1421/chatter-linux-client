//! DM Bootstrap service for managing DM channel establishment and session recovery.

use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::Mutex;

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
    /// Shared database handle for spawning bootstrap event handler.
    ///
    /// TODO: Refactor `spawn_dm_bootstrap_handler_from_bus` to take pre-built
    /// services (SignalService, KeyStore) instead of raw Database, then remove this.
    database: Arc<crate::storage::Database>,
    /// Temporary storage for messages that need to be shown in UI during bootstrap
    pending_ui_messages: Arc<Mutex<HashMap<UserId, Vec<String>>>>,
}

impl DmBootstrapService {
    pub fn new(
        message_service: Arc<MessageService>,
        channel_repo: Arc<ChannelRepository>,
        grpc_pool: Arc<GrpcClientPool>,
        mailbox_manager: Arc<MailboxManager>,
        database: Arc<crate::storage::Database>,
    ) -> Self {
        Self {
            message_service,
            channel_repo,
            grpc_pool,
            mailbox_manager,
            database,
            pending_ui_messages: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawn the DM bootstrap event handler.
    ///
    /// Listens for domain events (e.g., incoming prekey messages) and handles
    /// session establishment. Returns a `JoinHandle` for the background task.
    pub fn spawn_handler(&self, event_bus: &crate::events::EventBus) -> tokio::task::JoinHandle<()> {
        crate::inbox::spawn_dm_bootstrap_handler_from_bus(
            self.database.clone(),
            self.grpc_pool.clone(),
            event_bus,
        )
    }
    
    /// Store a message to be shown in UI during bootstrap
    pub async fn store_pending_ui_message(&self, peer_id: &UserId, message: String) {
        let mut pending = self.pending_ui_messages.lock().await;
        pending.entry(*peer_id).or_insert_with(Vec::new).push(message);
    }
    
    /// Take and clear pending UI messages for a peer
    pub async fn take_pending_messages(&self, peer_id: &UserId) -> Vec<String> {
        let mut pending = self.pending_ui_messages.lock().await;
        pending.remove(peer_id).unwrap_or_default()
    }

    /// Flush pending UI messages for a peer by sending them as DMs.
    ///
    /// Called after DM bootstrap completes on the responder side.
    /// Builds Content protos and sends via Double Ratchet.
    pub async fn flush_pending_ui_messages(
        &self,
        peer_id: &UserId,
    ) {
        let pending = self.take_pending_messages(peer_id).await;
        if pending.is_empty() {
            return;
        }

        tracing::info!(
            peer_id = %peer_id,
            count = pending.len(),
            "Flushing pending UI messages after bootstrap"
        );

        let msg_svc = self.message_service.clone();
        let mailbox = self.mailbox_manager.clone();
        let peer = *peer_id;
        tokio::spawn(async move {
            for text in pending {
                let now = chrono::Utc::now().timestamp_millis() as u64;
                let content = crate::services::gateway::build_text_content(&text, now);
                if let Err(e) = msg_svc.send_dm(&peer, content, &*mailbox).await {
                    tracing::warn!(peer_id = %peer, error = %e, "Failed to send pending message");
                }
            }
        });
    }

    /// Handle session convergence failure by resetting and re-establishing sessions.
    ///
    /// Multi-device: resets sessions for ALL peer devices, not just primary.
    pub fn handle_convergence_failure(&self, peer_id: UserId) {
        let message_service = self.message_service.clone();
        let grpc_pool = self.grpc_pool.clone();

        tokio::spawn(async move {
            let mut auth_client = AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            // Reset sessions and establish fresh ones for ALL peer devices
            match message_service
                .reset_and_establish_sessions_all_devices(&mut auth_client, &peer_id)
                .await
            {
                Ok(device_ids) => {
                    tracing::info!(
                        peer_id = %peer_id,
                        device_count = device_ids.len(),
                        "Session reset complete for all devices - ready for fresh messages"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "Failed to reset sessions after convergence failure"
                    );
                }
            }
        });
    }

    /// Handle mailbox expiration by triggering a re-bootstrap.
    ///
    /// This is part of the NDSS '21 healing path. If our mailbox is gone
    /// or expired, we must establish a new one and inform the peer.
    pub fn handle_mailbox_expired(&self, peer_id: UserId) {
        let message_service = self.message_service.clone();
        let mailbox_manager = self.mailbox_manager.clone();
        let grpc_pool = self.grpc_pool.clone();

        tracing::info!(
            peer_id = %peer_id,
            "HEALING: DM mailbox expired, triggering re-bootstrap"
        );

        tokio::spawn(async move {
            let mut auth_client = AuthClient::new(
                grpc_pool.channel(),
                grpc_pool.auth_clone(),
            );

            // Initiate bootstrap - this will create a new mailbox and tell the peer.
            // We send an empty content proto which just triggers the bootstrap flow.
            let dummy_content = crate::proto::content_v1::Content {
                content: None,
                timestamp: crate::util::time::TimestampNanos::now().as_u64(),
                ephemeral_reply_to: None,
                padding: vec![],
            };

            match message_service
                .send_dm_content(crate::services::message::SendDmContentParams {
                    auth_client: &mut auth_client,
                    peer_id: &peer_id,
                    content: dummy_content,
                    mailbox_manager: &mailbox_manager,
                    grpc_pool: Some(&grpc_pool),
                    attachment_ids: None,
                    channel_id: None,
                })
                .await
            {
                Ok(_) => {
                    tracing::info!(peer_id = %peer_id, "HEALING: Re-bootstrap triggered successfully");
                }
                Err(crate::services::message::MessageError::BootstrapInitiated) => {
                    tracing::info!(peer_id = %peer_id, "HEALING: Re-bootstrap initiated (message queued)");
                }
                Err(e) => {
                    tracing::error!(
                        peer_id = %peer_id,
                        error = %e,
                        "HEALING: Failed to trigger re-bootstrap for expired mailbox"
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
