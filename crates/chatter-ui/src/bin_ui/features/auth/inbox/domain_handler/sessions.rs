//! Multi-device / sessions event handlers.
//!
//! `DomainEvent::DeviceLinked` fires when a sibling device joins the
//! account. UI side: log it and refresh the sessions/devices modal so
//! the new entry appears without manual re-fetch.

use std::sync::Arc;

use slint::Weak;

use chatter_core::client::ChatterClient;
use crate::MainWindow;

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

    // Always refresh on DeviceLinked — cheap (one ListDevices RPC) and
    // means the sessions/devices modal shows the new entry without the
    // user having to close + reopen it.
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    crate::rt_handle().spawn(async move {
        match ui_ctx.list_sessions().await {
            Ok(sessions) => {
                let slint_sessions: Vec<crate::SessionInfo> = sessions
                    .into_iter()
                    .map(|s| crate::SessionInfo {
                        device_id: s.device_id as i32,
                        device_info: s.device_info.into(),
                        ip_address: s.ip_address.into(),
                        last_used: s
                            .last_used
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M")
                            .to_string()
                            .into(),
                        created_at: s
                            .created_at
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d")
                            .to_string()
                            .into(),
                        is_current: s.is_current,
                    })
                    .collect();
                let count = slint_sessions.len() as i32;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_sessions(slint::ModelRc::new(
                            slint::VecModel::from(slint_sessions),
                        ));
                        w.set_settings_session_count(count);
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to refresh sessions on DeviceLinked");
            }
        }
    });
}
