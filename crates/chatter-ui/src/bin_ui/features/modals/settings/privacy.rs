//! Settings privacy callbacks.

use super::*;

pub(super) fn wire_preferences(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Read receipts toggle
    let ui_ctx_read = ui_ctx.clone();
    let weak_read = window.as_weak();
    let saved_read = std::cell::Cell::new(
        ui_ctx_read.read_receipts_enabled().unwrap_or(
            window
                .global::<crate::SettingsPane>()
                .get_settings_read_receipts(),
        ),
    );
    window
        .global::<crate::SettingsPane>()
        .on_settings_read_receipts_changed(move |enabled: bool| {
            if let Err(error) = ui_ctx_read.set_read_receipts_enabled(enabled) {
                if let Some(w) = weak_read.upgrade() {
                    w.global::<crate::SettingsPane>()
                        .set_settings_read_receipts(
                            ui_ctx_read
                                .read_receipts_enabled()
                                .unwrap_or(saved_read.get()),
                        );
                    saved_read.set(
                        w.global::<crate::SettingsPane>()
                            .get_settings_read_receipts(),
                    );
                    privacy_save_error(&w, "read receipts", &error);
                }
            } else {
                saved_read.set(enabled);
            }
        });

    // Delivery receipts toggle
    let ui_ctx_delivery = ui_ctx.clone();
    let weak_delivery = window.as_weak();
    let saved_delivery = std::cell::Cell::new(
        ui_ctx_delivery.delivery_receipts_enabled().unwrap_or(
            window
                .global::<crate::SettingsPane>()
                .get_settings_delivery_receipts(),
        ),
    );
    window
        .global::<crate::SettingsPane>()
        .on_settings_delivery_receipts_changed(move |enabled: bool| {
            if let Err(error) = ui_ctx_delivery.set_delivery_receipts_enabled(enabled) {
                if let Some(w) = weak_delivery.upgrade() {
                    w.global::<crate::SettingsPane>()
                        .set_settings_delivery_receipts(
                            ui_ctx_delivery
                                .delivery_receipts_enabled()
                                .unwrap_or(saved_delivery.get()),
                        );
                    saved_delivery.set(
                        w.global::<crate::SettingsPane>()
                            .get_settings_delivery_receipts(),
                    );
                    privacy_save_error(&w, "delivery receipts", &error);
                }
            } else {
                saved_delivery.set(enabled);
            }
        });

    // Typing indicators toggle
    let ui_ctx_typing = ui_ctx.clone();
    let weak_typing = window.as_weak();
    let saved_typing = std::cell::Cell::new(
        ui_ctx_typing.typing_indicators_enabled().unwrap_or(
            window
                .global::<crate::SettingsPane>()
                .get_settings_typing_indicators(),
        ),
    );
    window
        .global::<crate::SettingsPane>()
        .on_settings_typing_indicators_changed(move |enabled: bool| {
            if let Err(error) = ui_ctx_typing.set_typing_indicators_enabled(enabled) {
                if let Some(w) = weak_typing.upgrade() {
                    w.global::<crate::SettingsPane>()
                        .set_settings_typing_indicators(
                            ui_ctx_typing
                                .typing_indicators_enabled()
                                .unwrap_or(saved_typing.get()),
                        );
                    saved_typing.set(
                        w.global::<crate::SettingsPane>()
                            .get_settings_typing_indicators(),
                    );
                    privacy_save_error(&w, "typing indicators", &error);
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
    window
        .global::<crate::SettingsPane>()
        .on_settings_notification_detail_changed(move |value: SharedString| {
            use chatter_core::client::NotificationDetailLevel;
            let level = NotificationDetailLevel::from_str(value.as_str());
            if let Err(e) = ui_ctx_detail.set_notification_detail_level(level) {
                tracing::error!("Failed to save notification detail level: {}", e);
            }
            // Names reach the system share sheet only where notifications show them.
            #[cfg(target_os = "android")]
            crate::platform::android::share_targets::refresh(&ui_ctx_detail);
            if let Some(w) = weak_detail.upgrade() {
                w.global::<crate::SettingsPane>()
                    .set_settings_notification_detail(
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
    window
        .global::<crate::SettingsPane>()
        .on_settings_search_shortcut_changed(move |value: SharedString| {
            let shortcut = chatter_core::client::SearchShortcut::from_str(value.as_str());
            if let Err(e) = ui_ctx_shortcut.set_search_shortcut(shortcut) {
                tracing::error!("Failed to save search shortcut: {}", e);
                return;
            }
            if let Some(w) = weak_shortcut.upgrade() {
                w.global::<crate::SearchPane>()
                    .set_search_shortcut_pref(shortcut.as_str().into());
            }
        });
}

pub(super) fn wire_identity(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Copy fingerprint
    let ui_ctx_fp = ui_ctx.clone();
    window
        .global::<crate::SettingsPane>()
        .on_settings_copy_fingerprint(move || {
            let ui_ctx = ui_ctx_fp.clone();
            tokio::spawn(async move {
                let fingerprint = match ui_ctx.my_identity_fingerprint().await {
                    Ok(fp) => fp,
                    Err(e) => {
                        tracing::error!("Failed to get identity fingerprint: {}", e);
                        return;
                    }
                };

                match crate::platform::clipboard::set_text(
                    &chatter_i18n::tr("Safety number"),
                    &fingerprint,
                    false,
                ) {
                    Ok(()) => tracing::info!("Fingerprint copied to clipboard"),
                    Err(e) => tracing::error!("Failed to copy fingerprint to clipboard: {}", e),
                }
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
    window
        .global::<crate::SettingsPane>()
        .on_settings_sealed_sender_changed(move |allow_from_anyone: bool| {
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
                                w.global::<crate::SettingsPane>()
                                    .set_settings_sealed_sender_unrestricted(persisted);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("Failed to set sealed sender policy on server: {}", e);
                        // Revert the toggle to the opposite of the attempted value.
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_sealed_sender_unrestricted(!allow_from_anyone);
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
    window
        .global::<crate::SettingsPane>()
        .on_settings_username_discoverable_changed(move |discoverable: bool| {
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
                        w.global::<crate::SettingsPane>()
                            .set_settings_username_discoverable(shown);
                    }
                });
            });
        });
}

fn wire_profile_visibility(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    use chatter_core::client::ProfileVisibility;

    /// Run the change off the UI thread, then show the outcome.
    fn apply(
        weak: slint::Weak<MainWindow>,
        ui_ctx: Arc<ChatterClient>,
        visibility: ProfileVisibility,
        pin: Option<String>,
    ) {
        if let Some(w) = weak.upgrade() {
            w.global::<crate::SettingsPane>()
                .set_settings_profile_visibility_busy(true);
            w.global::<crate::SettingsPane>()
                .set_settings_profile_visibility_error("".into());
        }
        tokio::spawn(async move {
            let result = ui_ctx
                .set_profile_visibility(visibility, pin.as_deref())
                .await;
            let now = ui_ctx.profile_visibility().as_str();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(w) = weak.upgrade() else { return };
                w.global::<crate::SettingsPane>()
                    .set_settings_profile_visibility_busy(false);
                w.global::<crate::SettingsPane>()
                    .set_settings_profile_visibility(now.into());
                match result {
                    Ok(()) => {
                        w.global::<crate::SettingsPane>()
                            .set_settings_profile_visibility_pending("".into());
                        w.global::<crate::SettingsPane>()
                            .set_settings_profile_visibility_pin("".into());
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "profile visibility change failed");
                        w.global::<crate::SettingsPane>()
                            .set_settings_profile_visibility_error(e.user_message().into());
                    }
                }
            });
        });
    }

    let ui_ctx_sel = ui_ctx.clone();
    let weak_sel = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_profile_visibility_selected(move |key: SharedString| {
            let Some(visibility) = ProfileVisibility::parse(key.as_str()) else {
                return;
            };
            let (ui_ctx, weak) = (ui_ctx_sel.clone(), weak_sel.clone());
            tokio::spawn(async move {
                match ui_ctx.profile_visibility_needs_pin(visibility).await {
                    Ok(true) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_profile_visibility_error("".into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_profile_visibility_pin("".into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_profile_visibility_pending(
                                        visibility.as_str().into(),
                                    );
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
                                w.global::<crate::SettingsPane>()
                                    .set_settings_profile_visibility_error(e.user_message().into());
                            }
                        });
                    }
                }
            });
        });

    let ui_ctx_conf = ui_ctx.clone();
    let weak_conf = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_profile_visibility_confirmed(move |key: SharedString, pin: SharedString| {
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
    window
        .global::<crate::SettingsPane>()
        .on_settings_profile_visibility_cancelled(move || {
            if let Some(w) = weak_cancel.upgrade() {
                w.global::<crate::SettingsPane>()
                    .set_settings_profile_visibility_pending("".into());
                w.global::<crate::SettingsPane>()
                    .set_settings_profile_visibility_pin("".into());
                w.global::<crate::SettingsPane>()
                    .set_settings_profile_visibility_error("".into());
            }
        });
}

fn privacy_save_error(w: &MainWindow, setting: &str, error: &dyn std::fmt::Display) {
    tracing::warn!(%error, setting, "privacy setting not saved");
    let message = match setting {
        "read receipts" => chatter_i18n::tr("Couldn't save read receipts. Try again."),
        "typing indicators" => chatter_i18n::tr("Couldn't save typing indicators. Try again."),
        other => chatter_i18n::trf("Couldn't save {}. Try again.", &[&other]),
    };
    w.set_toast_message(message.into());
    w.set_toast_variant("error".into());
    w.set_toast_visible(true);
}
