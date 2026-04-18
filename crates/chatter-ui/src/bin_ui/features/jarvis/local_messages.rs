//! Local-only Jarvis messages (not sent to server).
//!
//! These messages appear in the chat view but are only stored locally.
//! Used for displaying Jarvis AI responses inline with regular messages.

use slint::{Model, ModelRc, VecModel};

use crate::MainWindow;
use crate::MessageItemData;

/// Add a "Jarvis is thinking..." placeholder message.
///
/// Returns the message ID for later removal/replacement.
pub fn add_jarvis_thinking_placeholder(window: &MainWindow) -> String {
    let msg_id = format!("jarvis-thinking-{}", uuid::Uuid::new_v4());
    let timestamp = chrono::Local::now().format("%H:%M").to_string();
    let timestamp_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;

    let msg_item = MessageItemData {
        message_id: msg_id.clone().into(),
        sender_id: "jarvis".into(),
        sender_name: "Jarvis".into(),
        sender_initials: "J".into(),
        sender_avatar_url: "".into(),
        sender_avatar_source: slint::Image::default(),
        has_avatar: false,
        content: "_Thinking..._".into(),
        rich_content: ModelRc::default(),
        timestamp: timestamp.into(),
        timestamp_ns: timestamp_ns.to_string().into(),
        is_own: false,
        is_edited: false,
        is_deleted: false,
        is_system: false,
        is_ai: true,
        has_attachments: false,
        attachment_count: 0,
        attachments: ModelRc::default(),
        has_reactions: false,
        reaction_summary: "".into(),
        reactions: ModelRc::default(),
        reply_count: 0,
        reply_count_text: "".into(),
        is_thread_reply: false,
        is_reply: false,
        reply_to_sender_name: "".into(),
        reply_to_preview: "".into(),
        reply_to_has_thumbnail: false,
        reply_to_thumbnail: slint::Image::default(),
        show_sender: true,
        delivery_status: "thinking".into(),
        is_pinned: false,
        is_call: false,
        call_type: "".into(),
        call_duration: "".into(),
        date_separator_text: Default::default(),
        is_continuation: false,
        show_date_separator: false,
        has_link_preview: false,
        link_preview: Default::default(),
    };

    let model = window.get_messages();
    let mut messages: Vec<_> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect();
    messages.push(msg_item);
    window.set_messages(ModelRc::new(VecModel::from(messages)));
    window.set_message_scroll_position(-99999.0);

    msg_id
}

/// Remove the thinking placeholder and add the real response.
pub fn replace_thinking_with_response(
    window: &MainWindow,
    thinking_id: &str,
    query: &str,
    response: &str,
) {
    let timestamp = chrono::Local::now().format("%H:%M").to_string();
    let timestamp_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;

    let model = window.get_messages();
    let mut messages: Vec<_> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .filter(|m| m.message_id.as_str() != thinking_id) // Remove thinking placeholder
        .collect();

    // Add the real response
    let msg_item = MessageItemData {
        message_id: format!("jarvis-{}", uuid::Uuid::new_v4()).into(),
        sender_id: "jarvis".into(),
        sender_name: "Jarvis".into(),
        sender_initials: "J".into(),
        sender_avatar_url: "".into(),
        sender_avatar_source: slint::Image::default(),
        has_avatar: false,
        content: format!("**Q:** {}\n\n{}", query, response).into(),
        rich_content: ModelRc::default(),
        timestamp: timestamp.into(),
        timestamp_ns: timestamp_ns.to_string().into(),
        is_own: false,
        is_edited: false,
        is_deleted: false,
        is_system: false,
        is_ai: true,
        has_attachments: false,
        attachment_count: 0,
        attachments: ModelRc::default(),
        has_reactions: false,
        reaction_summary: "".into(),
        reactions: ModelRc::default(),
        reply_count: 0,
        reply_count_text: "".into(),
        is_thread_reply: false,
        is_reply: false,
        reply_to_sender_name: "".into(),
        reply_to_preview: "".into(),
        reply_to_has_thumbnail: false,
        reply_to_thumbnail: slint::Image::default(),
        show_sender: true,
        delivery_status: "local".into(),
        is_pinned: false,
        is_call: false,
        call_type: "".into(),
        call_duration: "".into(),
        date_separator_text: Default::default(),
        is_continuation: false,
        show_date_separator: false,
        has_link_preview: false,
        link_preview: Default::default(),
    };

    messages.push(msg_item);
    window.set_messages(ModelRc::new(VecModel::from(messages)));
    window.set_message_scroll_position(-99999.0);
    window.set_is_at_bottom(true);
}
