//! Appearance façade: persisted theme, text size, message density and glass effects.

use crate::services::appearance::{MessageDensity, TextSize, ThemeMode};
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};

use super::ChatterClient;

impl ChatterClient {
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
