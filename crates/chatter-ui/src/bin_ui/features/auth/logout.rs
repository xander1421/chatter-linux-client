//! Logout callback wiring.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub fn wire_logout(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Session expiry uses the same native password/PIN recovery screen. Read
    // authoritative state again on the UI thread so queued old events cannot
    // route a newly authenticated account back to login.
    let mut auth_rx = ui_ctx.subscribe_auth_snapshots();
    let recovery_client = ui_ctx.clone();
    let recovery_window = window.as_weak();
    tokio::spawn(async move {
        loop {
            match auth_rx.recv().await {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
            let state = recovery_client.auth_snapshot();
            if state.current_user_id.is_some() {
                continue;
            }
            let client = recovery_client.clone();
            let weak = recovery_window.clone();
            let revision = state.session_revision;
            let _ = slint::invoke_from_event_loop(move || {
                let state = client.auth_snapshot();
                if state.current_user_id.is_some() || state.session_revision != revision {
                    return;
                }
                if let Some(w) = weak.upgrade() {
                    crate::bin_ui::features::sidebar::clear_saved_messages(&w);
                    if !state.session_ended {
                        return;
                    }
                    w.global::<crate::AuthFlow>().set_is_signed_in(false);
                    w.set_current_view("login".into());
                    crate::bin_ui::features::back::reset_to_chats(&w);
                    w.global::<crate::AuthFlow>().set_login_password("".into());
                    w.global::<crate::AuthFlow>().set_login_pin("".into());
                    w.global::<crate::AuthFlow>().set_login_loading(false);
                    w.global::<crate::AuthFlow>()
                        .set_login_error(state.login_error.unwrap_or_default().into());
                }
            });
        }
    });

    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window
        .global::<crate::SettingsPane>()
        .on_settings_logout(move || {
            let ui_ctx = ui_ctx.clone();
            let weak = weak_window.clone();

            tokio::spawn(async move {
                if let Err(error) = ui_ctx.logout().await {
                    tracing::warn!(%error, "Could not log out");
                    let message = error.user_message();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_toast_message(message.into());
                            w.set_toast_variant("error".into());
                            w.set_toast_visible(true);
                        }
                    });
                    return;
                }
                // The next account must not be shown this one's avatars and media.
                crate::bin_ui::decode_cache::on_logout();

                // Switch to login view
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        crate::bin_ui::features::composer::reset_session(&w, &ui_ctx);
                        w.global::<crate::AuthFlow>().set_is_signed_in(false);
                        w.set_current_view("login".into());
                        crate::bin_ui::features::back::reset_to_chats(&w);
                        w.global::<crate::AuthFlow>().set_login_username("".into());
                        w.global::<crate::AuthFlow>().set_login_password("".into());
                        w.global::<crate::AuthFlow>().set_login_pin("".into());
                        w.global::<crate::AuthFlow>().set_login_error("".into());
                    }
                });
            });
        });
}
