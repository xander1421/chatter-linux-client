//! Logout callback wiring.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub fn wire_logout(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Session expiry uses the same native password/PIN recovery screen. Read
    // authoritative state again on the UI thread so queued old events cannot
    // route a newly authenticated account back to login.
    let mut auth_rx = ui_ctx.auth_state().subscribe();
    let recovery_client = ui_ctx.clone();
    let recovery_window = window.as_weak();
    tokio::spawn(async move {
        loop {
            match auth_rx.recv().await {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
            let state = recovery_client.auth_state().get();
            if !state.session_ended {
                continue;
            }
            let client = recovery_client.clone();
            let weak = recovery_window.clone();
            let revision = state.session_revision;
            let _ = slint::invoke_from_event_loop(move || {
                let state = client.auth_state().get();
                if !state.session_ended || state.session_revision != revision {
                    return;
                }
                if let Some(w) = weak.upgrade() {
                    w.set_is_signed_in(false);
                    w.set_current_view("login".into());
                    crate::bin_ui::features::back::reset_to_chats(&w);
                    w.set_login_password("".into());
                    w.set_login_loading(false);
                    w.set_login_error(state.login_error.unwrap_or_default().into());
                }
            });
        }
    });

    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_settings_logout(move || {
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();

        if let Some(w) = weak.upgrade() {
            crate::bin_ui::features::composer::reset_session(&w, &ui_ctx);
        }

        tokio::spawn(async move {
            // Local sign-out and teardown happen even if the server call fails.
            if let Err(e) = ui_ctx.logout().await {
                tracing::warn!("logout: {e}");
            }
            // The next account must not be shown this one's avatars and media.
            crate::bin_ui::decode_cache::on_logout();

            // Switch to login view
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_is_signed_in(false);
                    w.set_current_view("login".into());
                    crate::bin_ui::features::back::reset_to_chats(&w);
                    w.set_login_username("".into());
                    w.set_login_password("".into());
                    w.set_login_error("".into());
                }
            });
        });
    });
}
