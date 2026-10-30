//! Multi-device / sessions event handlers.
//!
//! `DomainEvent::DeviceLinked` fires when a sibling device joins the
//! account. UI side: log it and refresh the sessions/devices modal so
//! the new entry appears without manual re-fetch.

use slint::ComponentHandle;
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
    let revision = client.auth_snapshot().session_revision;
    let _ = slint::invoke_from_event_loop(move || {
        if client.session_current_user_id() != owner
            || client.auth_snapshot().session_revision != revision
        {
            return;
        }
        if let Some(w) = weak.upgrade() {
            // The same visibility rule as the loader: gating on the Sessions
            // modal alone missed Settings › Linked devices, which kept
            // showing the old count after a link completed.
            if crate::bin_ui::features::modals::sessions::sessions_visible(&w)
                && !w.global::<crate::ModalHostState>().get_sessions_revoking()
            {
                w.global::<crate::SettingsPane>()
                    .invoke_settings_view_sessions();
            }
        }
    });
}
