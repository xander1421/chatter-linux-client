//! Sessions modal callbacks.
//!
//! Uses SessionService for all session operations - no direct network calls.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::session_service::Session;

use crate::{MainWindow, SessionInfo, spawn_async_action, run_ui};

/// Convert domain Session to Slint SessionInfo.
fn session_to_slint(s: Session) -> SessionInfo {
    SessionInfo {
        device_id: s.device_id as i32,
        device_info: s.device_info.into(),
        ip_address: s.ip_address.into(),
        last_used: s.last_used.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string().into(),
        created_at: s.created_at.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string().into(),
        is_current: s.is_current,
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

pub fn wire_session_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let session_service = ui_ctx.sessions().clone();

    // View sessions
    let weak_window = window.as_weak();
    let service_view = session_service.clone();

    window.on_settings_view_sessions(move || {
        let service = service_view.clone();
        run_ui!(weak_window, |w| w.set_active_modal("sessions".into()));

        spawn_async_action!(weak_window, set_sessions_loading,
            async { service.list_sessions().await },
            |w: &MainWindow, sessions| { update_sessions_ui(w.as_weak(), sessions); }
        );
    });

    // Revoke single session
    let weak_window = window.as_weak();
    let service_revoke = session_service.clone();

    window.on_revoke_session(move |device_id: i32| {
        let service = service_revoke.clone();
        let service_refresh = service_revoke.clone();

        spawn_async_action!(weak_window, set_sessions_revoking,
            async { 
                service.revoke_session(device_id as u32).await?;
                service_refresh.list_sessions().await
            },
            |w: &MainWindow, sessions| { update_sessions_ui(w.as_weak(), sessions); }
        );
    });

    // Revoke all other sessions
    let weak_window = window.as_weak();
    let service_revoke_all = session_service.clone();

    window.on_revoke_all_sessions(move || {
        let service = service_revoke_all.clone();
        let service_refresh = service_revoke_all.clone();

        spawn_async_action!(weak_window, set_sessions_revoking,
            async {
                service.revoke_all_other_sessions().await?;
                service_refresh.list_sessions().await
            },
            |w: &MainWindow, sessions| { update_sessions_ui(w.as_weak(), sessions); }
        );
    });
}
