//! Local AI settings section: provider detection, hardware summary,
//! model catalog with hardware-fit tags, and Ollama downloads.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;

use crate::{CatalogModelData, LocalAIProviderData, MainWindow};

pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Rescan providers + hardware (also fired on section open via Slint init).
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window.on_settings_local_ai_refresh(move || {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            tokio::spawn(async move { refresh_all(ui_ctx, weak).await });
        });
    }

    // Switch active provider (persist + hot-swap the Jarvis engine).
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window.on_settings_local_ai_select_provider(move |kind: SharedString| {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            tokio::spawn(async move {
                if let Err(e) = ui_ctx.local_ai_apply_provider(kind.as_str()) {
                    tracing::error!("provider switch failed: {e}");
                }
                refresh_all(ui_ctx, weak).await;
            });
        });
    }

    // Pick the chat model for the active provider.
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window.on_settings_local_ai_select_model(move |model: SharedString| {
            if let Err(e) = ui_ctx.local_ai_apply_chat_model(model.as_str()) {
                tracing::error!("chat model switch failed: {e}");
                return;
            }
            if let Some(w) = weak.upgrade() {
                w.set_settings_local_ai_active_model(model);
            }
        });
    }

    // Download a catalog model through Ollama (streaming progress).
    {
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();
        window.on_settings_local_ai_download(move |tag: SharedString| {
            let ui_ctx = ui_ctx.clone();
            let weak = weak.clone();
            tokio::spawn(async move { download_model(ui_ctx, tag.to_string(), weak).await });
        });
    }
}

/// Re-probe hardware + providers and push everything into the settings UI.
pub async fn refresh_all(ui_ctx: Arc<ChatterClient>, weak: slint::Weak<MainWindow>) {
    {
        let weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_settings_local_ai_detecting(true);
            }
        });
    }

    // Hardware probes shell out (nvidia-smi) — keep off the async pool.
    let ui_ctx_hw = ui_ctx.clone();
    let profile = tokio::task::spawn_blocking(move || ui_ctx_hw.local_ai_hardware_profile())
        .await
        .unwrap_or_else(|_| ui_ctx.local_ai_hardware_profile());
    let hardware_summary = profile.summary();

    let providers = ui_ctx.local_ai_detect_providers().await;
    let active_provider = ui_ctx.local_ai_active_provider();
    let active_model = ui_ctx.local_ai_chat_model();

    let choices = ui_ctx.local_ai_model_choices(&providers, &active_provider, &profile);
    let active_models = choices.active_models;
    let provider_rows: Vec<LocalAIProviderData> = choices.providers.into_iter().map(|p|
        LocalAIProviderData {
            kind: p.kind.into(), display_name: p.display_name.into(),
            detail: p.detail.into(), is_active: p.is_active,
        }).collect();
    let catalog_rows: Vec<CatalogModelData> = choices.catalog.into_iter().map(|m|
        CatalogModelData {
            name: m.name.into(), tag: m.tag.into(), detail: m.detail.into(),
            fit: m.fit.into(), fit_label: m.fit_label.into(), installed: m.installed,
            downloading: false, progress: 0.0, error: "".into(),
        }).collect();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_settings_local_ai_detecting(false);
            w.set_settings_local_ai_hardware_summary(hardware_summary.into());
            w.set_settings_local_ai_providers(ModelRc::new(VecModel::from(provider_rows)));
            w.set_settings_local_ai_active_provider(active_provider.into());
            w.set_settings_local_ai_models(ModelRc::new(VecModel::from(
                active_models
                    .into_iter()
                    .map(SharedString::from)
                    .collect::<Vec<_>>(),
            )));
            w.set_settings_local_ai_active_model(active_model.into());
            w.set_settings_local_ai_catalog(ModelRc::new(VecModel::from(catalog_rows)));
        }
    });
}

/// Update one catalog row by tag on the UI thread.
fn update_catalog_row(
    weak: &slint::Weak<MainWindow>,
    tag: &str,
    apply: impl Fn(&mut CatalogModelData) + Send + 'static,
) {
    let weak = weak.clone();
    let tag = tag.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            let model = w.get_settings_local_ai_catalog();
            for i in 0..model.row_count() {
                if let Some(mut row) = model.row_data(i) {
                    if row.tag.as_str() == tag {
                        apply(&mut row);
                        model.set_row_data(i, row);
                        break;
                    }
                }
            }
        }
    });
}

async fn download_model(ui_ctx: Arc<ChatterClient>, tag: String, weak: slint::Weak<MainWindow>) {
    update_catalog_row(&weak, &tag, |row| {
        row.downloading = true;
        row.progress = 0.0;
        row.error = "".into();
    });

    // Throttle UI updates to whole-percent changes.
    let last_percent = std::sync::atomic::AtomicI32::new(-1);
    let weak_progress = weak.clone();
    let tag_progress = tag.clone();
    let result = ui_ctx
        .local_ai_pull_model(&tag, move |p| {
            let (Some(total), Some(completed)) = (p.total, p.completed) else {
                return;
            };
            if total == 0 {
                return;
            }
            let percent = ((completed as f64 / total as f64) * 100.0) as i32;
            if last_percent.swap(percent, std::sync::atomic::Ordering::Relaxed) == percent {
                return;
            }
            let progress = (completed as f64 / total as f64) as f32;
            update_catalog_row(&weak_progress, &tag_progress, move |row| {
                row.progress = progress;
            });
        })
        .await;

    match result {
        Ok(()) => refresh_all(ui_ctx, weak).await, // re-list: marks installed
        Err(e) => {
            let msg = e.to_string();
            update_catalog_row(&weak, &tag, move |row| {
                row.downloading = false;
                row.error = msg.clone().into();
            });
        }
    }
}
