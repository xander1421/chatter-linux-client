//! Helper functions for sending encrypted content to DMs and channels.
//!
//! Uses MessageService for all message operations - no direct network calls.

use std::sync::Arc;

use slint::Model;

use chatter_linux::domain::ids::ChannelId;
use chatter_linux::domain::UserId;
use chatter_linux::mailbox::group::GroupFanout;
use chatter_linux::proto::content_v1::Content;
use chatter_linux::services::{build_read_receipt_content, MailboxManager, MessageService};
use chatter_linux::storage::Database;

use crate::MainWindow;

/// Look up the peer's user_id from a DM channel_id using the conversations model.
///
/// Returns None if the channel_id is not found in the DM conversations list.
pub fn get_peer_id_from_dm_channel(window: &MainWindow, channel_id: &str) -> Option<String> {
    let conversations = window.get_dm_conversations();
    for i in 0..conversations.row_count() {
        if let Some(item) = conversations.row_data(i) {
            if item.channel_id.as_str() == channel_id {
                return Some(item.user_id.to_string());
            }
        }
    }
    None
}

/// Look up the DM channel_id from a peer's user_id using the conversations model.
///
/// Returns None if the peer is not found in the DM conversations list.
pub fn get_dm_channel_from_peer(window: &MainWindow, peer_id: &str) -> Option<String> {
    let conversations = window.get_dm_conversations();
    let count = conversations.row_count();

    tracing::debug!(
        peer_id = %peer_id,
        dm_count = count,
        "get_dm_channel_from_peer: searching conversations"
    );

    for i in 0..count {
        if let Some(item) = conversations.row_data(i) {
            if item.user_id.as_str() == peer_id {
                let channel_id = item.channel_id.to_string();
                tracing::debug!(
                    peer_id = %peer_id,
                    channel_id = %channel_id,
                    "get_dm_channel_from_peer: FOUND match at index {}",
                    i
                );
                return Some(channel_id);
            }
        }
    }

    // Log all user_ids in the model for debugging
    let user_ids: Vec<String> = (0..count)
        .filter_map(|i| conversations.row_data(i).map(|item| item.user_id.to_string()))
        .collect();
    tracing::warn!(
        peer_id = %peer_id,
        dm_count = count,
        user_ids = ?user_ids,
        "get_dm_channel_from_peer: peer NOT FOUND in conversations model"
    );

    None
}

/// Send any Content proto to a DM peer via MessageService (two-layer encryption).
///
/// Uses MessageService::send_dm() which handles:
/// - Layer 1: Signal Protocol (forward secrecy via Double Ratchet)
/// - Layer 2: Mailbox encryption (sender anonymity via XChaCha20-Poly1305)
pub async fn send_dm_content(
    content: Content,
    peer_id: &UserId,
    database: Arc<Database>,
    message_service: Arc<MessageService>,
    mailbox_manager: Arc<MailboxManager>,
) -> Result<(), String> {
    message_service
        .send_dm(peer_id, content, database, &mailbox_manager)
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
/// When `group_fanout` is provided, also sends via member mailboxes (NDSS '21 compliance).
pub async fn send_channel_content(
    content: Content,
    channel_id: &ChannelId,
    message_service: Arc<MessageService>,
    group_fanout: Option<Arc<GroupFanout>>,
) -> Result<(), String> {
    message_service
        .send_control(channel_id, content, group_fanout.as_deref())
        .await
        .map_err(|e| format!("Failed to send channel content: {}", e))
}
