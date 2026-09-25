//! Local AI channel — the Jarvis conversation rendered as a normal channel.
//!
//! The channel is virtual: nil-UUID id, injected by
//! `ChatterClient::sidebar_channels`, never touches the network. History is
//! Jarvis's persisted `general()` conversation; sends route to `jarvis_ask`.

use std::sync::Arc;

use slint::{ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::jarvis::Role;

use crate::{run_ui, MainWindow, MessageItemData};

/// Whether a Slint-side channel-id string refers to the local AI channel.
pub fn is_jarvis_channel_str(channel_id: &str) -> bool {
    channel_id
        .parse::<chatter_core::domain::ids::ChannelId>()
        .is_ok_and(|id| chatter_core::client::is_jarvis_channel(&id))
}

/// Load the AI channel (header + persisted history) into the message view.
pub async fn load_jarvis_channel_into_ui(weak: slint::Weak<MainWindow>, ui_ctx: Arc<ChatterClient>) {
    let history = match ui_ctx.jarvis_channel_history(200) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("failed to load AI channel history: {e}");
            Vec::new()
        }
    };
    let own_name = ui_ctx
        .auth_state()
        .get()
        .display_name
        .unwrap_or_else(|| "Me".to_string());
    // Where these messages go (local vs cloud) is core's call — both the
    // topic and the privacy line under the messages say so.
    let topic = ui_ctx.jarvis_channel_topic();
    let (privacy_line, leaves_device) = ui_ctx.jarvis_privacy_line();

    run_ui!(weak, |w| {
        w.set_channel_name(chatter_core::client::JARVIS_CHANNEL_NAME.into());
        crate::bin_ui::converters::set_conversation_kind(
            &w, &chatter_core::domain::message::ConversationType::Channel, true,
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

        let items: Vec<MessageItemData> = history
            .iter()
            .map(|c| {
                let is_own = matches!(c.role, Role::User);
                conversation_item(
                    &c.id.to_string(),
                    is_own,
                    if is_own { &own_name } else { "Jarvis" },
                    &c.content,
                    c.timestamp,
                )
            })
            .collect();
        w.set_messages(ModelRc::new(VecModel::from(items)));
        w.set_loading_messages(false);
        w.set_is_at_bottom(true);
    });
}

/// Build one message row for the AI channel.
pub fn conversation_item(
    id: &str,
    is_own: bool,
    sender_name: &str,
    content: &str,
    timestamp_ns: u64,
) -> MessageItemData {
    let ts = chrono::DateTime::from_timestamp_nanos(timestamp_ns as i64);
    MessageItemData {
        message_id: id.into(),
        sender_id: if is_own { "me" } else { "jarvis" }.into(),
        sender_name: sender_name.into(),
        sender_initials: sender_name.chars().next().unwrap_or('J').to_string().into(),
        sender_avatar_source: slint::Image::default(),
        has_avatar: false,
        content: content.into(),
        rich_content: ModelRc::default(),
        timestamp: ts.with_timezone(&chrono::Local).format("%H:%M").to_string().into(),
        timestamp_ns: timestamp_ns.to_string().into(),
        is_own,
        is_edited: false,
        is_deleted: false,
        is_system: false,
        is_ai: !is_own,
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
        ..Default::default()
    }
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
    let own_name = ui_ctx
        .auth_state()
        .get()
        .display_name
        .unwrap_or_else(|| "Me".to_string());

    // Append the user's message + thinking placeholder on the UI thread.
    let thinking_id = {
        let weak = weak.clone();
        let text = text.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                use slint::Model;
                let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
                let model = w.get_messages();
                let mut messages: Vec<_> = (0..model.row_count()).filter_map(|i| model.row_data(i)).collect();
                messages.push(conversation_item(
                    &format!("jarvis-user-{}", uuid::Uuid::new_v4()),
                    true,
                    &own_name,
                    &text,
                    now_ns,
                ));
                w.set_is_at_bottom(true);
                w.set_messages(ModelRc::new(VecModel::from(messages)));
                w.set_jarvis_thinking(true);
                let id = super::add_jarvis_thinking_placeholder(&w);
                let _ = tx.send(id);
            }
        });
        rx.recv().ok()
    };

    // `general()` context == the AI channel identity; ask persists both sides.
    let context = JarvisContext::general();
    let result = if let Some(cmd) = super::parse_slash_command(&text) {
        ui_ctx.jarvis_execute(cmd, &context, author_id.as_ref()).await.1
    } else {
        ui_ctx.jarvis_ask(&context, &text, author_id.as_ref()).await
    };
    let response = match result {
        Ok(r) => r,
        Err(e) => format!(
            "Error: {}\n\nCheck Settings → AI Assistant: is a local provider (Ollama, LM Studio, …) running and a model selected?",
            e
        ),
    };

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            use slint::Model;
            w.set_jarvis_thinking(false);
            let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;
            let model = w.get_messages();
            let mut messages: Vec<_> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .filter(|m| thinking_id.as_deref() != Some(m.message_id.as_str()))
                .collect();
            messages.push(conversation_item(
                &format!("jarvis-{}", uuid::Uuid::new_v4()),
                false,
                "Jarvis",
                &response,
                now_ns,
            ));
            w.set_is_at_bottom(true);
            w.set_messages(ModelRc::new(VecModel::from(messages)));
        }
    });
}
