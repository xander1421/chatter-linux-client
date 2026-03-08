//! Helper functions for sending encrypted content to DMs and channels.
//!
//! Uses MessageService for all message operations - no direct network calls.

use std::sync::Arc;

use slint::Model;

use chatter_linux::domain::ids::ChannelId;
use chatter_linux::domain::UserId;
use chatter_linux::mailbox::group::GroupCoordinator;
use chatter_linux::proto::content_v1::Content;
use chatter_linux::services::{
    build_delivery_receipt_content, build_delivery_receipt_content_with_channel,
    build_read_receipt_content, build_read_receipt_content_with_channel,
    MailboxManager, MessageService,
};

use crate::MainWindow;

/// Look up the peer's user_id from a DM channel_id using the conversations model.
pub fn get_peer_id_from_dm_channel(window: &MainWindow, channel_id: &str) -> Option<String> {
    let conversations = window.get_dm_conversations();
    let count = conversations.row_count();
    tracing::debug!(
        channel_id = %channel_id,
        conversation_count = count,
        "get_peer_id_from_dm_channel: Looking up peer"
    );

    for i in 0..count {
        if let Some(item) = conversations.row_data(i) {
            let item_channel_id = item.channel_id.as_str();
            let item_user_id = item.user_id.as_str();
            if item_channel_id == channel_id {
                tracing::debug!(
                    found_peer_id = %item_user_id,
                    "get_peer_id_from_dm_channel: Found matching peer"
                );
                return Some(item_user_id.to_string());
            }
        }
    }

    tracing::debug!(
        channel_id = %channel_id,
        "get_peer_id_from_dm_channel: No matching peer found"
    );
    None
}

/// Look up the DM channel_id from a peer's user_id using the conversations model.
pub fn get_dm_channel_from_peer(window: &MainWindow, peer_id: &str) -> Option<String> {
    let conversations = window.get_dm_conversations();
    let count = conversations.row_count();
    tracing::debug!(
        peer_id = %peer_id,
        conversation_count = count,
        "get_dm_channel_from_peer: Looking up channel"
    );

    for i in 0..count {
        if let Some(item) = conversations.row_data(i) {
            let item_user_id = item.user_id.as_str();
            let item_channel_id = item.channel_id.as_str();
            if item_user_id == peer_id {
                tracing::debug!(
                    found_channel_id = %item_channel_id,
                    "get_dm_channel_from_peer: Found matching channel"
                );
                return Some(item_channel_id.to_string());
            }
        }
    }

    tracing::debug!(
        peer_id = %peer_id,
        "get_dm_channel_from_peer: No matching channel found"
    );
    None
}

/// Send any Content proto to a DM peer via MessageService.
pub async fn send_dm_content(
    content: Content,
    peer_id: &UserId,
    message_service: Arc<MessageService>,
    mailbox_manager: Arc<MailboxManager>,
) -> Result<(), String> {
    message_service
        .send_dm(peer_id, content, &mailbox_manager)
        .await
        .map_err(|e| e.to_string())
}

/// Send read receipts for messages in a DM conversation.
pub fn send_dm_read_receipts(
    peer_id: UserId,
    message_timestamps: Vec<u64>,
    message_service: Arc<MessageService>,
    mailbox_manager: Arc<MailboxManager>,
    _runtime_handle: tokio::runtime::Handle,
) {
    if message_timestamps.is_empty() {
        tracing::debug!("READ_RECEIPT_SEND: No timestamps to send");
        return;
    }

    tracing::info!(
        peer_id = %peer_id,
        count = message_timestamps.len(),
        timestamps = ?message_timestamps,
        "READ_RECEIPT_SEND: Sending read receipts for DM"
    );

    tokio::spawn(async move {
        let content = build_read_receipt_content(message_timestamps.clone());
        if let Err(e) = send_dm_content(content, &peer_id, message_service, mailbox_manager).await {
            tracing::warn!(
                peer_id = %peer_id,
                error = %e,
                "READ_RECEIPT_SEND: Failed to send read receipts"
            );
        } else {
            tracing::info!(
                peer_id = %peer_id,
                "READ_RECEIPT_SEND: Successfully sent read receipts"
            );
        }
    });
}

/// Send delivery receipt for a received DM message.
pub fn send_dm_delivery_receipt(
    peer_id: UserId,
    message_timestamp: u64,
    message_service: Arc<MessageService>,
    mailbox_manager: Arc<MailboxManager>,
    _runtime_handle: tokio::runtime::Handle,
) {
    tokio::spawn(async move {
        let content = build_delivery_receipt_content(vec![message_timestamp]);
        if let Err(e) = send_dm_content(content, &peer_id, message_service, mailbox_manager).await {
            tracing::warn!("Failed to send delivery receipt: {}", e);
        }
    });
}

/// Send encrypted Content proto to a channel via sender key.
pub async fn send_channel_content(
    content: Content,
    channel_id: &ChannelId,
    message_service: Arc<MessageService>,
    _group_coordinator: Arc<GroupCoordinator>,
) -> Result<(), String> {
    message_service
        .send_control(channel_id, content, None)
        .await
        .map_err(|e| format!("Failed to send channel content: {}", e))
}

/// Send read receipts for messages in a channel conversation.
pub fn send_channel_read_receipts(
    channel_id: ChannelId,
    message_timestamps: Vec<u64>,
    message_service: Arc<MessageService>,
    group_coordinator: Arc<GroupCoordinator>,
    _runtime_handle: tokio::runtime::Handle,
) {
    if message_timestamps.is_empty() {
        return;
    }

    tokio::spawn(async move {
        let content = build_read_receipt_content_with_channel(
            message_timestamps.clone(),
            Some(&channel_id.to_string()),
        );
        let _ = send_channel_content(content, &channel_id, message_service, group_coordinator).await;
    });
}

/// Send delivery receipt for a received channel message.
pub fn send_channel_delivery_receipt(
    channel_id: ChannelId,
    message_timestamp: u64,
    message_service: Arc<MessageService>,
    group_coordinator: Arc<GroupCoordinator>,
    _runtime_handle: tokio::runtime::Handle,
) {
    tokio::spawn(async move {
        let content = build_delivery_receipt_content_with_channel(
            vec![message_timestamp],
            Some(&channel_id.to_string()),
        );
        let _ = send_channel_content(content, &channel_id, message_service, group_coordinator).await;
    });
}
