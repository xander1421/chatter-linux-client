//! Handle @Jarvis mentions in messages.
//!
//! Core decides whether a message is an @Jarvis request
//! (`ChatterClient::jarvis_mention`); this renders the turn and routes the
//! query to the JarvisService.

use std::sync::Arc;

use chatter_core::client::ChatterClient;
use chatter_core::domain::jarvis::JarvisContext;
use slint::Weak;

use crate::MainWindow;

/// Handle a @Jarvis mention asynchronously.
///
/// Sets thinking state, adds thinking placeholder to chat, calls JarvisService,
/// and replaces placeholder with actual response.
pub async fn handle_jarvis_mention(
    ui_ctx: &Arc<ChatterClient>,
    query: &str,
    context: JarvisContext,
    weak_window: Weak<MainWindow>,
) {
    // Set thinking state
    ui_ctx.jarvis_state().set_thinking(true);

    // Add thinking placeholder to chat and update header indicator
    let thinking_id = {
        let weak = weak_window.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_jarvis_thinking(true);
                let id = super::local_messages::add_jarvis_thinking_placeholder(&w);
                let _ = tx.send(id);
            }
        });
        rx.recv().ok()
    };

    // Get current user ID
    let author_id = ui_ctx.auth_snapshot().current_user_id;

    // Add user message to Jarvis state
    ui_ctx
        .jarvis_state()
        .add_user_message(query, Some(&context.context_id));

    // Call Jarvis
    match ui_ctx.jarvis_ask(&context, query, author_id.as_ref()).await {
        Ok(response) => {
            // Add assistant response to state
            ui_ctx.jarvis_state().add_assistant_message(&response);

            // Update UI - replace thinking placeholder with actual response
            let response_clone = response.clone();
            let query_clone = query.to_string();
            let thinking_id_clone = thinking_id.clone();
            let jarvis_state = ui_ctx.jarvis_state().clone();
            let _ = slint::invoke_from_event_loop(move || {
                jarvis_state.set_thinking(false);
                if let Some(w) = weak_window.upgrade() {
                    w.set_jarvis_thinking(false);
                    if let Some(tid) = thinking_id_clone {
                        super::local_messages::replace_thinking_with_response(
                            &w,
                            &tid,
                            &query_clone,
                            &response_clone,
                        );
                    }
                    tracing::info!("Jarvis response displayed: {} chars", response_clone.len());
                }
            });
        }
        Err(e) => {
            tracing::error!("Jarvis error: {}", e);
            let error_msg = format!("I encountered an error: {}", e);
            ui_ctx.jarvis_state().add_assistant_message(&error_msg);

            let query_clone = query.to_string();
            let thinking_id_clone = thinking_id.clone();
            let jarvis_state = ui_ctx.jarvis_state().clone();
            let _ = slint::invoke_from_event_loop(move || {
                jarvis_state.set_thinking(false);
                if let Some(w) = weak_window.upgrade() {
                    w.set_jarvis_thinking(false);
                    if let Some(tid) = thinking_id_clone {
                        super::local_messages::replace_thinking_with_response(
                            &w,
                            &tid,
                            &query_clone,
                            &error_msg,
                        );
                    }
                    tracing::warn!("Jarvis error displayed: {}", error_msg);
                }
            });
        }
    }
}
