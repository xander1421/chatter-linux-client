//! Logout callback wiring.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub fn wire_logout(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_settings_logout(move || {
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();

        tokio::spawn(async move {
            // Local sign-out and teardown happen even if the server call fails.
            if let Err(e) = ui_ctx.logout().await {
                tracing::warn!("logout: {e}");
            }

            // Switch to login view
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_is_signed_in(false);
                    w.set_current_view("login".into());
                    w.set_active_modal("none".into());
                    w.set_login_username("".into());
                    w.set_login_password("".into());
                    w.set_login_error("".into());
                }
            });
        });
    });
}
