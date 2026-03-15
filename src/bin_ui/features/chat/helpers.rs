//! Helper functions for sending encrypted content to DMs and channels.
//!
//! Uses MessageService for all message operations - no direct network calls.

use std::sync::Arc;

use slint::Model;

use chatter_linux::domain::attachment::Attachment;
use chatter_linux::domain::ids::ChannelId;
use chatter_linux::domain::UserId;
use chatter_linux::mailbox::group::GroupCoordinator;
use chatter_linux::network::grpc::client_pool::GrpcClientPool;
use chatter_linux::proto::content_v1::{AttachmentPointer, Content};
use chatter_linux::services::attachment::AttachmentService;
use chatter_linux::services::{
    build_delivery_receipt_content, build_delivery_receipt_content_with_channel,
    build_read_receipt_content, build_read_receipt_content_with_channel,
    MailboxManager, MessageService,
};
use chatter_linux::state::chat_state::PendingAttachment;

use crate::MainWindow;

/// Look up the peer's user_id from a DM channel_id using the conversations model.
#[allow(dead_code)]
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

/// Parsed reply context from UI strings.
pub struct ReplyContext {
    pub sender_id: UserId,
    pub timestamp: u64,
}

/// Parse reply context from UI string values.
///
/// Returns `None` if either string fails to parse. Logs errors on failure.
pub fn parse_reply_context(sender_str: &str, timestamp_str: &str) -> Option<ReplyContext> {
    let sender_id: UserId = match sender_str.parse() {
        Ok(id) => id,
        Err(_) => {
            tracing::error!("Invalid reply_to_sender: {}", sender_str);
            return None;
        }
    };
    let timestamp: u64 = match timestamp_str.parse() {
        Ok(ts) => ts,
        Err(_) => {
            tracing::error!("Invalid reply_to_timestamp: {}", timestamp_str);
            return None;
        }
    };
    Some(ReplyContext { sender_id, timestamp })
}

/// Build a `MessageItemData` for a sent (own) message.
///
/// Most fields are constant for sent messages — this avoids repeating
/// 28 fields in every send branch.
pub fn build_own_message_item(
    message_id: &str,
    sender_id: &str,
    sender_name: &str,
    text: &str,
    timestamp: &str,
    timestamp_ns: u64,
    slint_attachments: Vec<crate::MessageAttachmentData>,
    is_reply: bool,
    reply_to_sender_name: &str,
    reply_to_preview: &str,
    delivery_status: &str,
) -> crate::MessageItemData {
    use slint::{ModelRc, VecModel};

    crate::MessageItemData {
        message_id: message_id.into(),
        sender_id: sender_id.into(),
        sender_name: sender_name.into(),
        sender_initials: crate::bin_ui::converters::get_initials(sender_name).into(),
        sender_avatar_url: "".into(),
        sender_avatar_source: slint::Image::default(),
        has_avatar: false,
        content: text.into(),
        timestamp: timestamp.into(),
        timestamp_ns: timestamp_ns.to_string().into(),
        is_own: true,
        is_edited: false,
        is_deleted: false,
        is_system: false,
        is_ai: false,
        has_attachments: !slint_attachments.is_empty(),
        attachment_count: slint_attachments.len() as i32,
        attachments: ModelRc::new(VecModel::from(slint_attachments)),
        has_reactions: false,
        reaction_summary: "".into(),
        reactions: ModelRc::default(),
        reply_count: 0,
        reply_count_text: "".into(),
        is_thread_reply: false,
        is_reply,
        reply_to_sender_name: reply_to_sender_name.into(),
        reply_to_preview: reply_to_preview.into(),
        show_sender: true,
        delivery_status: delivery_status.into(),
        is_pinned: false,
        is_call: false,
        call_type: "".into(),
        call_duration: "".into(),
    }
}

/// Append a message to the UI message list, sort by timestamp, and scroll to bottom.
///
/// This is the common pattern after a message is sent: add to model,
/// sort for correct ordering, clear composer, and auto-scroll.
pub fn append_message_to_ui(window: &crate::MainWindow, msg_item: crate::MessageItemData) {
    use slint::{Model, ModelRc, VecModel};

    let model = window.get_messages();
    let mut messages: Vec<_> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect();
    messages.push(msg_item);
    messages.sort_by(|a, b| {
        let ts_a = a.timestamp_ns.parse::<u64>().unwrap_or(0);
        let ts_b = b.timestamp_ns.parse::<u64>().unwrap_or(0);
        ts_a.cmp(&ts_b).then_with(|| a.sender_id.as_str().cmp(b.sender_id.as_str()))
    });
    window.set_messages(ModelRc::new(VecModel::from(messages)));
    window.set_composer_text("".into());
    window.set_message_scroll_position(-99999.0);
    window.set_is_at_bottom(true);
}

/// Result of uploading pending attachments.
pub struct UploadedAttachments {
    /// AttachmentPointer protos for the Content proto.
    pub pointers: Vec<AttachmentPointer>,
    /// Full Attachment domain objects for UI display.
    pub attachments: Vec<Attachment>,
}

/// Upload pending attachments to CDN, returning pointers and display data.
///
/// Handles both pre-uploaded attachments (e.g., GIFs from picker where
/// `pointer` is already set) and new uploads that need CDN encryption.
///
/// This is the shared upload loop used by anon DM, anon group, and
/// other send paths that take pre-uploaded pointers.
pub async fn upload_pending_attachments(
    pending: &[PendingAttachment],
    attachment_service: &AttachmentService,
    grpc_pool: &Arc<GrpcClientPool>,
    context: &str,
) -> UploadedAttachments {
    let mut pointers = Vec::with_capacity(pending.len());
    let mut attachments = Vec::with_capacity(pending.len());

    for att in pending {
        // Pre-uploaded attachment (e.g., GIF from picker)
        if let Some(pointer) = &att.pointer {
            pointers.push(pointer.clone());
            if let Ok(Some(full_att)) = attachment_service.get(&att.id) {
                attachments.push(full_att);
            }
            continue;
        }

        // Upload to CDN with encryption
        match attachment_service.upload_to_cdn_encrypted(grpc_pool, &att.path).await {
            Ok(res) => {
                pointers.push(res.to_attachment_pointer());
                if let Ok(Some(full_att)) = attachment_service.get(&res.attachment_id) {
                    attachments.push(full_att);
                }
            }
            Err(e) => {
                tracing::error!(
                    context = context,
                    path = %att.path.display(),
                    error = %e,
                    "Failed to upload attachment"
                );
            }
        }
    }

    UploadedAttachments { pointers, attachments }
}

// ============================================================================
// Shared UI update helpers
// ============================================================================

/// Update a message's reaction summary in the UI model.
///
/// Finds the message by sender_id + timestamp_ns, updates its reaction
/// fields, and replaces the model. Used by all 4 reaction paths
/// (anon DM, anon group, regular DM, channel).
pub(crate) fn update_reaction_in_ui(
    window: &MainWindow,
    sender_id: &str,
    timestamp_ns: &str,
    emoji: &str,
    remove: bool,
) {
    use crate::bin_ui::converters::parse_reaction_summary_to_entries;
    use crate::bin_ui::features::auth::update_reaction_summary;

    let current_model = window.get_messages();
    let messages: Vec<_> = (0..current_model.row_count())
        .filter_map(|i| current_model.row_data(i))
        .map(|mut m| {
            if m.sender_id.as_str() == sender_id
                && m.timestamp_ns.as_str() == timestamp_ns
            {
                let new_summary = update_reaction_summary(&m.reaction_summary.to_string(), emoji, remove);
                m.reaction_summary = new_summary.clone().into();
                m.has_reactions = !new_summary.is_empty();
                m.reactions = slint::ModelRc::new(slint::VecModel::from(parse_reaction_summary_to_entries(&new_summary)));
            }
            m
        })
        .collect();
    window.set_messages(slint::ModelRc::new(slint::VecModel::from(messages)));
}

/// Remove a message from the UI model by sender_id + timestamp_ns.
///
/// Unregisters any inline GIF animations for the deleted message.
/// Used by all delete paths (anon DM, anon group, regular DM, channel)
/// for both delete-for-me and delete-for-everyone.
pub(crate) fn remove_message_from_ui(
    window: &MainWindow,
    sender_id: &str,
    timestamp_ns: &str,
    unregister_gifs: impl Fn(&crate::MessageItemData),
) {
    let current_model = window.get_messages();
    let mut messages = Vec::new();
    for i in 0..current_model.row_count() {
        if let Some(m) = current_model.row_data(i) {
            if m.sender_id.as_str() == sender_id
                && m.timestamp_ns.as_str() == timestamp_ns
            {
                unregister_gifs(&m);
            } else {
                messages.push(m);
            }
        }
    }
    window.set_messages(slint::ModelRc::new(slint::VecModel::from(messages)));
}
