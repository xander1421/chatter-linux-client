//! Settings recovery callbacks.

use super::*;

pub(super) fn wire_pin(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Change PIN
    let weak_window = window.as_weak();
    let ui_ctx_pin = ui_ctx.clone();
    window
        .global::<crate::SettingsPane>()
        .on_settings_change_pin(move |current_pin: SharedString, new_pin: SharedString| {
            let Some(window) = weak_window.upgrade() else { return };
            let settings = window.global::<crate::SettingsPane>();
            if settings.get_settings_change_pin_loading() {
                return;
            }
            if current_pin == new_pin {
                settings.set_settings_change_pin_error(
                    "New PIN must be different from current PIN".into(),
                );
                return;
            }
            settings.set_settings_change_pin_loading(true);
            settings.set_settings_change_pin_error("".into());
            settings.set_settings_change_pin_success("".into());
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_pin.clone();
            let current_pin = current_pin.to_string();
            let new_pin = new_pin.to_string();

            tokio::spawn(async move {
                match ui_ctx.change_pin(&current_pin, &new_pin).await {
                    Ok(()) => {
                        tracing::info!("PIN changed successfully");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_change_pin_loading(false);
                                w.global::<crate::SettingsPane>()
                                    .set_settings_change_pin_success(
                                        "PIN changed successfully".into(),
                                    );
                                w.global::<crate::SettingsPane>()
                                    .set_settings_change_pin_current("".into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_change_pin_new("".into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_change_pin_confirm("".into());
                            }
                        });
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        tracing::error!("Failed to change PIN: {}", error_msg);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_change_pin_loading(false);
                                w.global::<crate::SettingsPane>()
                                    .set_settings_change_pin_error(error_msg.into());
                            }
                        });
                    }
                }
            });
        });

    // Retry SVR backup
    let weak_window = window.as_weak();
    let ui_ctx_retry = ui_ctx.clone();
    window
        .global::<crate::SettingsPane>()
        .on_settings_retry_backup(move |pin: SharedString| {
            let Some(window) = weak_window.upgrade() else { return };
            let settings = window.global::<crate::SettingsPane>();
            if settings.get_settings_backup_retry_loading() {
                return;
            }
            settings.set_settings_backup_retry_loading(true);
            settings.set_settings_backup_retry_error("".into());
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_retry.clone();
            let pin = pin.to_string();

            tokio::spawn(async move {
                let result = ui_ctx.retry_svr_backup(&pin).await;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.global::<crate::SettingsPane>()
                            .set_settings_backup_retry_loading(false);
                        match result {
                            Ok(()) => {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_backup_status("backed_up".into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_backup_retry_pin("".into());
                            }
                            Err(e) => w
                                .global::<crate::SettingsPane>()
                                .set_settings_backup_retry_error(e.user_message().into()),
                        }
                    }
                });
            });
        });
}

pub(super) fn wire_codes(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Settings › Recovery Codes › Regenerate (password-confirmed). The server
    // replaces every code; the new set is shown once in RecoveryCodesModal,
    // whose Continue returns to Settings (`recovery-codes-regenerated`).
    let ui_ctx_regen = ui_ctx.clone();
    let weak_window = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_regenerate_recovery_codes(move |password: SharedString| {
            let ui_ctx = ui_ctx_regen.clone();
            let weak = weak_window.clone();
            let password = password.to_string();
            if let Some(w) = weak.upgrade() {
                w.global::<crate::SettingsPane>()
                    .set_settings_recovery_regen_loading(true);
                w.global::<crate::SettingsPane>()
                    .set_settings_recovery_regen_error("".into());
            }
            tokio::spawn(async move {
                let result = ui_ctx.regenerate_recovery_codes(&password).await;
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    w.global::<crate::SettingsPane>()
                        .set_settings_recovery_regen_loading(false);
                    match result {
                        Ok(codes) => {
                            let model: Vec<SharedString> =
                                codes.iter().map(|c| c.as_str().into()).collect();
                            w.global::<crate::AuthFlow>()
                                .set_recovery_codes(slint::ModelRc::new(slint::VecModel::from(
                                    model,
                                )));
                            w.global::<crate::SettingsPane>()
                                .set_settings_recovery_regen_password("".into());
                            w.global::<crate::AuthFlow>()
                                .set_recovery_codes_regenerated(true);
                            crate::bin_ui::features::back::push(&w, "recovery-codes");
                        }
                        Err(e) => {
                            tracing::warn!("Recovery code regeneration failed: {e}");
                            w.global::<crate::SettingsPane>()
                                .set_settings_recovery_regen_error(
                                    format!("Couldn't make new codes. {}", e.user_message()).into(),
                                );
                        }
                    }
                });
            });
        });

    // Recovery codes confirmed (after registration)
    let weak_window = window.as_weak();
    let ui_ctx_confirm = ui_ctx.clone();
    window
        .global::<crate::AuthFlow>()
        .on_recovery_codes_confirmed(move || {
            if let Err(error) = ui_ctx_confirm.acknowledge_recovery_codes() {
                tracing::error!(%error, "Could not acknowledge recovery codes");
                return false;
            }
            if let Some(w) = weak_window.upgrade() {
                if !w
                    .global::<crate::AuthFlow>()
                    .get_recovery_codes_regenerated()
                {
                    w.set_current_view("main".into());
                }
            }
            true
        });

    // Sealed sender onboarding explainer confirmed (after recovery codes).
    // Persist the user's initial RECEIVING-policy choice to the server so the
    // column matches what they saw. Default (false) is contacts-only; sending
    // is always sealed regardless of this choice.
    let ui_ctx_intro = ui_ctx.clone();
    window
        .global::<crate::AuthFlow>()
        .on_sealed_sender_intro_confirmed(move |allow_from_anyone: bool| {
            let ui_ctx = ui_ctx_intro.clone();
            tokio::spawn(async move {
                match ui_ctx.set_sealed_sender_policy(allow_from_anyone).await {
                    Ok(persisted) => tracing::info!(
                        "Sealed sender onboarding policy persisted: allow_from_anyone={}",
                        persisted
                    ),
                    Err(e) => {
                        tracing::error!("Failed to persist sealed sender onboarding policy: {}", e)
                    }
                }
            });
        });

    // Recovery codes copy all
    window
        .global::<crate::AuthFlow>()
        .on_recovery_codes_copy_all({
            let weak = window.as_weak();
            move || {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        let codes_model = w.global::<crate::AuthFlow>().get_recovery_codes();
                        let mut all_codes = String::new();
                        for i in 0..codes_model.row_count() {
                            if i > 0 {
                                all_codes.push('\n');
                            }
                            all_codes.push_str(&codes_model.row_data(i).unwrap_or_default());
                        }

                        match crate::platform::clipboard::set_text(
                            "Recovery codes",
                            &all_codes,
                            true,
                        ) {
                            Ok(()) => {
                                tracing::info!(
                                    "Recovery codes copied to clipboard (expires in 60s)"
                                )
                            }
                            Err(e) => {
                                tracing::error!("Failed to copy recovery codes to clipboard: {}", e)
                            }
                        }
                    }
                });
            }
        });
}
