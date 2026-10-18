//! Typing indicator callback.
//!
//! Thin glue: forwards keystroke / idle signals to the core state machine,
//! which owns the Signal-style refresh (10s) + pause (3s) timers. UI does
//! not track typing state — see `chatter_core::services::outgoing_typing`.

use std::sync::Arc;

use chatter_core::client::ChatterClient;

/// Clear outgoing typing state (no STOPPED message needed).
/// Called when a message is sent — the message itself implies "stopped typing".
pub fn clear_outgoing_typing(ui_ctx: &Arc<ChatterClient>) {
    ui_ctx.typing_clear();
}

/// Wire typing indicator callback for DM and channel conversations.
pub fn wire_typing_changed(window: &crate::MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();

    window.on_typing_changed(move |is_typing: bool| {
        if is_typing {
            let ui_ctx_activity = ui_ctx.clone();
            tokio::spawn(async move {
                ui_ctx_activity.record_presence_activity().await;
            });

            ui_ctx.typing_bump(ui_ctx.active_conversation());
        } else {
            ui_ctx.typing_clear();
        }
    });
}
