//! Helper functions for chat UI operations.

use slint::Model;
use chatter_core::domain::UserId;
use crate::MainWindow;

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
        rich_content: slint::ModelRc::default(),
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
        date_separator_text: Default::default(),
        is_continuation: false,
        show_date_separator: false,
        has_link_preview: false,
        link_preview: Default::default(),
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
    window.set_messages(ModelRc::new(VecModel::from(messages)));
    window.set_composer_text("".into());
    window.set_message_scroll_position(-99999.0);
    window.set_is_at_bottom(true);
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
