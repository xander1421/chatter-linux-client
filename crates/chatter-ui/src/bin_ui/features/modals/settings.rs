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
    let weak_read = window.as_weak();
    let saved_read = std::cell::Cell::new(
        ui_ctx_read
            .read_receipts_enabled()
            .unwrap_or(window.get_settings_read_receipts()),
    );
    window.on_settings_read_receipts_changed(move |enabled: bool| {
        if let Err(error) = ui_ctx_read.set_read_receipts_enabled(enabled) {
            if let Some(w) = weak_read.upgrade() {
                w.set_settings_read_receipts(
                    ui_ctx_read
                        .read_receipts_enabled()
                        .unwrap_or(saved_read.get()),
                );
                saved_read.set(w.get_settings_read_receipts());
                privacy_save_error(&w, "read receipts", &error.to_string());
            }
        } else {
            saved_read.set(enabled);
        }
    });

    // Typing indicators toggle
    let ui_ctx_typing = ui_ctx.clone();
    let weak_typing = window.as_weak();
    let saved_typing = std::cell::Cell::new(
        ui_ctx_typing
            .typing_indicators_enabled()
            .unwrap_or(window.get_settings_typing_indicators()),
    );
    window.on_settings_typing_indicators_changed(move |enabled: bool| {
        if let Err(error) = ui_ctx_typing.set_typing_indicators_enabled(enabled) {
            if let Some(w) = weak_typing.upgrade() {
                w.set_settings_typing_indicators(
                    ui_ctx_typing
                        .typing_indicators_enabled()
                        .unwrap_or(saved_typing.get()),
                );
                saved_typing.set(w.get_settings_typing_indicators());
                privacy_save_error(&w, "typing indicators", &error.to_string());
            }
        } else {
            saved_typing.set(enabled);
        }
    });

    // Notification content selector (Signal's "Show: Name and message /
    // Name only / No name or message"). Core persists and applies; the
    // property is re-set from what core now holds so a failed save shows the
    // level actually in force, not the one clicked.
    let ui_ctx_detail = ui_ctx.clone();
    let weak_detail = window.as_weak();
    window.on_settings_notification_detail_changed(move |value: SharedString| {
        use chatter_core::services::notification_service::NotificationDetailLevel;
        let level = NotificationDetailLevel::from_str(value.as_str());
        if let Err(e) = ui_ctx_detail.set_notification_detail_level(level) {
            tracing::error!("Failed to save notification detail level: {}", e);
        }
        if let Some(w) = weak_detail.upgrade() {
            w.set_settings_notification_detail(
                ui_ctx_detail.notification_detail_level().as_str().into(),
            );
        }
    });

    // "Who can see my photo and name". Core decides everything: whether the
    // choice needs the PIN (it restricts, so the key rotates, and the account
    // has a key backup) and the change itself. The radio always shows what
    // core holds afterwards.
    wire_profile_visibility(window, ui_ctx);

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

    // Theme (Settings › Appearance). Applied before the first frame from what
    // core persisted; a pick is saved, then re-read so a failed save shows the
    // theme actually in force.
    let appearance = window.global::<crate::Appearance>();
    appearance.set_theme(ui_ctx.theme_mode().as_str().into());
    mirror_theme_mode(ui_ctx.theme_mode().as_str());
    appearance.on_dark_changed(apply_system_bars);
    apply_system_bars(appearance.get_dark());
    let ui_ctx_theme = ui_ctx.clone();
    let weak_theme = window.as_weak();
    appearance.on_theme_changed(move |value: SharedString| {
        let mode = chatter_core::services::appearance::ThemeMode::from_str(value.as_str());
        if let Err(e) = ui_ctx_theme.set_theme_mode(mode) {
            tracing::error!("Failed to save theme: {}", e);
        }
        let saved = ui_ctx_theme.theme_mode().as_str();
        mirror_theme_mode(saved);
        if let Some(w) = weak_theme.upgrade() {
            w.global::<crate::Appearance>().set_theme(saved.into());
        }
    });

    // Text size and message spacing (Settings › Appearance) — the theme's
    // shape: applied before the first frame, a pick saved then re-read.
    let system_font_scale = system_font_scale();
    let text_size = ui_ctx.text_size();
    appearance.set_text_size(text_size.as_str().into());
    appearance.set_text_scale(text_size.scale(system_font_scale));
    let ui_ctx_text = ui_ctx.clone();
    let weak_text = window.as_weak();
    appearance.on_text_size_changed(move |value: SharedString| {
        let size = chatter_core::services::appearance::TextSize::from_str(value.as_str());
        if let Err(e) = ui_ctx_text.set_text_size(size) {
            tracing::error!("Failed to save text size: {}", e);
        }
        if let Some(w) = weak_text.upgrade() {
            let size = ui_ctx_text.text_size();
            let appearance = w.global::<crate::Appearance>();
            appearance.set_text_size(size.as_str().into());
            appearance.set_text_scale(size.scale(system_font_scale));
        }
    });

    appearance.set_density(ui_ctx.message_density().as_str().into());
    let ui_ctx_density = ui_ctx.clone();
    let weak_density = window.as_weak();
    appearance.on_density_changed(move |value: SharedString| {
        let density = chatter_core::services::appearance::MessageDensity::from_str(value.as_str());
        if let Err(e) = ui_ctx_density.set_message_density(density) {
            tracing::error!("Failed to save message spacing: {}", e);
        }
        if let Some(w) = weak_density.upgrade() {
            w.global::<crate::Appearance>()
                .set_density(ui_ctx_density.message_density().as_str().into());
        }
    });

    // Chrome effects are device-local and applied before the first frame.
    appearance.set_glass_effects(ui_ctx.glass_effects_enabled());
    let ui_ctx_glass = ui_ctx.clone();
    let weak_glass = window.as_weak();
    appearance.on_glass_effects_changed(move |enabled: bool| {
        if let Err(e) = ui_ctx_glass.set_glass_effects_enabled(enabled) {
            tracing::error!("Failed to save glass effects: {}", e);
        }
        if let Some(w) = weak_glass.upgrade() {
            w.global::<crate::Appearance>()
                .set_glass_effects(ui_ctx_glass.glass_effects_enabled());
        }
    });

    // Reduce animations is device-local too, and read before the first frame
    // so no decorative loop starts only to be stopped.
    appearance.set_reduce_motion(ui_ctx.reduce_motion_enabled());
    let ui_ctx_motion = ui_ctx.clone();
    let weak_motion = window.as_weak();
    appearance.on_reduce_motion_changed(move |enabled: bool| {
        if let Err(e) = ui_ctx_motion.set_reduce_motion_enabled(enabled) {
            tracing::error!("Failed to save reduce motion: {}", e);
        }
        if let Some(w) = weak_motion.upgrade() {
            let reduce = ui_ctx_motion.reduce_motion_enabled();
            w.global::<crate::Appearance>().set_reduce_motion(reduce);
            // Decorative playback in the GIF picker stops at once; tiles keep
            // their first frame.
            if reduce {
                crate::bin_ui::gif_state::clear_picker_gifs();
            }
        }
    });

    // Account push delivery is read from and saved to the server. Keep this
    // separate from device sound/OS permissions and notification privacy.
    let delivery = window.global::<crate::NotificationDelivery>();
    let weak_delivery = window.as_weak();
    let client_delivery = ui_ctx.clone();
    delivery.on_load(move || {
        update_notification_delivery(&weak_delivery, &client_delivery, None);
    });
    let weak_delivery = window.as_weak();
    let client_delivery = ui_ctx.clone();
    delivery.on_push_enabled_changed(move |enabled| {
        update_notification_delivery(&weak_delivery, &client_delivery, Some(enabled));
    });

    // Sound and vibration (Settings › Notifications). Android keeps both on
    // the OS notification channel, so the row opens its page; desktop
    // notifiers take a sound flag; iOS has neither, so the row is hidden.
    let sound = window.global::<crate::NotificationSound>();
    #[cfg(target_os = "android")]
    {
        sound.set_mode("system".into());
        sound.on_open_system_settings(
            crate::platform::android::system_settings::open_message_notification_settings,
        );
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        sound.set_mode("toggle".into());
        sound.set_enabled(ui_ctx.notification_sound_enabled());
        let ui_ctx_sound = ui_ctx.clone();
        let weak_sound = window.as_weak();
        sound.on_enabled_changed(move |enabled| {
            if let Err(e) = ui_ctx_sound.set_notification_sound_enabled(enabled) {
                tracing::error!("Failed to save notification sound: {}", e);
            }
            if let Some(w) = weak_sound.upgrade() {
                w.global::<crate::NotificationSound>()
                    .set_enabled(ui_ctx_sound.notification_sound_enabled());
            }
        });
    }
    #[cfg(target_os = "ios")]
    let _ = sound;

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

    // "Who can find me by my username" (Everyone / Nobody). Persisted and
    // enforced by the SERVER (SetUsernameDiscoverability), so it holds on
    // every device. On failure the choice reverts to the previous value.
    let ui_ctx_discover = ui_ctx.clone();
    let weak_discover = window.as_weak();
    window.on_settings_username_discoverable_changed(move |discoverable: bool| {
        let ui_ctx = ui_ctx_discover.clone();
        let weak = weak_discover.clone();
        tokio::spawn(async move {
            let shown = match ui_ctx.set_username_discoverability(discoverable).await {
                Ok(persisted) => persisted,
                Err(e) => {
                    tracing::error!("Failed to set username discoverability on server: {}", e);
                    !discoverable
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_settings_username_discoverable(shown);
                }
            });
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
                        w.set_settings_change_pin_error(
                            "New PIN must be different from current PIN".into(),
                        );
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

            match ui_ctx.change_pin(&current_pin, &new_pin).await {
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
    let weak_window = window.as_weak();
    let ui_ctx_retry = ui_ctx.clone();
    window.on_settings_retry_backup(move |pin: SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_retry.clone();
        let pin = pin.to_string();

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_settings_backup_retry_loading(true);
                    w.set_settings_backup_retry_error("".into());
                }
            });

            let result = ui_ctx.retry_svr_backup(&pin).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_settings_backup_retry_loading(false);
                    match result {
                        Ok(()) => {
                            w.set_settings_backup_status("backed_up".into());
                            w.set_settings_backup_retry_pin("".into());
                        }
                        Err(e) => w.set_settings_backup_retry_error(e.to_string().into()),
                    }
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
                    "Export to a file is not available on Android yet — use \"Back up to server\""
                        .to_string(),
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
                    Ok(s) => format!(
                        "Restored {} of {} rows (rest already present)",
                        s.rows_inserted, s.rows_seen
                    ),
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
                Ok(s) => format!(
                    "Restored {} of {} rows (rest already present)",
                    s.rows_inserted, s.rows_seen
                ),
                Err(e) => format!("Server restore failed: {e}"),
            };
            set_message_backup_status(&weak, status);
        });
    });

    // Settings › Recovery Codes › Regenerate (password-confirmed). The server
    // replaces every code; the new set is shown once in RecoveryCodesModal,
    // whose Continue returns to Settings (`recovery-codes-regenerated`).
    let ui_ctx_regen = ui_ctx.clone();
    let weak_window = window.as_weak();
    window.on_settings_regenerate_recovery_codes(move |password: SharedString| {
        let ui_ctx = ui_ctx_regen.clone();
        let weak = weak_window.clone();
        let password = password.to_string();
        if let Some(w) = weak.upgrade() {
            w.set_settings_recovery_regen_loading(true);
            w.set_settings_recovery_regen_error("".into());
        }
        tokio::spawn(async move {
            let result = ui_ctx.regenerate_recovery_codes(&password).await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                w.set_settings_recovery_regen_loading(false);
                match result {
                    Ok(codes) => {
                        let model: Vec<SharedString> =
                            codes.iter().map(|c| c.as_str().into()).collect();
                        w.set_recovery_codes(slint::ModelRc::new(slint::VecModel::from(model)));
                        w.set_settings_recovery_regen_password("".into());
                        w.set_recovery_codes_regenerated(true);
                        crate::bin_ui::features::back::push(&w, "recovery-codes");
                    }
                    Err(e) => {
                        tracing::warn!("Recovery code regeneration failed: {e}");
                        w.set_settings_recovery_regen_error(
                            format!("Could not regenerate codes: {e}").into(),
                        );
                    }
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
                Err(e) => {
                    tracing::error!("Failed to persist sealed sender onboarding policy: {}", e)
                }
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
                                    tracing::error!(
                                        "Failed to copy recovery codes to clipboard: {}",
                                        e
                                    );
                                } else {
                                    tracing::info!(
                                        "Recovery codes copied to clipboard (auto-clears in 60s)"
                                    );
                                    // SECURITY: Auto-clear clipboard after 60 seconds
                                    std::thread::spawn(move || {
                                        std::thread::sleep(std::time::Duration::from_secs(60));
                                        let _ = slint::invoke_from_event_loop(move || {
                                            if let Ok(mut cb) = Clipboard::new() {
                                                if let Err(e) = cb.clear() {
                                                    tracing::warn!(
                                                        "Failed to auto-clear clipboard: {}",
                                                        e
                                                    );
                                                } else {
                                                    tracing::info!(
                                                        "Clipboard auto-cleared after 60s"
                                                    );
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
                        &w,
                        &user_id,
                        &auth_state,
                        &user_state,
                        Some(&ui_ctx),
                    );
                    crate::bin_ui::features::back::push(&w, "user-profile");
                    // The button says Edit, so open the editor, over the
                    // profile it edits (REVIEW-R4 P2-24). open_user_profile
                    // sets the own name and bio synchronously, which the
                    // editor copies.
                    w.invoke_profile_edit_profile();
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
                    crate::bin_ui::decode_cache::on_logout();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_delete_loading(false);
                            crate::bin_ui::features::composer::reset_session(&w, &ui_ctx);
                            w.set_settings_delete_password("".into());
                            w.set_settings_delete_error("".into());
                            w.set_is_signed_in(false);
                            w.set_current_view("login".into());
                            crate::bin_ui::features::back::reset_to_chats(&w);
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
    #[cfg(feature = "jarvis")]
    let weak_ai_toggle = window.as_weak();
    window.on_settings_local_ai_enabled_changed(move |enabled: bool| {
        if let Err(e) = ui_ctx_ai.set_local_ai_enabled(enabled) {
            tracing::error!("Failed to save local AI enabled setting: {}", e);
            return;
        }
        tracing::debug!("Local AI enabled setting changed to: {}", enabled);
        // The AI channel's sidebar visibility follows this setting — rebuild
        // the chat list live.
        #[cfg(feature = "jarvis")]
        {
            let (weak, ui_ctx) = (weak_ai_toggle.clone(), ui_ctx_ai.clone());
            tokio::spawn(async move {
                crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &ui_ctx)
            });
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
                        if settings.is_available() {
                            "API key saved."
                        } else {
                            "API key cleared."
                        }
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

/// Status/nav-bar icon colour for the palette in force: the app draws
/// edge-to-edge on Android, so light icons vanish on the light palette.
/// The OS font scale "Default" text size follows (Android's Settings ›
/// Display › Font size); desktop has no such setting.
fn system_font_scale() -> f32 {
    #[cfg(target_os = "android")]
    return crate::platform::android::system_settings::font_scale();
    #[cfg(not(target_os = "android"))]
    1.0
}

/// The saved theme, for Android's pre-first-frame window background.
fn mirror_theme_mode(mode: &str) {
    #[cfg(target_os = "android")]
    crate::platform::android::activity::set_theme_mode(mode);
    #[cfg(not(target_os = "android"))]
    let _ = mode;
}

fn apply_system_bars(dark: bool) {
    #[cfg(target_os = "android")]
    crate::platform::android::activity::set_light_system_bars(!dark);
    #[cfg(not(target_os = "android"))]
    let _ = dark;
}

fn wire_profile_visibility(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    use chatter_core::services::profile_visibility::ProfileVisibility;

    /// Run the change off the UI thread, then show the outcome.
    fn apply(
        weak: slint::Weak<MainWindow>,
        ui_ctx: Arc<ChatterClient>,
        visibility: ProfileVisibility,
        pin: Option<String>,
    ) {
        if let Some(w) = weak.upgrade() {
            w.set_settings_profile_visibility_busy(true);
            w.set_settings_profile_visibility_error("".into());
        }
        tokio::spawn(async move {
            let result = ui_ctx
                .set_profile_visibility(visibility, pin.as_deref())
                .await;
            let now = ui_ctx.profile_visibility().as_str();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                w.set_settings_profile_visibility_busy(false);
                w.set_settings_profile_visibility(now.into());
                match result {
                    Ok(()) => {
                        w.set_settings_profile_visibility_pending("".into());
                        w.set_settings_profile_visibility_pin("".into());
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "profile visibility change failed");
                        w.set_settings_profile_visibility_error(e.to_string().into());
                    }
                }
            });
        });
    }

    let ui_ctx_sel = ui_ctx.clone();
    let weak_sel = window.as_weak();
    window.on_settings_profile_visibility_selected(move |key: SharedString| {
        let Some(visibility) = ProfileVisibility::parse(key.as_str()) else {
            return;
        };
        let (ui_ctx, weak) = (ui_ctx_sel.clone(), weak_sel.clone());
        tokio::spawn(async move {
            match ui_ctx.profile_visibility_needs_pin(visibility).await {
                Ok(true) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_profile_visibility_error("".into());
                            w.set_settings_profile_visibility_pin("".into());
                            w.set_settings_profile_visibility_pending(visibility.as_str().into());
                        }
                    });
                }
                Ok(false) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        apply(weak, ui_ctx, visibility, None);
                    });
                }
                Err(e) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_settings_profile_visibility_error(e.to_string().into());
                        }
                    });
                }
            }
        });
    });

    let ui_ctx_conf = ui_ctx.clone();
    let weak_conf = window.as_weak();
    window.on_settings_profile_visibility_confirmed(move |key: SharedString, pin: SharedString| {
        let Some(visibility) = ProfileVisibility::parse(key.as_str()) else {
            return;
        };
        apply(
            weak_conf.clone(),
            ui_ctx_conf.clone(),
            visibility,
            Some(pin.to_string()),
        );
    });

    let weak_cancel = window.as_weak();
    window.on_settings_profile_visibility_cancelled(move || {
        if let Some(w) = weak_cancel.upgrade() {
            w.set_settings_profile_visibility_pending("".into());
            w.set_settings_profile_visibility_pin("".into());
            w.set_settings_profile_visibility_error("".into());
        }
    });
}

fn update_notification_delivery(
    weak: &slint::Weak<MainWindow>,
    client: &Arc<ChatterClient>,
    enabled: Option<bool>,
) {
    let Some(window) = weak.upgrade() else { return };
    let delivery = window.global::<crate::NotificationDelivery>();
    if delivery.get_busy() {
        return;
    }
    delivery.set_busy(true);
    delivery.set_error("".into());
    if enabled.is_none() {
        delivery.set_loaded(false);
    }
    let client = client.clone();
    let weak = weak.clone();
    tokio::spawn(async move {
        let result = match enabled {
            Some(value) => client.set_account_push_enabled(value).await,
            None => client.account_push_enabled().await,
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(window) = weak.upgrade() else { return };
            let delivery = window.global::<crate::NotificationDelivery>();
            delivery.set_busy(false);
            match result {
                Ok(value) => {
                    delivery.set_push_enabled(value);
                    delivery.set_saved_push_enabled(value);
                    delivery.set_loaded(true);
                }
                Err(error) => {
                    delivery.set_push_enabled(delivery.get_saved_push_enabled());
                    delivery.set_error(
                        format!("Could not update notification preferences: {error}").into(),
                    );
                }
            }
        });
    });
}

fn privacy_save_error(w: &MainWindow, setting: &str, error: &str) {
    w.set_toast_message(format!("Couldn't save {setting}. {error}").into());
    w.set_toast_variant("error".into());
    w.set_toast_visible(true);
}
