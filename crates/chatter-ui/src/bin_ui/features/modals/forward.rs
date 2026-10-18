//! Forward-message modal confirm handler.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;

use crate::MainWindow;

/// Decode the already-shaped sidebar avatar for a forwarding destination.
/// Missing or malformed cached bytes leave Avatar's honest initials fallback.
pub fn target_avatar(
    ui_ctx: &ChatterClient,
    kind: &chatter_core::domain::message::ConversationType,
    id: &str,
) -> slint::Image {
    ui_ctx
        .forward_destination_avatar(kind, id)
        .as_deref()
        .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
        .unwrap_or_default()
}

pub fn wire_forward_message(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Forward message: look up the selected target's type in the populated
    // targets model, map to an ActiveConversation, and forward via core.
    let weak_window = window.as_weak();
    let ui_ctx_forward = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_forward_message(move |target_id: SharedString| {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_forward.clone();
            let target_id_str = target_id.to_string();

            let Some(w) = weak.upgrade() else { return };
            let target_type = {
                let targets = w.global::<crate::ModalHostState>().get_forward_targets();
                targets
                    .iter()
                    .find(|t| t.id.as_str() == target_id_str)
                    .map(|t| t.target_type.to_string())
            };
            // In the order they are to arrive.
            let sources = crate::bin_ui::features::chat::forward_sources(&w);

            // The row's type tag is core's conversation vocabulary, so the target
            // resolves through core's own inverse — no second mapping to drift.
            use chatter_core::domain::message::ConversationType;
            use chatter_core::state::ActiveConversation;
            let target = target_type
                .as_deref()
                .and_then(|kind| ConversationType::from_str_value(kind).ok())
                .and_then(|kind| ActiveConversation::from_routing_key(&kind, &target_id_str));
            let (Some(target), Some(sources)) = (target, sources.filter(|s| !s.is_empty())) else {
                tracing::error!(
                    target_id = %target_id_str,
                    ?target_type,
                    "forward: unparsable target or source identity"
                );
                return;
            };
            w.global::<crate::ModalHostState>()
                .set_forward_loading(true);

            tokio::spawn(async move {
                let source = ui_ctx.conversation(ui_ctx.active_conversation());
                // One at a time, in order: the copies must arrive in the order the
                // originals were written. A failure does not stop the rest.
                let mut failed = 0usize;
                for (sender_id, timestamp) in &sources {
                    if let Err(e) = source.forward_to(sender_id, *timestamp, target).await {
                        tracing::error!("forward failed: {e}");
                        failed += 1;
                    }
                }
                let total = sources.len();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.global::<crate::ModalHostState>()
                            .set_forward_loading(false);
                        if failed < total {
                            tracing::info!(?target, sent = total - failed, "forward sent");
                            crate::bin_ui::features::back::close(&w, "forward-message");
                            // Core persisted the copy, but the open list is a
                            // Slint model only the composer's send path appends
                            // to — so a forward into the conversation on screen
                            // stayed invisible until the next load (§112k).
                            // Reopen it through the sidebar path to reload.
                            if ui_ctx.active_conversation() == target {
                                crate::bin_ui::features::sidebar::select_conversation(&w, target);
                            }
                        }
                        if failed > 0 {
                            // A failed forward used to look identical to a
                            // no-op: the button un-spun and nothing else moved.
                            let msg = if total == 1 {
                                "Could not forward that message".to_string()
                            } else {
                                format!("Could not forward {failed} of {total} messages")
                            };
                            w.set_toast_message(msg.into());
                            w.set_toast_variant("error".into());
                            w.set_toast_visible(true);
                        }
                    }
                });
            });
        });
}
