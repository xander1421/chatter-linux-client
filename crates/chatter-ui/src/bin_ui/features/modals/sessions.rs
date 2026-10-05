//! Sessions modal callbacks.
//!
//! Uses SessionService for all session operations - no direct network calls.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::session_service::Session;

use crate::{MainWindow, SessionInfo, spawn_async_action};

/// Convert domain Session to Slint SessionInfo. Times use the app's one
/// formatter ("Yesterday 2:01 PM"), not ISO.
pub(crate) fn session_to_slint(s: Session) -> SessionInfo {
    SessionInfo {
        device_id: s.device_id as i32,
        device_info: s.device_info.into(),
        ip_address: s.ip_address.into(),
        last_used: chatter_core::ui::format_timestamp(s.last_used).into(),
        created_at: chatter_core::ui::format_timestamp(s.created_at).into(),
        is_current: s.is_current,
        is_mobile: s.is_mobile,
    }
}

/// Update UI with sessions list.
fn update_sessions_ui(weak: slint::Weak<MainWindow>, sessions: Vec<Session>) {
    let slint_sessions: Vec<SessionInfo> = sessions.into_iter().map(session_to_slint).collect();
    let session_count = slint_sessions.len() as i32;

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_sessions(ModelRc::new(VecModel::from(slint_sessions)));
            w.set_sessions_loading(false);
            w.set_sessions_revoking(false);
            w.set_settings_session_count(session_count);
        }
    });
}

/// Load the approved-but-unfinished device links into the Sessions modal.
/// A failed fetch shows none rather than a stale list.
fn refresh_pending_links(weak: slint::Weak<MainWindow>, ui_ctx: Arc<ChatterClient>) {
    tokio::spawn(async move {
        let links = ui_ctx.list_pending_links().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to list pending device links");
            Vec::new()
        });
        let rows: Vec<crate::PendingLinkInfo> = links
            .into_iter()
            .map(|l| crate::PendingLinkInfo {
                uuid: l.uuid.into(),
                device_name: l.device_name.into(),
                expires: chrono::DateTime::from_timestamp_nanos(l.expires_at as i64)
                    .with_timezone(&chrono::Local)
                    .format("%H:%M")
                    .to_string()
                    .into(),
            })
            .collect();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_sessions_pending_links(ModelRc::new(VecModel::from(rows)));
            }
        });
    });
}

pub fn wire_session_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // View sessions
    let weak_window = window.as_weak();
    let ui_ctx_view = ui_ctx.clone();

    window.on_settings_view_sessions(move || {
        let ui_ctx = ui_ctx_view.clone();
        refresh_pending_links(weak_window.clone(), ui_ctx.clone());

        spawn_async_action!(
            weak_window,
            set_sessions_loading,
            async { ui_ctx.list_sessions().await },
            |w: &MainWindow, sessions| {
                update_sessions_ui(w.as_weak(), sessions);
            }
        );
    });

    // Cancel an approved link before its device completes it.
    let weak_window = window.as_weak();
    let ui_ctx_cancel = ui_ctx.clone();

    window.on_cancel_pending_link(move |uuid: slint::SharedString| {
        let ui_ctx = ui_ctx_cancel.clone();
        let weak = weak_window.clone();
        let uuid = uuid.to_string();
        tokio::spawn(async move {
            if let Err(e) = ui_ctx.cancel_pending_link(&uuid).await {
                tracing::warn!(error = %e, "Failed to cancel pending device link");
            }
            refresh_pending_links(weak, ui_ctx);
        });
    });

    // Revoke single session
    let weak_window = window.as_weak();
    let ui_ctx_revoke = ui_ctx.clone();

    window.on_revoke_session(move |device_id: i32| {
        let ui_ctx = ui_ctx_revoke.clone();

        spawn_async_action!(
            weak_window,
            set_sessions_revoking,
            async {
                ui_ctx.revoke_session(device_id as u32).await?;
                ui_ctx.list_sessions().await
            },
            |w: &MainWindow, sessions| {
                update_sessions_ui(w.as_weak(), sessions);
            }
        );
    });

    // Revoke all other sessions
    let weak_window = window.as_weak();
    let ui_ctx_all = ui_ctx.clone();

    window.on_revoke_all_sessions(move || {
        let ui_ctx = ui_ctx_all.clone();

        spawn_async_action!(
            weak_window,
            set_sessions_revoking,
            async {
                ui_ctx.revoke_all_other_sessions().await?;
                ui_ctx.list_sessions().await
            },
            |w: &MainWindow, sessions| {
                update_sessions_ui(w.as_weak(), sessions);
            }
        );
    });
}
