//! Account recovery callback wiring.

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub fn wire_recovery(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.global::<crate::AuthFlow>().on_recover_account(move |username: slint::SharedString, recovery_code: slint::SharedString, new_password: slint::SharedString, new_pin: slint::SharedString| {
        let ui_ctx = ui_ctx.clone();
        let weak = weak_window.clone();
        let username = username.to_string();
        let recovery_code = recovery_code.to_string();
        let new_password = new_password.to_string();
        let new_pin = new_pin.to_string();

        if let Some(w) = weak.upgrade() {
            if w.global::<crate::AuthFlow>().get_recovery_loading() {
                return;
            }
            w.global::<crate::AuthFlow>().set_recovery_loading(true);
            w.global::<crate::AuthFlow>().set_recovery_error("".into());
            w.global::<crate::AuthFlow>().set_recovery_success("".into());
        }
        tokio::spawn(async move {
            match ui_ctx.use_recovery_code(&username, &recovery_code, &new_password, &new_pin).await {
                Ok(result) => {
                    // Surface key_backup_invalidated (§7): recovery rotates the
                    // PIN, and the SVR key + message backups were encrypted under
                    // and indexed by a secret derived from the OLD PIN, so the
                    // server drops them — they are gone, not merely stale. The
                    // user must be told, or they believe a backup still protects
                    // them when none does. It is destroyed on every recovery, so
                    // the notice fires whenever the flag is set.
                    let backup_notice = if result.key_backup_invalidated {
                        " Your previous PIN-based backup was invalidated by this recovery — set up a new backup from Settings after signing in."
                    } else {
                        ""
                    };
                    let msg = format!(
                        "Account recovered successfully! {} recovery codes remaining.{} Please sign in with your new credentials.",
                        result.remaining_codes, backup_notice
                    );
                    tracing::info!(
                        "Account recovery succeeded, {} codes remaining, key_backup_invalidated={}",
                        result.remaining_codes, result.key_backup_invalidated
                    );

                    let weak_for_success = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_for_success.upgrade() {
                            w.global::<crate::AuthFlow>().set_recovery_loading(false);
                            w.global::<crate::AuthFlow>().set_recovery_success(msg.into());
                            w.global::<crate::AuthFlow>().set_recovery_error("".into());
                            w.global::<crate::AuthFlow>().set_recovery_code("".into());
                            w.global::<crate::AuthFlow>().set_recovery_new_password("".into());
                            w.global::<crate::AuthFlow>().set_recovery_confirm_password("".into());
                            w.global::<crate::AuthFlow>().set_recovery_new_pin("".into());
                        }
                    });

                    // After a short delay, switch to login view.
                    // NOTE: Recovery is unauthenticated (no active session to invalidate).
                    // Server-side session invalidation is handled in UseRecoveryCode.
                    // We clear all form fields to ensure no sensitive data lingers in UI state.
                    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            if w.get_current_view().as_str() != "recovery"
                                || w.global::<crate::AuthFlow>().get_recovery_loading()
                                || w.global::<crate::AuthFlow>()
                                    .get_recovery_success()
                                    .as_str()
                                    .is_empty()
                            {
                                return;
                            }
                            w.set_current_view("login".into());
                            // Clear recovery form fields and any cached auth state
                            w.global::<crate::AuthFlow>().set_recovery_username("".into());
                            w.global::<crate::AuthFlow>().set_recovery_code("".into());
                            w.global::<crate::AuthFlow>().set_recovery_new_password("".into());
                            w.global::<crate::AuthFlow>().set_recovery_confirm_password("".into());
                            w.global::<crate::AuthFlow>().set_recovery_new_pin("".into());
                            w.global::<crate::AuthFlow>().set_recovery_success("".into());
                            // Also clear login fields so user starts fresh
                            w.global::<crate::AuthFlow>().set_login_username("".into());
                            w.global::<crate::AuthFlow>().set_login_password("".into());
                            w.global::<crate::AuthFlow>().set_login_pin("".into());
                            w.global::<crate::AuthFlow>().set_login_error("".into());
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Account recovery failed");
                    let error_msg = e.user_message();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.global::<crate::AuthFlow>().set_recovery_loading(false);
                            w.global::<crate::AuthFlow>().set_recovery_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });
}
