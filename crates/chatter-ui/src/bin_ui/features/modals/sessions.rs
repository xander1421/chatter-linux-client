//! Sessions modal callbacks.
//!
//! Uses SessionService for all session operations - no direct network calls.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::session_service::Session;

use crate::{MainWindow, SessionInfo};

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
    let session_service_view = session_service.clone();

    window.on_settings_view_sessions(move || {
        let weak = weak_window.clone();
        let service = session_service_view.clone();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_sessions_loading(true);
                    w.set_active_modal("sessions".into());
                }
            });

            match service.list_sessions().await {
                Ok(sessions) => update_sessions_ui(weak, sessions),
                Err(e) => {
                    tracing::error!("Failed to list sessions: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Revoke single session
    let weak_window = window.as_weak();
    let session_service_revoke = session_service.clone();

    window.on_revoke_session(move |device_id: i32| {
        let weak = weak_window.clone();
        let service = session_service_revoke.clone();

        tokio::spawn(async move {
            let weak_revoking = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_revoking.upgrade() {
                    w.set_sessions_revoking(true);
                }
            });

            match service.revoke_session(device_id as u32).await {
                Ok(()) => {
                    tracing::info!("Revoked session for device {}", device_id);
                    // Refresh sessions list
                    match service.list_sessions().await {
                        Ok(sessions) => update_sessions_ui(weak, sessions),
                        Err(e) => {
                            tracing::error!("Failed to refresh sessions after revoke: {}", e);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions_revoking(false);
                                }
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to revoke session: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_revoking(false);
                        }
                    });
                }
            }
        });
    });

    // Revoke all other sessions
    let weak_window = window.as_weak();
    let session_service_revoke_all = session_service.clone();

    window.on_revoke_all_sessions(move || {
        let weak = weak_window.clone();
        let service = session_service_revoke_all.clone();

        tokio::spawn(async move {
            let weak_revoking = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_revoking.upgrade() {
                    w.set_sessions_revoking(true);
                }
            });

            match service.revoke_all_other_sessions().await {
                Ok(()) => {
                    tracing::info!("Revoked all other sessions");
                    // Refresh sessions list
                    match service.list_sessions().await {
                        Ok(sessions) => update_sessions_ui(weak, sessions),
                        Err(e) => {
                            tracing::error!("Failed to refresh sessions after revoke all: {}", e);
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = weak.upgrade() {
                                    w.set_sessions_revoking(false);
                                }
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to revoke all sessions: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_sessions_revoking(false);
                        }
                    });
                }
            }
        });
    });
}
