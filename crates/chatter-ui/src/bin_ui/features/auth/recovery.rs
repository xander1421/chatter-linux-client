//! Account recovery callback wiring.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub fn wire_recovery(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_recover_account(move |username: slint::SharedString, recovery_code: slint::SharedString, new_password: slint::SharedString, new_pin: slint::SharedString| {
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();
        let username = username.to_string();
        let recovery_code = recovery_code.to_string();
        let new_password = new_password.to_string();
        let new_pin = new_pin.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_for_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_for_loading.upgrade() {
                    w.set_recovery_loading(true);
                    w.set_recovery_error("".into());
                    w.set_recovery_success("".into());
                }
            });

            match ui_ctx.use_recovery_code(&username, &recovery_code, &new_password, &new_pin).await {
                Ok(result) => {
                    let msg = format!(
                        "Account recovered successfully! {} recovery codes remaining. Please sign in with your new credentials.",
                        result.remaining_codes
                    );
                    tracing::info!("Account recovery succeeded, {} codes remaining", result.remaining_codes);

                    let weak_for_success = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_for_success.upgrade() {
                            w.set_recovery_loading(false);
                            w.set_recovery_success(msg.into());
                            w.set_recovery_error("".into());
                        }
                    });

                    // After a short delay, switch to login view.
                    // NOTE: Recovery is unauthenticated (no active session to invalidate).
                    // Server-side session invalidation is handled in UseRecoveryCode.
                    // We clear all form fields to ensure no sensitive data lingers in UI state.
                    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_current_view("login".into());
                            // Clear recovery form fields and any cached auth state
                            w.set_recovery_username("".into());
                            w.set_recovery_code("".into());
                            w.set_recovery_new_password("".into());
                            w.set_recovery_confirm_password("".into());
                            w.set_recovery_new_pin("".into());
                            w.set_recovery_success("".into());
                            // Also clear login fields so user starts fresh
                            w.set_login_username("".into());
                            w.set_login_password("".into());
                            w.set_login_pin("".into());
                            w.set_login_error("".into());
                        }
                    });
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    tracing::error!("Account recovery failed: {}", error_msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_recovery_loading(false);
                            w.set_recovery_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });
}
