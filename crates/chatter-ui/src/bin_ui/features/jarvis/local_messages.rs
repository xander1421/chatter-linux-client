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
        content: "_Thinking..._".into(),
        timestamp: timestamp.into(),
        timestamp_ns: timestamp_ns.to_string().into(),
        is_ai: true,
        show_sender: true,
        delivery_status: "thinking".into(),
        ..Default::default()
    };

    let model = window.get_messages();
    let mut messages: Vec<_> = model.iter()
        .collect();
    messages.push(msg_item);
    window.set_is_at_bottom(true);
    window.set_messages(ModelRc::new(VecModel::from(messages)));

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
    let mut messages: Vec<_> = model.iter()
        .filter(|m| m.message_id.as_str() != thinking_id) // Remove thinking placeholder
        .collect();

    // Add the real response
    let msg_item = MessageItemData {
        message_id: format!("jarvis-{}", uuid::Uuid::new_v4()).into(),
        sender_id: "jarvis".into(),
        sender_name: "Jarvis".into(),
        sender_initials: "J".into(),
        content: format!("**Q:** {}\n\n{}", query, response).into(),
        timestamp: timestamp.into(),
        timestamp_ns: timestamp_ns.to_string().into(),
        is_ai: true,
        show_sender: true,
        delivery_status: "local".into(),
        ..Default::default()
    };

    messages.push(msg_item);
    window.set_is_at_bottom(true);
    window.set_messages(ModelRc::new(VecModel::from(messages)));
}
