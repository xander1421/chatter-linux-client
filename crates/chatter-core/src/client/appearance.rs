//! Appearance façade: persisted theme, text size, message density and glass effects.

use crate::services::appearance::{MessageDensity, TextSize, ThemeMode};
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};

use super::ChatterClient;

impl ChatterClient {
    /// Device-local animation multiplier. Only supported segmented values are stored.
    pub fn animation_speed(&self) -> f32 {
        let speed = SettingsRepository::new(self.services.database.clone())
            .get::<f32>("app.animation_speed")
            .ok()
            .flatten()
            .unwrap_or(1.0);
        if matches!(speed, 0.5 | 1.0 | 2.0 | 4.0 | 8.0) { speed } else { 1.0 }
    }

    pub fn set_animation_speed(&self, speed: f32) -> Result<(), crate::error::StorageError> {
        let speed = if matches!(speed, 0.5 | 1.0 | 2.0 | 4.0 | 8.0) { speed } else { 1.0 };
        SettingsRepository::new(self.services.database.clone()).set("app.animation_speed", &speed)
    }

    /// Whether chrome uses glass optics. Enabled by default on each device.
    pub fn glass_effects_enabled(&self) -> bool {
        SettingsRepository::new(self.services.database.clone())
            .get_bool(keys::GLASS_EFFECTS)
            .ok()
            .flatten()
            .unwrap_or(true)
    }

    /// Persist the device-local glass effects preference.
    pub fn set_glass_effects_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), crate::error::StorageError> {
        SettingsRepository::new(self.services.database.clone())
            .set_bool(keys::GLASS_EFFECTS, enabled)
    }

    /// Whether decorative animation is skipped. Off by default on each device.
    pub fn reduce_motion_enabled(&self) -> bool {
        SettingsRepository::new(self.services.database.clone())
            .get_bool(keys::REDUCE_MOTION)
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// Persist the device-local reduce-motion preference.
    pub fn set_reduce_motion_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), crate::error::StorageError> {
        SettingsRepository::new(self.services.database.clone())
            .set_bool(keys::REDUCE_MOTION, enabled)
    }

    /// Device-local IME geometry used to keep the picker at the keyboard baseline.
    pub fn composer_keyboard_geometry(&self) -> Option<(f32, f32)> {
        SettingsRepository::new(self.services.database.clone())
            .get(keys::COMPOSER_KEYBOARD_GEOMETRY)
            .ok()
            .flatten()
    }

    pub fn set_composer_keyboard_geometry(
        &self,
        height: f32,
        width: f32,
    ) -> Result<(), crate::error::StorageError> {
        SettingsRepository::new(self.services.database.clone())
            .set(keys::COMPOSER_KEYBOARD_GEOMETRY, &(height, width))
    }

    /// The user's theme choice (defaults to [`ThemeMode::System`]).
    pub fn theme_mode(&self) -> ThemeMode {
        let raw = SettingsRepository::new(self.services.database.clone())
            .get_string(keys::THEME)
            .ok()
            .flatten()
            .unwrap_or_default();
        ThemeMode::from_str(&raw)
    }

    /// Persist the user's theme choice.
    pub fn set_theme_mode(&self, mode: ThemeMode) -> Result<(), crate::error::StorageError> {
        SettingsRepository::new(self.services.database.clone())
            .set_string(keys::THEME, mode.as_str())
    }

    /// The user's text size (defaults to [`TextSize::Default`], which
    /// follows the OS font scale).
    pub fn text_size(&self) -> TextSize {
        let raw = SettingsRepository::new(self.services.database.clone())
            .get_string(keys::TEXT_SIZE)
            .ok()
            .flatten()
            .unwrap_or_default();
        TextSize::from_str(&raw)
    }

    /// Persist the user's text size.
    pub fn set_text_size(&self, size: TextSize) -> Result<(), crate::error::StorageError> {
        SettingsRepository::new(self.services.database.clone())
            .set_string(keys::TEXT_SIZE, size.as_str())
    }

    /// The user's message spacing (defaults to [`MessageDensity::Comfortable`]).
    pub fn message_density(&self) -> MessageDensity {
        let raw = SettingsRepository::new(self.services.database.clone())
            .get_string(keys::MESSAGE_DENSITY)
            .ok()
            .flatten()
            .unwrap_or_default();
        MessageDensity::from_str(&raw)
    }

    /// Persist the user's message spacing.
    pub fn set_message_density(
        &self,
        density: MessageDensity,
    ) -> Result<(), crate::error::StorageError> {
        SettingsRepository::new(self.services.database.clone())
            .set_string(keys::MESSAGE_DENSITY, density.as_str())
    }
}
