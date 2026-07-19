//! Forward-message modal confirm handler.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;

use crate::MainWindow;

pub fn wire_forward_message(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Forward message: look up the selected target's type in the populated
    // targets model, map to an ActiveConversation, and forward via core.
    let weak_window = window.as_weak();
    let ui_ctx_forward = ui_ctx.clone();
    window.on_forward_message(move |target_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_forward.clone();
        let target_id_str = target_id.to_string();

        let Some(w) = weak.upgrade() else { return };
        let target_type = {
            let targets = w.get_forward_targets();
            (0..targets.row_count())
                .filter_map(|i| targets.row_data(i))
                .find(|t| t.id.as_str() == target_id_str)
                .map(|t| t.target_type.to_string())
        };
        let sender_id_str = w.get_forward_source_sender_id().to_string();
        let timestamp_str = w.get_forward_source_timestamp().to_string();
        w.set_forward_loading(true);

        use chatter_core::state::ActiveConversation;
        let target = match target_type.as_deref() {
            Some("dm") => target_id_str.parse().ok().map(|peer_id| ActiveConversation::ServerDm { peer_id }),
            Some("anon-dm") => target_id_str.parse().ok().map(ActiveConversation::AnonDm),
            Some("group") => target_id_str.parse().ok().map(ActiveConversation::AnonGroup),
            Some("channel") => target_id_str.parse().ok().map(ActiveConversation::Channel),
            _ => None,
        };
        let source_identity = sender_id_str
            .parse::<UserId>()
            .ok()
            .zip(timestamp_str.parse::<u64>().ok());
        let (Some(target), Some((sender_id, timestamp))) = (target, source_identity) else {
            tracing::error!(
                target_id = %target_id_str,
                ?target_type,
                "forward: unparsable target or source identity"
            );
            w.set_forward_loading(false);
            return;
        };

        tokio::spawn(async move {
            let source = ui_ctx.conversation(ui_ctx.chat_state().active_conversation());
            let result = source.forward_to(&sender_id, timestamp, target).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_forward_loading(false);
                    match result {
                        Ok(()) => w.set_active_modal("none".into()),
                        Err(e) => tracing::error!("forward failed: {e}"),
                    }
                }
            });
        });
    });
}

