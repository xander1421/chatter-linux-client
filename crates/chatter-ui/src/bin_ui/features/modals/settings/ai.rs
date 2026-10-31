//! Remote AI settings callbacks.

use super::*;

pub(super) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // ── Cloud AI ─────────────────────────────────────────────────────────
    //
    // Thin bindings only: which silo may actually run is decided by
    // chatter-core's ai_policy, not here. The UI records a *preference*; a DM
    // or mesh conversation remains unavailable regardless of it.

    // Prefer the cloud silo where policy allows it.
    let ui_ctx_pref = ui_ctx.clone();
    #[cfg(feature = "jarvis")]
    let weak_preference = window.as_weak();
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
                return;
            }
            #[cfg(feature = "jarvis")]
            if let Some(w) = weak_preference.upgrade() {
                w.global::<crate::JarvisGlobal>().set_opted_in(prefer);
                let weak = weak_preference.clone();
                let client = ui_ctx_pref.clone();
                tokio::spawn(async move {
                    crate::bin_ui::features::sidebar::rebuild_chat_list(&weak, &client);
                });
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
