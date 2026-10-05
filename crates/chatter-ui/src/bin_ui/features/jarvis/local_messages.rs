//! Local-only Jarvis messages (not sent to server).
//!
//! Core shapes each row (`chatter_core::client::JarvisRow`); this turns it
//! into the Slint view type and splices it into the message list.

use slint::{Model, ModelRc, VecModel};

use chatter_core::client::JarvisRow;

use crate::MainWindow;
use crate::MessageItemData;

/// The Slint row for a core-shaped Jarvis row. Presentation only: the time
/// label and the avatar initial.
pub fn row_to_item(row: JarvisRow) -> MessageItemData {
    let ts = chrono::DateTime::from_timestamp_nanos(row.timestamp_ns as i64);
    let (content, rich_content) = crate::bin_ui::converters::body_fields(&row.content);
    MessageItemData {
        message_id: row.id.into(),
        sender_id: if row.is_own { "me" } else { "jarvis" }.into(),
        sender_initials: row.sender_name.chars().next().unwrap_or('J').to_string().into(),
        sender_name: row.sender_name.into(),
        content,
        rich_content,
        timestamp: ts.with_timezone(&chrono::Local).format("%H:%M").to_string().into(),
        timestamp_ns: row.timestamp_ns.to_string().into(),
        is_own: row.is_own,
        is_ai: !row.is_own,
        show_sender: true,
        delivery_status: if row.thinking { "thinking" } else { "local" }.into(),
        ..Default::default()
    }
}

/// Append rows to the message list, dropping the row with `remove_id` first.
pub fn push_rows(window: &MainWindow, remove_id: Option<&str>, rows: Vec<JarvisRow>) {
    let model = window.get_messages();
    let mut messages: Vec<_> = model
        .iter()
        .filter(|m| remove_id != Some(m.message_id.as_str()))
        .collect();
    messages.extend(rows.into_iter().map(row_to_item));
    window.set_is_at_bottom(true);
    window.set_messages(ModelRc::new(VecModel::from(messages)));
}

/// Add a "Jarvis is thinking..." placeholder message.
///
/// Returns the message ID for later removal/replacement.
pub fn add_jarvis_thinking_placeholder(window: &MainWindow) -> String {
    let row = JarvisRow::thinking();
    let id = row.id.clone();
    push_rows(window, None, vec![row]);
    id
}

/// Remove the thinking placeholder and add the real response.
pub fn replace_thinking_with_response(
    window: &MainWindow,
    thinking_id: &str,
    query: &str,
    response: &str,
) {
    push_rows(window, Some(thinking_id), vec![JarvisRow::inline_reply(query, response)]);
}
