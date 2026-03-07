//! Jarvis panel UI callbacks.
//!
//! Handles Jarvis panel open/close, sending messages, and slash commands.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::jarvis::JarvisContext;
use chatter_linux::state::JarvisRole;

use crate::{JarvisMessageData, MainWindow};

/// Wire Jarvis panel callbacks to the main window.
pub fn wire_jarvis_panel(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    // Close panel
    {
        let jarvis_state = ui_ctx.jarvis_state.clone();
        let weak = window.as_weak();
        window.on_jarvis_panel_close(move || {
            jarvis_state.hide_panel();
            if let Some(w) = weak.upgrade() {
                w.set_jarvis_panel_visible(false);
            }
        });
    }

    // Toggle panel
    {
        let jarvis_state = ui_ctx.jarvis_state.clone();
        let weak = window.as_weak();
        window.on_toggle_jarvis_panel(move || {
            jarvis_state.toggle_panel();
            if let Some(w) = weak.upgrade() {
                w.set_jarvis_panel_visible(jarvis_state.is_panel_visible());
            }
        });
    }

    // Clear messages
    {
        let jarvis_state = ui_ctx.jarvis_state.clone();
        let weak = window.as_weak();
        window.on_jarvis_clear(move || {
            jarvis_state.clear_messages();
            if let Some(w) = weak.upgrade() {
                w.set_jarvis_messages(ModelRc::new(VecModel::from(Vec::<JarvisMessageData>::new())));
            }
        });
    }

    // Send message to Jarvis
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window.on_jarvis_send(move |text: slint::SharedString| {
            let text = text.to_string();
            if text.trim().is_empty() {
                return;
            }

            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();

            // Check for slash commands
            if let Some(cmd) = super::slash_commands::parse_slash_command(&text) {
                tokio::spawn(async move {
                    handle_jarvis_command(&ui_ctx, cmd, weak).await;
                });
            } else {
                // Regular question
                tokio::spawn(async move {
                    handle_jarvis_question(&ui_ctx, &text, weak).await;
                });
            }
        });
    }
}

/// Handle a regular Jarvis question.
async fn handle_jarvis_question(
    ui_ctx: &Arc<UIContext>,
    query: &str,
    weak: slint::Weak<MainWindow>,
) {
    // Set thinking state
    ui_ctx.jarvis_state.set_thinking(true);
    update_thinking_ui(&weak, true);

    // Add user message
    let channel_id = ui_ctx.chat_state.get().active_channel_id;
    let channel_id_str = channel_id.as_ref().map(|id| id.to_string());
    ui_ctx
        .jarvis_state
        .add_user_message(query, channel_id_str.as_deref());
    update_messages_ui(ui_ctx, &weak);

    // Build context
    let context = match &channel_id {
        Some(id) => JarvisContext::channel(id.clone()),
        None => JarvisContext::general(),
    };

    let author_id = ui_ctx.auth_state.get().current_user_id;

    // Call Jarvis
    match ui_ctx
        .jarvis_service
        .ask(&context, query, author_id.as_ref())
        .await
    {
        Ok(response) => {
            ui_ctx.jarvis_state.add_assistant_message(&response);
            ui_ctx.jarvis_state.set_thinking(false);
            update_thinking_ui(&weak, false);
            update_messages_ui(ui_ctx, &weak);
        }
        Err(e) => {
            let error_msg = format!("Error: {}", e);
            ui_ctx.jarvis_state.add_assistant_message(&error_msg);
            ui_ctx.jarvis_state.set_thinking(false);
            update_thinking_ui(&weak, false);
            update_messages_ui(ui_ctx, &weak);
        }
    }
}

/// Handle a Jarvis slash command.
async fn handle_jarvis_command(
    ui_ctx: &Arc<UIContext>,
    cmd: super::slash_commands::JarvisCommand,
    weak: slint::Weak<MainWindow>,
) {
    use super::slash_commands::JarvisCommand;

    ui_ctx.jarvis_state.set_thinking(true);
    update_thinking_ui(&weak, true);

    let channel_id = ui_ctx.chat_state.get().active_channel_id;
    let context = match &channel_id {
        Some(id) => JarvisContext::channel(id.clone()),
        None => JarvisContext::general(),
    };

    let result = match cmd {
        JarvisCommand::Ask(query) => {
            ui_ctx
                .jarvis_state
                .add_user_message(&format!("/ask {}", query), None);
            update_messages_ui(ui_ctx, &weak);

            let author_id = ui_ctx.auth_state.get().current_user_id;
            ui_ctx
                .jarvis_service
                .ask(&context, &query, author_id.as_ref())
                .await
        }
        JarvisCommand::Summarize => {
            ui_ctx.jarvis_state.add_user_message("/summarize", None);
            update_messages_ui(ui_ctx, &weak);

            ui_ctx.jarvis_service.summarize_conversation(&context).await
        }
        JarvisCommand::Knowledge(entity) => {
            ui_ctx
                .jarvis_state
                .add_user_message(&format!("/knowledge {}", entity), None);
            update_messages_ui(ui_ctx, &weak);

            match ui_ctx.jarvis_service.get_entity_knowledge(&entity) {
                Ok(knowledge) => Ok(knowledge.to_summary()),
                Err(e) => Err(e),
            }
        }
        JarvisCommand::Remember(fact) => {
            ui_ctx
                .jarvis_state
                .add_user_message(&format!("/remember {}", fact), None);
            update_messages_ui(ui_ctx, &weak);

            // For /remember, we use observe_message to store the fact
            let author_id = ui_ctx.auth_state.get().current_user_id;
            match ui_ctx
                .jarvis_service
                .observe_message(&context, &fact, author_id.as_ref(), None)
                .await
            {
                Ok(_) => Ok("I'll remember that.".to_string()),
                Err(e) => Err(e),
            }
        }
    };

    match result {
        Ok(response) => {
            ui_ctx.jarvis_state.add_assistant_message(&response);
        }
        Err(e) => {
            ui_ctx
                .jarvis_state
                .add_assistant_message(&format!("Error: {}", e));
        }
    }

    ui_ctx.jarvis_state.set_thinking(false);
    update_thinking_ui(&weak, false);
    update_messages_ui(ui_ctx, &weak);
}

/// Update the thinking state in the UI.
fn update_thinking_ui(weak: &slint::Weak<MainWindow>, thinking: bool) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_jarvis_thinking(thinking);
        }
    });
}

/// Update the messages list in the UI.
fn update_messages_ui(ui_ctx: &Arc<UIContext>, weak: &slint::Weak<MainWindow>) {
    let messages = ui_ctx.jarvis_state.get_messages();
    let slint_messages: Vec<JarvisMessageData> = messages
        .into_iter()
        .map(|m| JarvisMessageData {
            id: m.id.into(),
            role: match m.role {
                JarvisRole::User => "user".into(),
                JarvisRole::Assistant => "assistant".into(),
            },
            content: m.content.into(),
            timestamp: m.timestamp.format("%H:%M").to_string().into(),
        })
        .collect();

    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_jarvis_messages(ModelRc::new(VecModel::from(slint_messages)));
        }
    });
}
