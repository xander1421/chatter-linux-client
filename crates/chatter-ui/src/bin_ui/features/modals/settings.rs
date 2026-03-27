//! Settings modal callbacks.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub fn wire_settings_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let signal_service = ui_ctx.signal().clone();
    let session_service = ui_ctx.sessions().clone();

    // Change password
    let weak_window = window.as_weak();
    let ui_ctx_pw = ui_ctx.clone();

    window.on_settings_change_password(move |current: SharedString, new: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_pw.clone();
        let current = current.to_string();
        let new = new.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_settings_password_changing(true);
                    w.set_settings_password_error("".into());
                }
            });

            match ui_ctx.change_password(&current, &new).await {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_password_changing(false);
                            w.set_settings_current_password("".into());
                            w.set_settings_new_password("".into());
                            w.set_settings_confirm_password("".into());
                        }
                    });
                    tracing::info!("Password changed successfully");
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    tracing::error!("Failed to change password: {}", error_msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_password_changing(false);
                            w.set_settings_password_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    // Read receipts toggle
    let ui_ctx_read = ui_ctx.clone();
    window.on_settings_read_receipts_changed(move |enabled: bool| {
        let _ = ui_ctx_read.set_read_receipts_enabled(enabled);
    });

    // Typing indicators toggle
    let ui_ctx_typing = ui_ctx.clone();
    window.on_settings_typing_indicators_changed(move |enabled: bool| {
        let _ = ui_ctx_typing.set_typing_indicators_enabled(enabled);
    });

    // Copy fingerprint
    let signal_service_fp = signal_service.clone();
    window.on_settings_copy_fingerprint(move || {
        let signal_service = signal_service_fp.clone();
        tokio::spawn(async move {
            let fingerprint = match signal_service.get_identity_fingerprint().await {
                Ok(fp) => fp,
                Err(e) => {
                    tracing::error!("Failed to get identity fingerprint: {}", e);
                    return;
                }
            };

            #[cfg(not(target_os = "android"))]
            {
                use chatter_core::system::Clipboard;
                match Clipboard::new() {
                    Ok(mut clipboard) => {
                        if let Err(e) = clipboard.set_text(&fingerprint) {
                            tracing::error!("Failed to copy fingerprint to clipboard: {}", e);
                        } else {
                            tracing::info!("Fingerprint copied to clipboard");
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to access clipboard: {}", e);
                    }
                }
            }
            #[cfg(target_os = "android")]
            tracing::warn!("Clipboard not supported on Android");
        });
    });

    // Sealed sender toggle
    let session_service_sealed = session_service.clone();
    window.on_settings_sealed_sender_changed(move |unrestricted: bool| {
        if let Err(e) = session_service_sealed.set_bool_setting("privacy.sealed_sender_unrestricted", unrestricted) {
            tracing::error!("Failed to save sealed sender setting: {}", e);
        } else {
            tracing::debug!(
                "Sealed sender unrestricted setting changed to: {}",
                unrestricted
            );
        }
    });

    // Refresh prekeys
    let weak_window = window.as_weak();
    let ui_ctx_keys = ui_ctx.clone();

    window.on_settings_refresh_keys(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_keys.clone();

        tokio::spawn(async move {
            match ui_ctx.refresh_prekeys(100).await {
                Ok(count) => {
                    let count = count as i32;
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_prekey_count(count);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to refresh prekeys: {}", e);
                }
            }
        });
    });

    // Change PIN
    let weak_window = window.as_weak();
    let ui_ctx_pin = ui_ctx.clone();
    window.on_settings_change_pin(move |current_pin: SharedString, new_pin: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_pin.clone();
        let current_pin = current_pin.to_string();
        let new_pin = new_pin.to_string();

        tokio::spawn(async move {
            // SECURITY: Early check - new PIN must differ from current PIN
            if current_pin == new_pin {
                let weak_err = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_err.upgrade() {
                        w.set_settings_change_pin_error("New PIN must be different from current PIN".into());
                    }
                });
                return;
            }

            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_settings_change_pin_loading(true);
                    w.set_settings_change_pin_error("".into());
                    w.set_settings_change_pin_success("".into());
                }
            });

            // Get username from auth state
            let username = ui_ctx.auth_state().get().display_name.unwrap_or_default();

            match ui_ctx.change_pin(&current_pin, &new_pin, &username).await {
                Ok(()) => {
                    tracing::info!("PIN changed successfully");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_change_pin_loading(false);
                            w.set_settings_change_pin_success("PIN changed successfully".into());
                            w.set_settings_change_pin_current("".into());
                            w.set_settings_change_pin_new("".into());
                            w.set_settings_change_pin_confirm("".into());
                        }
                    });
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    tracing::error!("Failed to change PIN: {}", error_msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_change_pin_loading(false);
                            w.set_settings_change_pin_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    // Retry SVR backup
    // Note: retry_svr_backup(pin, username, password) requires the plaintext password
    // for key derivation. Since passwords are never stored, we inform the user.
    let weak_window = window.as_weak();
    window.on_settings_retry_backup(move |_pin: SharedString| {
        let weak = weak_window.clone();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_settings_backup_retry_loading(true);
                    w.set_settings_backup_retry_error("".into());
                }
            });

            // retry_svr_backup(pin, username, password) requires the plaintext password
            // for key derivation. Since passwords are never stored, show guidance.
            let error_msg = "Retry backup requires your password for key derivation. Please log out and log in again to retry the backup automatically.".to_string();
            tracing::warn!("SVR backup retry attempted but password not available");
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_settings_backup_retry_loading(false);
                    w.set_settings_backup_retry_error(error_msg.into());
                }
            });
        });
    });

    // Recovery codes confirmed (after registration)
    let weak_window = window.as_weak();
    window.on_recovery_codes_confirmed(move || {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Modal is already set to "none" by the Slint callback
                // Just ensure we're in main view
                w.set_current_view("main".into());
            }
        });
    });

    // Recovery codes copy all
    window.on_recovery_codes_copy_all({
        let weak = window.as_weak();
        move || {
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    let codes_model = w.get_recovery_codes();
                    let mut all_codes = String::new();
                    for i in 0..codes_model.row_count() {
                        if i > 0 {
                            all_codes.push('\n');
                        }
                        all_codes.push_str(&codes_model.row_data(i).unwrap_or_default());
                    }

                    #[cfg(not(target_os = "android"))]
                    {
                        use chatter_core::system::Clipboard;
                        match Clipboard::new() {
                            Ok(mut clipboard) => {
                                if let Err(e) = clipboard.set_text(&all_codes) {
                                    tracing::error!("Failed to copy recovery codes to clipboard: {}", e);
                                } else {
                                    tracing::info!("Recovery codes copied to clipboard (auto-clears in 60s)");
                                    // SECURITY: Auto-clear clipboard after 60 seconds
                                    std::thread::spawn(move || {
                                        std::thread::sleep(std::time::Duration::from_secs(60));
                                        let _ = slint::invoke_from_event_loop(move || {
                                            if let Ok(mut cb) = Clipboard::new() {
                                                if let Err(e) = cb.clear() {
                                                    tracing::warn!("Failed to auto-clear clipboard: {}", e);
                                                } else {
                                                    tracing::info!("Clipboard auto-cleared after 60s");
                                                }
                                            }
                                        });
                                    });
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to access clipboard: {}", e);
                            }
                        }
                    }
                    #[cfg(target_os = "android")]
                    tracing::warn!("Clipboard not supported on Android");
                }
            });
        }
    });

    // Edit profile
    let weak_window = window.as_weak();
    let auth_state_edit = ui_ctx.auth_state().clone();
    let user_state_edit = ui_ctx.user_state().clone();
    window.on_settings_edit_profile(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state_edit.clone();
        let user_state = user_state_edit.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if let Some(user_id) = auth_state.get().current_user_id {
                    crate::bin_ui::features::modals::open_user_profile(
                        &w, &user_id, &auth_state, &user_state,
                    );
                }
            }
        });
    });

    // Delete account (placeholder)
    let weak_window = window.as_weak();
    window.on_settings_delete_account(move || {
        let weak = weak_window.clone();
        tracing::info!("Delete account requested - showing confirmation would be needed");

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(_w) = weak.upgrade() {
                // Future: Show delete confirmation modal
            }
        });
    });

    // Local AI enabled toggle
    let local_ai_service = ui_ctx.local_ai().clone();
    window.on_settings_local_ai_enabled_changed(move |enabled: bool| {
        if let Err(e) = local_ai_service.set_enabled(enabled) {
            tracing::error!("Failed to save local AI enabled setting: {}", e);
        } else {
            tracing::debug!("Local AI enabled setting changed to: {}", enabled);
        }
    });

    // Load AI model
    let weak_window = window.as_weak();
    let local_ai_service = ui_ctx.local_ai().clone();
    window.on_settings_local_ai_load_model(move |model_id: SharedString| {
        let weak = weak_window.clone();
        let local_ai = local_ai_service.clone();
        let model_id = model_id.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_settings_local_ai_loading(true);
                    w.set_settings_local_ai_error("".into());
                    w.set_settings_local_ai_status("Loading...".into());
                }
            });

            // Save model preference first
            if let Err(e) = local_ai.set_preferred_model(&model_id, None) {
                let error_msg = e.to_string();
                tracing::error!("Failed to save model preference: {}", error_msg);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_settings_local_ai_loading(false);
                        w.set_settings_local_ai_error(error_msg.into());
                        w.set_settings_local_ai_status("".into());
                    }
                });
                return;
            }

            // Load the model
            match local_ai.load_model(model_id.clone(), None).await {
                Ok(info) => {
                    let status = format!("Loaded: {}", info.model_id);
                    tracing::info!("Local AI model loaded: {}", info.model_id);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_local_ai_loading(false);
                            w.set_settings_local_ai_status(status.into());
                            w.set_settings_local_ai_error("".into());
                        }
                    });
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    tracing::error!("Failed to load local AI model: {}", error_msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_local_ai_loading(false);
                            w.set_settings_local_ai_error(error_msg.into());
                            w.set_settings_local_ai_status("".into());
                        }
                    });
                }
            }
        });
    });

    // Unload AI model
    let weak_window = window.as_weak();
    let local_ai_service = ui_ctx.local_ai().clone();
    window.on_settings_local_ai_unload_model(move || {
        let weak = weak_window.clone();
        let local_ai = local_ai_service.clone();

        tokio::spawn(async move {
            match local_ai.unload_model().await {
                Ok(()) => {
                    tracing::info!("Local AI model unloaded");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_local_ai_status("".into());
                            w.set_settings_local_ai_error("".into());
                        }
                    });
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    tracing::error!("Failed to unload local AI model: {}", error_msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_local_ai_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });
}
