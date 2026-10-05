//! Local AI channel — the Jarvis conversation rendered as a normal channel.
//!
//! The channel is virtual: nil-UUID id, injected by
//! `ChatterClient::sidebar_channels`, never touches the network. History is
//! Jarvis's persisted `general()` conversation; sends route to `jarvis_ask`.

use std::sync::Arc;

use slint::{ModelRc, VecModel};

use chatter_core::client::{ChatterClient, JarvisRow, jarvis_channel_reply_text};

use super::local_messages::{push_rows, row_to_item};
use crate::{MainWindow, MessageItemData, run_ui};

/// Whether a Slint-side channel-id string refers to the local AI channel.
pub fn is_jarvis_channel_str(channel_id: &str) -> bool {
    channel_id
        .parse::<chatter_core::domain::ids::ChannelId>()
        .is_ok_and(|id| chatter_core::client::is_jarvis_channel(&id))
}

/// Load the AI channel (header + persisted history) into the message view.
pub async fn load_jarvis_channel_into_ui(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
) {
    let rows = ui_ctx.jarvis_channel_rows(200);
    // Where these messages go (local vs cloud) is core's call — both the
    // topic and the privacy line under the messages say so.
    let topic = ui_ctx.jarvis_channel_topic();
    let (privacy_line, leaves_device) = ui_ctx.jarvis_privacy_line();

    run_ui!(weak, |w| {
        w.set_channel_name(chatter_core::client::JARVIS_CHANNEL_NAME.into());
        crate::bin_ui::converters::set_conversation_kind(
            &w,
            &chatter_core::domain::message::ConversationType::Channel,
            true,
        );
        w.set_privacy_line_override(privacy_line.into());
        w.set_privacy_line_exposed(leaves_device);
        w.set_is_dm(false);
        w.set_member_count(0);
        w.set_topic(topic.into());
        w.set_header_presence_status("offline".into());
        w.set_channel_avatar_source(slint::Image::default());
        w.set_channel_has_avatar(false);
        w.set_has_pinned_messages(false);

        let items: Vec<MessageItemData> = rows.into_iter().map(row_to_item).collect();
        w.set_messages(ModelRc::new(VecModel::from(items)));
        w.set_loading_messages(false);
        w.set_is_at_bottom(true);
    });
}

/// Handle a send in the AI channel: append the user's message, show the
/// thinking placeholder, ask Jarvis, replace with the reply. Never networks.
pub async fn handle_jarvis_channel_send(
    ui_ctx: Arc<ChatterClient>,
    text: String,
    weak: slint::Weak<MainWindow>,
) {
    use chatter_core::domain::jarvis::JarvisContext;

    let author_id = ui_ctx.auth_state().get().current_user_id;
    let own_row = JarvisRow::own(&ui_ctx.jarvis_own_name(), &text);
    let thinking_row = JarvisRow::thinking();
    let thinking_id = thinking_row.id.clone();

    // Append the user's message + thinking placeholder on the UI thread.
    run_ui!(weak.clone(), |w| {
        w.set_jarvis_thinking(true);
        push_rows(&w, None, vec![own_row, thinking_row]);
    });

    // `general()` context == the AI channel identity; ask persists both sides.
    let context = JarvisContext::general();
    let result = if let Some(cmd) = super::parse_slash_command(&text) {
        ui_ctx
            .jarvis_execute(cmd, &context, author_id.as_ref())
            .await
            .1
    } else {
        ui_ctx.jarvis_ask(&context, &text, author_id.as_ref()).await
    };
    let reply = JarvisRow::reply(&jarvis_channel_reply_text(&result));

    run_ui!(weak, |w| {
        w.set_jarvis_thinking(false);
        push_rows(&w, Some(&thinking_id), vec![reply]);
    });
}
