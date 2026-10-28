//! Settings ai callbacks.

use super::*;

pub(super) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Local AI enabled toggle
    let ui_ctx_ai = ui_ctx.clone();
    let weak_ai_save = window.as_weak();
    #[cfg(feature = "jarvis")]
    let weak_ai_toggle = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_local_ai_enabled_changed(move |enabled: bool| {
            if let Err(e) = ui_ctx_ai.set_local_ai_enabled(enabled) {
                tracing::error!("Failed to save local AI enabled setting: {}", e);
                if let Some(w) = weak_ai_save.upgrade() {
                    w.global::<crate::SettingsPane>()
                        .set_settings_local_ai_enabled(
                            ui_ctx_ai.local_ai_enabled().unwrap_or(!enabled),
                        );
                }
                return;
            }
            tracing::debug!("Local AI enabled setting changed to: {}", enabled);
            // The AI channel's sidebar visibility follows this setting — rebuild
            // the chat list live.
            #[cfg(feature = "jarvis")]
            {
                if let Some(w) = weak_ai_toggle.upgrade() {
                    w.global::<crate::JarvisGlobal>().set_opted_in(enabled);
                }
                let (weak, ui_ctx) = (weak_ai_toggle.clone(), ui_ctx_ai.clone());
                tokio::spawn(async move {
                    crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &ui_ctx)
                });
            }
        });

    // Load AI model
    let weak_window = window.as_weak();
    let ui_ctx_load = ui_ctx.clone();
    window
        .global::<crate::SettingsPane>()
        .on_settings_local_ai_load_model(move |model_id: SharedString| {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_load.clone();
            let model_id = model_id.to_string();

            tokio::spawn(async move {
                // Set loading state
                let weak_loading = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_loading.upgrade() {
                        w.global::<crate::SettingsPane>()
                            .set_settings_local_ai_loading(true);
                        w.global::<crate::SettingsPane>()
                            .set_settings_local_ai_error("".into());
                        w.global::<crate::SettingsPane>()
                            .set_settings_local_ai_status("Loading...".into());
                    }
                });

                // Save model preference first
                if let Err(e) = ui_ctx.set_local_ai_preferred_model(&model_id, None) {
                    let error_msg = e.to_string();
                    tracing::error!("Failed to save model preference: {}", error_msg);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.global::<crate::SettingsPane>()
                                .set_settings_local_ai_loading(false);
                            w.global::<crate::SettingsPane>()
                                .set_settings_local_ai_error(error_msg.into());
                            w.global::<crate::SettingsPane>()
                                .set_settings_local_ai_status("".into());
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
                                w.global::<crate::SettingsPane>()
                                    .set_settings_local_ai_loading(false);
                                w.global::<crate::SettingsPane>()
                                    .set_settings_local_ai_status(status.into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_local_ai_error("".into());
                            }
                        });
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        tracing::error!("Failed to load local AI model: {}", error_msg);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_local_ai_loading(false);
                                w.global::<crate::SettingsPane>()
                                    .set_settings_local_ai_error(error_msg.into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_local_ai_status("".into());
                            }
                        });
                    }
                }
            });
        });

    // Unload AI model
    let weak_window = window.as_weak();
    let ui_ctx_unload = ui_ctx.clone();
    window
        .global::<crate::SettingsPane>()
        .on_settings_local_ai_unload_model(move || {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_unload.clone();

            tokio::spawn(async move {
                match ui_ctx.unload_local_ai_model().await {
                    Ok(()) => {
                        tracing::info!("Local AI model unloaded");
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_local_ai_status("".into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_local_ai_error("".into());
                            }
                        });
                    }
                    Err(e) => {
                        let error_msg = e.to_string();
                        tracing::error!("Failed to unload local AI model: {}", error_msg);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_local_ai_error(error_msg.into());
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
    window
        .global::<crate::SettingsPane>()
        .on_settings_ai_prefer_cloud_changed(move |prefer: bool| {
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
    window
        .global::<crate::SettingsPane>()
        .on_settings_cloud_ai_mode_changed(move |mode: SharedString| {
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
                w.global::<crate::SettingsPane>()
                    .set_settings_cloud_ai_configured(settings.is_available());
                w.global::<crate::SettingsPane>()
                    .set_settings_cloud_ai_status("".into());
            }
        });

    // Store the user's own provider key. Blank clears it.
    let ui_ctx_key = ui_ctx.clone();
    let weak_key = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_cloud_ai_api_key_changed(move |key: SharedString| {
            match ui_ctx_key.set_cloud_ai_api_key(key.as_str()) {
                Ok(()) => {
                    if let Some(w) = weak_key.upgrade() {
                        let settings = ui_ctx_key.cloud_ai_settings();
                        w.global::<crate::SettingsPane>()
                            .set_settings_cloud_ai_configured(settings.is_available());
                        w.global::<crate::SettingsPane>()
                            .set_settings_cloud_ai_status(
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
                        w.global::<crate::SettingsPane>()
                            .set_settings_cloud_ai_status(
                                format!("Could not save key: {msg}").into(),
                            );
                    }
                }
            }
        });

    // Token balance for the metered broker.
    let ui_ctx_quota = ui_ctx.clone();
    let weak_quota = window.as_weak();
    window
        .global::<crate::SettingsPane>()
        .on_settings_cloud_ai_refresh_quota(move || {
            let weak = weak_quota.clone();
            let ui_ctx = ui_ctx_quota.clone();
            slint::spawn_local(async move {
                match ui_ctx.cloud_ai_quota().await {
                    Ok((balance, spent)) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_cloud_ai_balance(
                                        format!("{balance} tokens remaining · {spent} used").into(),
                                    );
                                w.global::<crate::SettingsPane>()
                                    .set_settings_cloud_ai_status("".into());
                            }
                        });
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        tracing::error!("Failed to fetch AI token quota: {}", msg);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::SettingsPane>()
                                    .set_settings_cloud_ai_balance("".into());
                                w.global::<crate::SettingsPane>()
                                    .set_settings_cloud_ai_status(
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
