//! Sessions modal callbacks.
//!
//! Uses SessionService for all session operations - no direct network calls.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::session_service::Session;

use crate::{MainWindow, SessionInfo};

/// Convert domain Session to Slint SessionInfo. Times use the app's one
/// formatter ("Yesterday 2:01 PM"), not ISO.
pub(crate) fn session_to_slint(s: Session) -> SessionInfo {
    SessionInfo {
        device_id: s.device_id as i32,
        device_info: s.device_info.into(),
        ip_address: s.ip_address.into(),
        status_text: s.status_text.into(),
        // Day precision: the server records a device's activity per day
        // (Signal's lastSeen), so a time of day would be invented.
        last_used: crate::bin_ui::converters::format_activity_day(s.last_used).into(),
        created_at: crate::bin_ui::converters::format_timestamp(s.created_at).into(),
        is_current: s.is_current,
        is_mobile: s.is_mobile,
        can_revoke: s.can_revoke,
    }
}

enum Action {
    List,
    Revoke(u32),
    RevokeAll,
    Cancel(String),
}

/// Where the device list is on screen: the Sessions modal, or Settings ›
/// Linked devices (any page pushed over Settings keeps it mounted).
pub(crate) fn sessions_visible(w: &MainWindow) -> bool {
    w.get_active_modal() == "sessions" || w.global::<crate::ModalHostState>().get_settings_open()
}

/// One request owns all publications. Leaving/reopening the modal or starting
/// another request invalidates it, as does logout/relogin (including same user).
fn request(weak: slint::Weak<MainWindow>, client: Arc<ChatterClient>, action: Action) {
    let Some(w) = weak.upgrade() else {
        return;
    };
    if !sessions_visible(&w) {
        return;
    }
    let owner = client.session_current_user_id();
    if owner.is_none() {
        return;
    }
    let revision = client.auth_snapshot().session_revision;
    let generation = w
        .global::<crate::ModalHostState>()
        .get_sessions_generation()
        .wrapping_add(1);
    w.global::<crate::ModalHostState>()
        .set_sessions_generation(generation);
    let listing = matches!(&action, Action::List);
    w.global::<crate::ModalHostState>()
        .set_sessions_loading(true);
    w.global::<crate::ModalHostState>()
        .set_sessions_revoking(!listing);
    w.global::<crate::ModalHostState>()
        .set_sessions_error("".into());
    w.global::<crate::ModalHostState>()
        .set_sessions_notice("".into());
    if listing {
        // Never retain another owner's rows while opening a new view.
        w.global::<crate::ModalHostState>()
            .set_sessions(ModelRc::new(VecModel::from(Vec::<SessionInfo>::new())));
        w.global::<crate::ModalHostState>()
            .set_sessions_pending_links(ModelRc::new(VecModel::from(
                Vec::<crate::PendingLinkInfo>::new(),
            )));
        w.global::<crate::ModalHostState>()
            .set_sessions_loaded(false);
        w.global::<crate::ModalHostState>()
            .set_sessions_revocable_count(0);
    }
    tokio::spawn(async move {
        let current = || {
            client.session_current_user_id() == owner
                && client.auth_snapshot().session_revision == revision
        };
        if !current() {
            return;
        }
        let mut errors = Vec::new();
        let mutation = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            match action {
                Action::List => Ok(None),
                Action::Revoke(id) => client
                    .revoke_session(id)
                    .await
                    .map(|()| None)
                    .map_err(|e| e.to_string()),
                Action::RevokeAll => client
                    .revoke_all_other_sessions()
                    .await
                    .map(|outcome| Some(outcome.summary()))
                    .map_err(|e| e.to_string()),
                Action::Cancel(uuid) => client
                    .cancel_pending_link(&uuid)
                    .await
                    .map(|()| None)
                    .map_err(|e| e.to_string()),
            }
        })
        .await;
        let mut notice = String::new();
        match mutation {
            Ok(Ok(summary)) => notice = summary.unwrap_or_default(),
            Ok(Err(e)) => errors.push(e),
            Err(_) => errors.push("Action timed out; its outcome is uncertain. Review the refreshed devices and pending links.".to_string()),
        }
        if !current() {
            return;
        }
        let sessions =
            tokio::time::timeout(std::time::Duration::from_secs(30), client.list_sessions()).await;
        let sessions = match sessions {
            Ok(Ok(rows)) => Some(rows),
            Ok(Err(e)) => {
                errors.push(format!("Could not load sessions: {e}"));
                None
            }
            Err(_) => {
                errors.push("Loading sessions timed out.".to_string());
                None
            }
        };
        if !current() {
            return;
        }
        let pending = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.list_pending_links(),
        )
        .await;
        let pending = match pending {
            Ok(Ok(rows)) => Some(
                rows.into_iter()
                    .map(|l| crate::PendingLinkInfo {
                        uuid: l.uuid.into(),
                        device_name: l.device_name.into(),
                        expires: chrono::DateTime::from_timestamp_nanos(l.expires_at as i64)
                            .with_timezone(&chrono::Local)
                            .format("%H:%M")
                            .to_string()
                            .into(),
                    })
                    .collect::<Vec<_>>(),
            ),
            Ok(Err(e)) => {
                errors.push(format!("Could not load pending links: {e}"));
                None
            }
            Err(_) => {
                errors.push("Loading pending links timed out.".to_string());
                None
            }
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else {
                return;
            };
            if client.session_current_user_id() != owner
                || client.auth_snapshot().session_revision != revision
                || !sessions_visible(&w)
                || w.global::<crate::ModalHostState>()
                    .get_sessions_generation()
                    != generation
            {
                return;
            }
            if let Some(rows) = sessions {
                w.global::<crate::SettingsPane>()
                    .set_settings_session_count(rows.len() as i32);
                w.global::<crate::ModalHostState>().set_sessions_revocable_count(
                    chatter_core::services::session_service::revocable_count(&rows) as i32,
                );
                w.global::<crate::ModalHostState>()
                    .set_sessions(ModelRc::new(VecModel::from(
                        rows.into_iter().map(session_to_slint).collect::<Vec<_>>(),
                    )));
                w.global::<crate::ModalHostState>()
                    .set_sessions_loaded(true);
            }
            if let Some(rows) = pending {
                w.global::<crate::ModalHostState>()
                    .set_sessions_pending_links(ModelRc::new(VecModel::from(rows)));
            }
            w.global::<crate::ModalHostState>()
                .set_sessions_error(errors.join("\n").into());
            w.global::<crate::ModalHostState>()
                .set_sessions_notice(notice.into());
            w.global::<crate::ModalHostState>()
                .set_sessions_loading(false);
            w.global::<crate::ModalHostState>()
                .set_sessions_revoking(false);
        });
    });
}

pub fn wire_session_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window
        .global::<crate::SettingsPane>()
        .on_settings_view_sessions(move || request(weak.clone(), client.clone(), Action::List));

    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_cancel_pending_link(move |uuid| {
            request(
                weak.clone(),
                client.clone(),
                Action::Cancel(uuid.to_string()),
            )
        });

    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_revoke_session(move |id| {
            if id > 0 {
                request(weak.clone(), client.clone(), Action::Revoke(id as u32));
            }
        });

    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_revoke_all_sessions(move || request(weak.clone(), client.clone(), Action::RevokeAll));
}
