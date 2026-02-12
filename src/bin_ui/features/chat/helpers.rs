//! Helper functions for sending encrypted content to DMs and channels.

use std::sync::Arc;

use chatter_linux::domain::ids::ChannelId;
use chatter_linux::domain::UserId;
use chatter_linux::network::grpc::auth_client::AuthClient;
use chatter_linux::network::grpc::channel_client::ChannelClient;
use chatter_linux::network::grpc::message_client::MessageClient;
use chatter_linux::network::grpc::GrpcClientPool;
use chatter_linux::proto::content_v1::Content;
use chatter_linux::services::{build_read_receipt_content, MailboxManager, MessageService};
use chatter_linux::storage::Database;

/// Send any Content proto to a DM peer via MessageService (two-layer encryption).
///
/// Uses MessageService::send_dm_content() which handles:
/// - Layer 1: Signal Protocol (forward secrecy via Double Ratchet)
/// - Layer 2: Mailbox encryption (sender anonymity via XChaCha20-Poly1305)
pub async fn send_dm_content(
    content: Content,
    peer_id: &UserId,
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    message_service: Arc<MessageService>,
    mailbox_manager: Arc<MailboxManager>,
) -> Result<(), String> {
    let mut auth_client = AuthClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    message_service
        .send_dm_content(
            &mut auth_client,
            peer_id,
            content,
            database,
            &mailbox_manager,
            Some(&grpc_pool),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Send read receipts for messages in a DM conversation.
///
/// Call this when a DM conversation is opened to mark messages as read.
pub fn send_dm_read_receipts(
    peer_id: UserId,
    message_timestamps: Vec<u64>,
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    message_service: Arc<MessageService>,
    mailbox_manager: Arc<MailboxManager>,
) {
    if message_timestamps.is_empty() {
        return;
    }

    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create runtime for read receipt");

        rt.block_on(async {
            let content = build_read_receipt_content(message_timestamps.clone());

            if let Err(e) = send_dm_content(
                content,
                &peer_id,
                database,
                grpc_pool,
                message_service,
                mailbox_manager,
            )
            .await
            {
                tracing::debug!("Failed to send read receipts: {}", e);
            } else {
                tracing::debug!(
                    "Sent read receipts for {} messages to peer {}",
                    message_timestamps.len(),
                    peer_id
                );
            }
        });
    });
}

/// Send encrypted Content proto to a channel via sender key.
///
/// Used for control messages (typing, reactions, edits, etc.) in encrypted channels.
pub async fn send_channel_content(
    content: Content,
    channel_id: &ChannelId,
    message_service: Arc<MessageService>,
    grpc_pool: Arc<GrpcClientPool>,
) -> Result<(), String> {
    let mut message_client = MessageClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
    let mut auth_client = AuthClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
    let mut channel_client = ChannelClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

    message_service
        .send_control_content(
            &mut message_client,
            &mut auth_client,
            &mut channel_client,
            channel_id,
            content,
        )
        .await
        .map_err(|e| format!("Failed to send channel content: {}", e))
}
