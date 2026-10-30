//! Settings appearance callbacks.

use super::*;

pub(super) fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
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
    appearance.set_animation_speed(ui_ctx.animation_speed());
    let ui_ctx_speed = ui_ctx.clone();
    let weak_speed = window.as_weak();
    appearance.on_animation_speed_changed(move |speed: f32| {
        if let Err(e) = ui_ctx_speed.set_animation_speed(speed) {
            tracing::error!("Failed to save animation speed: {}", e);
        }
        if let Some(w) = weak_speed.upgrade() {
            w.global::<crate::Appearance>()
                .set_animation_speed(ui_ctx_speed.animation_speed());
        }
    });
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
