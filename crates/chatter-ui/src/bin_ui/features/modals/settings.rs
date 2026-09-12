//! Settings modal callbacks.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;

use crate::MainWindow;

pub fn wire_settings_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {

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

    // Search shortcut selector
    let ui_ctx_shortcut = ui_ctx.clone();
    let weak_shortcut = window.as_weak();
    window.on_settings_search_shortcut_changed(move |value: SharedString| {
        let shortcut = chatter_core::services::search::SearchShortcut::from_str(value.as_str());
        if let Err(e) = ui_ctx_shortcut.set_search_shortcut(shortcut) {
            tracing::error!("Failed to save search shortcut: {}", e);
            return;
        }
        if let Some(w) = weak_shortcut.upgrade() {
            w.set_search_shortcut_pref(shortcut.as_str().into());
        }
    });

    // Copy fingerprint
    let ui_ctx_fp = ui_ctx.clone();
    window.on_settings_copy_fingerprint(move || {
        let ui_ctx = ui_ctx_fp.clone();
        tokio::spawn(async move {
            let fingerprint = match ui_ctx.my_identity_fingerprint().await {
                Ok(fp) => fp,
                Err(e) => {
                    tracing::error!("Failed to get identity fingerprint: {}", e);
                    return;
                }
            };

            #[cfg(not(any(target_os = "android", target_os = "ios")))]
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
            match crate::platform::android::clipboard::set_text("Safety number", &fingerprint) {
                Ok(()) => tracing::info!("Fingerprint copied to clipboard"),
                Err(e) => tracing::error!("Failed to copy fingerprint to clipboard: {}", e),
            }
            #[cfg(target_os = "ios")]
            tracing::warn!("Clipboard not implemented on iOS");
        });
    });

    // Sealed sender RECEIVING policy toggle.
    // "Allow sealed sender from anyone" governs who may send sealed messages TO
    // this user (users.unrestricted_unidentified_access on the server): off =
    // contacts-only (only holders of your UAK), on = anyone. This hits the
    // SERVER (SetSealedSenderPolicy RPC) — it changes server enforcement, not a
    // local bool. Sealed *sending* is always on and unaffected. On failure we
    // revert the toggle so the UI reflects the true server state.
    let ui_ctx_sealed = ui_ctx.clone();
    let weak_sealed = window.as_weak();
    window.on_settings_sealed_sender_changed(move |allow_from_anyone: bool| {
        let ui_ctx = ui_ctx_sealed.clone();
        let weak = weak_sealed.clone();
        tokio::spawn(async move {
            match ui_ctx.set_sealed_sender_policy(allow_from_anyone).await {
                Ok(persisted) => {
                    tracing::info!(
                        "Sealed sender receiving policy set on server: allow_from_anyone={}",
                        persisted
                    );
                    // Reconcile the toggle with the server-persisted value.
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_sealed_sender_unrestricted(persisted);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to set sealed sender policy on server: {}", e);
                    // Revert the toggle to the opposite of the attempted value.
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_sealed_sender_unrestricted(!allow_from_anyone);
                        }
                    });
                }
            }
        });
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

    // ── Message-history backup (L7) ─────────────────────────────────
    // Thin wires only: native file pickers + status text live here; ALL
    // backup logic (KDF, container crypto, dump/import, server RPCs) lives
    // in chatter-core behind the ChatterClient façade methods.

    fn set_message_backup_status(weak: &slint::Weak<MainWindow>, status: String) {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_settings_message_backup_status(status.into());
            }
        });
    }

    // Export to file (save picker → core export)
    let ui_ctx_mb_export = ui_ctx.clone();
    let weak_mb_export = window.as_weak();
    window.on_settings_export_message_backup(move |passphrase: SharedString| {
        let ui_ctx = ui_ctx_mb_export.clone();
        let weak = weak_mb_export.clone();
        let passphrase = passphrase.to_string();
        tokio::spawn(async move {
            // `FilePicker` is `#[cfg(not(target_os = "android"))]` in core
            // (media/mod.rs:33). Without this gate the Android target does not
            // compile at all — which is how it has been on this branch since
            // the backup UI landed. Tell the user instead of failing the build.
            #[cfg(target_os = "android")]
            {
                let _ = (&ui_ctx, &passphrase);
                set_message_backup_status(
                    &weak,
                    "Export to a file is not available on Android yet — use \"Back up to server\"".to_string(),
                );
            }
            #[cfg(not(target_os = "android"))]
            {
                use chatter_core::media::{FileFilter, FilePicker, FilePickerConfig};
                let Some(path) = FilePicker::save_file(
                    FilePickerConfig::new()
                        .with_title("Export message backup")
                        .with_filter(FileFilter::new("Chatter backup", vec!["chbk"])),
                )
                .await
                else {
                    return; // user cancelled
                };
                let status = match ui_ctx.export_message_backup(&path, &passphrase).await {
                    Ok(stats) => format!("Backup exported ({} rows)", stats.rows),
                    Err(e) => format!("Export failed: {e}"),
                };
                set_message_backup_status(&weak, status);
            }
        });
    });

    // Restore from file (open picker → core import)
    let ui_ctx_mb_import = ui_ctx.clone();
    let weak_mb_import = window.as_weak();
    window.on_settings_import_message_backup(move |passphrase: SharedString| {
        let ui_ctx = ui_ctx_mb_import.clone();
        let weak = weak_mb_import.clone();
        let passphrase = passphrase.to_string();
        tokio::spawn(async move {
            // See the export closure above — same cfg gate, same reason.
            #[cfg(target_os = "android")]
            {
                let _ = (&ui_ctx, &passphrase);
                set_message_backup_status(
                    &weak,
                    "Restore from a file is not available on Android yet".to_string(),
                );
            }
            #[cfg(not(target_os = "android"))]
            {
                use chatter_core::media::{FileFilter, FilePicker, FilePickerConfig};
                let Some(path) = FilePicker::pick_file(
                    FilePickerConfig::new()
                        .with_title("Restore message backup")
                        .with_filter(FileFilter::new("Chatter backup", vec!["chbk"])),
                )
                .await
                else {
                    return; // user cancelled
                };
                let status = match ui_ctx.import_message_backup(&path, &passphrase).await {
                    Ok(s) => format!("Restored {} of {} rows (rest already present)", s.rows_inserted, s.rows_seen),
                    Err(e) => format!("Restore failed: {e}"),
                };
                set_message_backup_status(&weak, status);
            }
        });
    });

    // Back up to server (PIN-keyed, server never sees content)
    let ui_ctx_mb_server = ui_ctx.clone();
    let weak_mb_server = window.as_weak();
    window.on_settings_server_message_backup(move |pin: SharedString| {
        let ui_ctx = ui_ctx_mb_server.clone();
        let weak = weak_mb_server.clone();
        let pin = pin.to_string();
        tokio::spawn(async move {
            let status = match ui_ctx.enable_server_message_backup(&pin).await {
                Ok(stats) => format!("Server backup updated ({} rows)", stats.rows),
                Err(e) => format!("Server backup failed: {e}"),
            };
            set_message_backup_status(&weak, status);
        });
    });

    // Restore from server (fresh install after login + PIN)
    let ui_ctx_mb_restore = ui_ctx.clone();
    let weak_mb_restore = window.as_weak();
    window.on_settings_restore_message_backup(move |pin: SharedString| {
        let ui_ctx = ui_ctx_mb_restore.clone();
        let weak = weak_mb_restore.clone();
        let pin = pin.to_string();
        tokio::spawn(async move {
            let status = match ui_ctx.restore_server_message_backup(&pin).await {
                Ok(s) => format!("Restored {} of {} rows (rest already present)", s.rows_inserted, s.rows_seen),
                Err(e) => format!("Server restore failed: {e}"),
            };
            set_message_backup_status(&weak, status);
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

    // Sealed sender onboarding explainer confirmed (after recovery codes).
    // Persist the user's initial RECEIVING-policy choice to the server so the
    // column matches what they saw. Default (false) is contacts-only; sending
    // is always sealed regardless of this choice.
    let ui_ctx_intro = ui_ctx.clone();
    window.on_sealed_sender_intro_confirmed(move |allow_from_anyone: bool| {
        let ui_ctx = ui_ctx_intro.clone();
        tokio::spawn(async move {
            match ui_ctx.set_sealed_sender_policy(allow_from_anyone).await {
                Ok(persisted) => tracing::info!(
                    "Sealed sender onboarding policy persisted: allow_from_anyone={}",
                    persisted
                ),
                Err(e) => tracing::error!(
                    "Failed to persist sealed sender onboarding policy: {}",
                    e
                ),
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

                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
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
                    match crate::platform::android::clipboard::set_text(
                        "Recovery codes",
                        &all_codes,
                    ) {
                        Ok(()) => {
                            tracing::info!(
                                "Recovery codes copied to clipboard (auto-clears in 60s)"
                            );
                            // Same 60s wipe as the desktop path: recovery codes
                            // unlock the encrypted key backup, so they must not
                            // sit in the clipboard indefinitely.
                            std::thread::spawn(|| {
                                std::thread::sleep(std::time::Duration::from_secs(60));
                                if let Err(e) =
                                    crate::platform::android::clipboard::set_text("", "")
                                {
                                    tracing::warn!("Failed to auto-clear clipboard: {}", e);
                                } else {
                                    tracing::info!("Clipboard auto-cleared after 60s");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("Failed to copy recovery codes to clipboard: {}", e)
                        }
                    }
                    #[cfg(target_os = "ios")]
                    tracing::warn!("Clipboard not implemented on iOS");
                }
            });
        }
    });

    // Edit profile
    let weak_window = window.as_weak();
    let auth_state_edit = ui_ctx.auth_state().clone();
    let user_state_edit = ui_ctx.user_state().clone();
    let ui_ctx_edit = ui_ctx.clone();
    window.on_settings_edit_profile(move || {
        let weak = weak_window.clone();
        let auth_state = auth_state_edit.clone();
        let user_state = user_state_edit.clone();
        let ui_ctx = ui_ctx_edit.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                if let Some(user_id) = auth_state.get().current_user_id {
                    crate::bin_ui::features::modals::open_user_profile(
                        &w, &user_id, &auth_state, &user_state, Some(&ui_ctx),
                    );
                }
            }
        });
    });

    // Delete account — first click. The Slint layer reveals an inline
    // confirmation step (password field + "Delete Forever"); this handler does
    // NOT delete. Actual deletion only happens via
    // `on_settings_delete_account_confirmed` below, after explicit confirm.
    let weak_window = window.as_weak();
    window.on_settings_delete_account(move || {
        let weak = weak_window.clone();
        tracing::info!("Delete account requested — showing confirmation step");
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                // Clear any stale error from a previous attempt.
                w.set_settings_delete_error("".into());
            }
        });
    });

    // Delete account — CONFIRMED (carries the password entered in the confirm
    // step). Reaches the `ChatterClient::delete_account` wrapper. On success:
    // clear local state and route to login. On failure: surface the error in the
    // confirm panel (do not silently swallow).
    let ui_ctx_del = ui_ctx.clone();
    let weak_window = window.as_weak();
    window.on_settings_delete_account_confirmed(move |password: SharedString| {
        let ui_ctx = ui_ctx_del.clone();
        let weak = weak_window.clone();
        let password = password.to_string();

        // Enter loading; clear prior error.
        let weak_loading = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_loading.upgrade() {
                w.set_settings_delete_loading(true);
                w.set_settings_delete_error("".into());
            }
        });

        tokio::spawn(async move {
            match ui_ctx.delete_account(&password).await {
                Ok(()) => {
                    // Account is gone server-side and local auth state is cleared
                    // by the wrapper. Reflect it in the UI and route to login.
                    ui_ctx.auth_state().set_logged_out();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_delete_loading(false);
                            w.set_settings_delete_password("".into());
                            w.set_settings_delete_error("".into());
                            w.set_is_signed_in(false);
                            w.set_current_view("login".into());
                            w.set_active_modal("none".into());
                            w.set_login_username("".into());
                            w.set_login_password("".into());
                            w.set_login_error("".into());
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Account deletion failed: {e}");
                    let msg = format!("Deletion failed: {e}");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_delete_loading(false);
                            w.set_settings_delete_error(msg.into());
                        }
                    });
                }
            }
        });
    });

    // Local AI enabled toggle
    let ui_ctx_ai = ui_ctx.clone();
    let weak_ai_toggle = window.as_weak();
    window.on_settings_local_ai_enabled_changed(move |enabled: bool| {
        if let Err(e) = ui_ctx_ai.set_local_ai_enabled(enabled) {
            tracing::error!("Failed to save local AI enabled setting: {}", e);
            return;
        }
        tracing::debug!("Local AI enabled setting changed to: {}", enabled);
        // The AI channel's sidebar visibility follows this setting — rebuild
        // the channel list live.
        #[cfg(feature = "jarvis")]
        if let Some(w) = weak_ai_toggle.upgrade() {
            let channels: Vec<_> = ui_ctx_ai
                .sidebar_channels()
                .iter()
                .map(|c| crate::bin_ui::converters::channel_to_slint(c, &ui_ctx_ai))
                .collect();
            w.set_channels(slint::ModelRc::new(slint::VecModel::from(channels)));
        }
    });

    // Load AI model
    let weak_window = window.as_weak();
    let ui_ctx_load = ui_ctx.clone();
    window.on_settings_local_ai_load_model(move |model_id: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_load.clone();
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
            if let Err(e) = ui_ctx.set_local_ai_preferred_model(&model_id, None) {
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
            match ui_ctx.load_local_ai_model(model_id.clone(), None).await {
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
    let ui_ctx_unload = ui_ctx.clone();
    window.on_settings_local_ai_unload_model(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_unload.clone();

        tokio::spawn(async move {
            match ui_ctx.unload_local_ai_model().await {
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

    // ── Cloud AI ─────────────────────────────────────────────────────────
    //
    // Thin bindings only: which silo may actually run is decided by
    // chatter-core's ai_policy, not here. The UI records a *preference*; a DM
    // or mesh conversation still resolves to local regardless of it.

    // Prefer the cloud silo where policy allows it.
    let ui_ctx_pref = ui_ctx.clone();
    window.on_settings_ai_prefer_cloud_changed(move |prefer: bool| {
        use chatter_core::ai_policy::channel_ai::SiloPreference;
        let preference = if prefer {
            SiloPreference::PreferCloud
        } else {
            SiloPreference::LocalOnly
        };
        if let Err(e) = ui_ctx_pref.set_ai_silo_preference(preference) {
            tracing::error!("Failed to save AI silo preference: {}", e);
        }
    });

    // Own API key vs metered server broker.
    let ui_ctx_mode = ui_ctx.clone();
    let weak_mode = window.as_weak();
    window.on_settings_cloud_ai_mode_changed(move |mode: SharedString| {
        use chatter_core::cloud_ai::CloudMode;
        let Some(parsed) = CloudMode::parse(mode.as_str()) else {
            tracing::error!("Unknown cloud AI mode: {}", mode);
            return;
        };
        if let Err(e) = ui_ctx_mode.set_cloud_ai_mode(parsed) {
            tracing::error!("Failed to save cloud AI mode: {}", e);
            return;
        }
        if let Some(w) = weak_mode.upgrade() {
            let settings = ui_ctx_mode.cloud_ai_settings();
            w.set_settings_cloud_ai_configured(settings.is_available());
            w.set_settings_cloud_ai_status("".into());
        }
    });

    // Store the user's own provider key. Blank clears it.
    let ui_ctx_key = ui_ctx.clone();
    let weak_key = window.as_weak();
    window.on_settings_cloud_ai_api_key_changed(move |key: SharedString| {
        match ui_ctx_key.set_cloud_ai_api_key(key.as_str()) {
            Ok(()) => {
                if let Some(w) = weak_key.upgrade() {
                    let settings = ui_ctx_key.cloud_ai_settings();
                    w.set_settings_cloud_ai_configured(settings.is_available());
                    w.set_settings_cloud_ai_status(
                        if settings.is_available() { "API key saved." } else { "API key cleared." }
                            .into(),
                    );
                }
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::error!("Failed to save cloud AI API key: {}", msg);
                if let Some(w) = weak_key.upgrade() {
                    w.set_settings_cloud_ai_status(format!("Could not save key: {msg}").into());
                }
            }
        }
    });

    // Token balance for the metered broker.
    let ui_ctx_quota = ui_ctx.clone();
    let weak_quota = window.as_weak();
    window.on_settings_cloud_ai_refresh_quota(move || {
        let weak = weak_quota.clone();
        let ui_ctx = ui_ctx_quota.clone();
        slint::spawn_local(async move {
            match ui_ctx.cloud_ai_quota().await {
                Ok((balance, spent)) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_cloud_ai_balance(
                                format!("{balance} tokens remaining · {spent} used").into(),
                            );
                            w.set_settings_cloud_ai_status("".into());
                        }
                    });
                }
                Err(e) => {
                    let msg = e.to_string();
                    tracing::error!("Failed to fetch AI token quota: {}", msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_cloud_ai_balance("".into());
                            w.set_settings_cloud_ai_status(
                                format!("Could not fetch balance: {msg}").into(),
                            );
                        }
                    });
                }
            }
        })
        .ok();
    });
}
