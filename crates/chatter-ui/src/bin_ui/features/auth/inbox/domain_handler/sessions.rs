//! Multi-device / sessions event handlers.
//!
//! `DomainEvent::DeviceLinked` fires when a sibling device joins the
//! account. UI side: log it and refresh the sessions/devices modal so
//! the new entry appears without manual re-fetch.

use std::sync::Arc;

use slint::Weak;

use crate::MainWindow;
use chatter_core::client::ChatterClient;

pub(super) fn handle_device_linked(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    device_name: Option<String>,
    approved_by_self: bool,
) {
    let label = device_name.unwrap_or_else(|| "a new device".to_string());
    if approved_by_self {
        tracing::info!(device = %label, "Approved + linked");
    } else {
        tracing::info!(device = %label, "New device linked to account");
    }

    // Reuse the modal's owner/request-fenced loader rather than publishing
    // asynchronous rows through a second, unfenced path.
    let weak = window.clone();
    let client = ui_ctx.clone();
    let owner = client.session_current_user_id();
    let revision = client.auth_state().get().session_revision;
    let _ = slint::invoke_from_event_loop(move || {
        if client.session_current_user_id() != owner
            || client.auth_state().get().session_revision != revision
        {
            return;
        }
        if let Some(w) = weak.upgrade() {
            if w.get_active_modal() == "sessions" && !w.get_sessions_revoking() {
                w.invoke_settings_view_sessions();
            }
        }
    });
}
