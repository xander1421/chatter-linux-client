//! Settings account callbacks.

use super::*;

pub(super) fn wire_credentials(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Change password
    let weak_window = window.as_weak();
    let ui_ctx_pw = ui_ctx.clone();

    window
        .global::<crate::SettingsPane>()
        .on_settings_change_password(move |current: SharedString, new: SharedString| {
            let Some(window) = weak_window.upgrade() else { return };
            let settings = window.global::<crate::SettingsPane>();
            if settings.get_settings_password_changing() {
                return;
            }
            settings.set_settings_password_changing(true);
            settings.set_settings_password_error("".into());
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_pw.clone();
            let current = current.to_string();
            let new = new.to_string();

            tokio::spawn(async move {
                match ui_ctx.change_password(&current, &new).await {
                    Ok(()) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_password_changing(false);
                                w.global::<crate::SettingsPane>()
                                    .set_settings_current_password("".into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_new_password("".into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_confirm_password("".into());
                            }
                        });
                        tracing::info!("Password changed successfully");
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        tracing::error!("Failed to change password: {}", error_msg);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_password_changing(false);
                                w.global::<crate::SettingsPane>()
                                    .set_settings_password_error(error_msg.into());
                            }
                        });
                    }
                }
            });
        });
}

pub(super) fn wire_profile_and_deletion(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Edit profile
    let weak_window = window.as_weak();
    let auth_state_edit = ui_ctx.clone();
    let ui_ctx_edit = ui_ctx.clone();
    window
        .global::<crate::SettingsPane>()
        .on_settings_edit_profile(move || {
            let weak = weak_window.clone();
            let auth_state = auth_state_edit.clone();
            let ui_ctx = ui_ctx_edit.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    if let Some(user_id) = auth_state.auth_snapshot().current_user_id {
                        crate::bin_ui::features::modals::open_user_profile(
                            &w,
                            &user_id,
                            Some(&ui_ctx),
                        );
                        crate::bin_ui::features::back::push(&w, "user-profile");
                        // The button says Edit, so open the editor, over the
                        // profile it edits (REVIEW-R4 P2-24). open_user_profile
                        // sets the own name and bio synchronously, which the
                        // editor copies.
                        w.global::<crate::ModalHostState>()
                            .invoke_profile_edit_profile();
                    }
                }
            });
        });

    // Delete account — first click. The Slint layer reveals an inline
    // confirmation step (password field + "Delete Forever"); this handler does
    // NOT delete. Actual deletion only happens via
    // `on_settings_delete_account_confirmed` below, after explicit confirm.
    let weak_window = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_delete_account(move || {
            let weak = weak_window.clone();
            tracing::info!("Delete account requested — showing confirmation step");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Clear any stale error from a previous attempt.
                    w.global::<crate::SettingsPane>()
                        .set_settings_delete_error("".into());
                }
            });
        });

    // Delete account — CONFIRMED (carries the password entered in the confirm
    // step). Reaches the `ChatterClient::delete_account` wrapper. On success:
    // clear local state and route to login. On failure: surface the error in the
    // confirm panel (do not silently swallow).
    let ui_ctx_del = ui_ctx.clone();
    let weak_window = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_delete_account_confirmed(move |password: SharedString| {
            let Some(window) = weak_window.upgrade() else { return };
            let settings = window.global::<crate::SettingsPane>();
            if settings.get_settings_delete_loading() {
                return;
            }
            settings.set_settings_delete_loading(true);
            settings.set_settings_delete_error("".into());
            let ui_ctx = ui_ctx_del.clone();
            let weak = weak_window.clone();
            let password = password.to_string();

            tokio::spawn(async move {
                match ui_ctx.delete_account(&password).await {
                    Ok(()) => {
                        // Account is gone server-side and local auth state is cleared
                        // by the wrapper. Reflect it in the UI and route to login.
                        crate::bin_ui::decode_cache::on_logout();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_delete_loading(false);
                                crate::bin_ui::features::composer::reset_session(&w, &ui_ctx);
                                w.global::<crate::SettingsPane>()
                                    .set_settings_delete_password("".into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_delete_error("".into());
                                w.global::<crate::AuthFlow>().set_is_signed_in(false);
                                w.set_current_view("login".into());
                                crate::bin_ui::features::back::reset_to_chats(&w);
                                w.global::<crate::AuthFlow>().set_login_username("".into());
                                w.global::<crate::AuthFlow>().set_login_password("".into());
                                w.global::<crate::AuthFlow>().set_login_error("".into());
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Account deletion failed: {e}");
                        let msg = format!("Deletion failed: {e}");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_delete_loading(false);
                                w.global::<crate::SettingsPane>()
                                    .set_settings_delete_error(msg.into());
                            }
                        });
                    }
                }
            });
        });
}
