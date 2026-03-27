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
            // Use pre-built auth service from ChatterClient
            let auth_service = &ui_ctx.auth();

            // Attempt logout (ignore errors - we logout locally regardless)
            let _ = auth_service.do_logout().await;

            // Update auth state
            ui_ctx.auth_state().set_logged_out();

            // Switch to login view
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
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
