//! DM Bootstrap service for managing DM channel establishment and session recovery.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::domain::ids::UserId;
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;

/// Service for DM channel establishment and session recovery.
///
/// Consolidates DM bootstrap logic from UI handlers into a dedicated service layer,
/// ensuring consistent handling of:
/// - Session recovery after decryption failures
/// - Bootstrap completion and pending message sending
pub struct DmBootstrapService {
    message_service: Arc<MessageService>,
    grpc_pool: Arc<GrpcClientPool>,
    mailbox_manager: Arc<MailboxManager>,
    /// When each peer's sessions were last reset.
    last_reset: Mutex<HashMap<UserId, Instant>>,
}

/// Signal-Desktop's `lightSessionReset` bound: one reset per peer per hour.
const SESSION_RESET_INTERVAL: Duration = Duration::from_secs(60 * 60);

impl DmBootstrapService {
    pub fn new(
        message_service: Arc<MessageService>,
        grpc_pool: Arc<GrpcClientPool>,
        mailbox_manager: Arc<MailboxManager>,
    ) -> Self {
        Self {
            message_service,
            grpc_pool,
            mailbox_manager,
            last_reset: Mutex::new(HashMap::new()),
        }
    }

    /// Reset the sessions with every device of `peer_id` after a session
    /// desync, at most once per peer per [`SESSION_RESET_INTERVAL`]: every
    /// envelope under a broken session fails the same way, and each reset
    /// fetches a prekey bundle per device.
    pub fn reset_peer_sessions(&self, peer_id: UserId) {
        {
            let mut last_reset = self.last_reset.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if last_reset
                .get(&peer_id)
                .is_some_and(|at| now.duration_since(*at) < SESSION_RESET_INTERVAL)
            {
                tracing::warn!(peer_id = %peer_id, "Skipping session reset: peer was reset within the hour");
                return;
            }
            last_reset.insert(peer_id, now);
        }
        let message_service = self.message_service.clone();
        let grpc_pool = self.grpc_pool.clone();

        tokio::spawn(async move {
            let mut auth_client = AuthClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

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
                        "Failed to reset sessions with peer"
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
            let mut auth_client = AuthClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            // Initiate bootstrap - this will create a new mailbox and tell the peer.
            // The content only triggers the bootstrap flow; it is queued and
            // delivered once the mailbox is up, so it is a NullMessage, which
            // the receiver acks and drops (an empty Content parses as an error
            // and becomes a delivery-issue marker).
            let dummy_content = crate::services::message::null_message_content();

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
    ///
    /// The flush runs in the background, so its outcome is announced on the
    /// bus (`PendingFlush::emit`): `DmBootstrapCompleted` for the messages
    /// that went out, `MessageSendFailed` for those that did not.
    pub fn handle_bootstrap_completed(&self, peer_id: UserId, event_bus: crate::events::EventBus) {
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

            let mut auth_client = AuthClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

            tracing::info!(
                peer_id = %peer_id,
                "BOOTSTRAP[4/4] Calling send_pending_messages (initiator side)"
            );

            let flush = message_service
                .send_pending_messages(&mut auth_client, &peer_id, &mailbox_manager)
                .await;
            tracing::info!(
                peer_id = %peer_id,
                sent = flush.sent.len(),
                failed = flush.failed.len(),
                error = ?flush.error,
                "BOOTSTRAP[4/4] pending messages flushed (initiator side)"
            );
            flush.emit(peer_id, &event_bus);
        });
    }
}
