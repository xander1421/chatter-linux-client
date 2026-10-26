use super::*;

impl ChatterClient {
    // ── UI helpers / formatting ─────────────────────────────────────────

    /// Truncate a displayed message excerpt to 100 graphemes and isolate its direction.
    pub fn truncate_message_preview(&self, text: &str) -> String {
        crate::domain::message::format_preview_body("", text, Some(100))
    }

    /// Convert a [`SendError`] into a user-facing error string (also logs the
    /// error with the given context). Keeps UI error-formatting consistent
    /// across send call sites.
    pub fn format_send_error(&self, err: &SendError, context: &str) -> String {
        crate::services::gateway::shared::handle_send_error(err, context)
    }

    /// Password strength level (0–4). See
    /// [`crate::services::validation::password_strength`] for scoring details.
    pub fn password_strength(&self, password: &str) -> i32 {
        crate::services::validation::password_strength(password)
    }

    /// PIN strength level (0–3). See
    /// [`crate::services::validation::pin_strength`] for scoring details.
    pub fn pin_strength(&self, pin: &str) -> i32 {
        crate::services::validation::pin_strength(pin)
    }

    // ── Bridge façades ───────────────────────────────────────────────
    //
    // Thin wrappers kept here (not in bin-specific modules) so the
    // `chatter-bridge` crate can reach every operation it needs without
    // widening `pub(crate)` service accessors. Do not expand this block
    // speculatively — add an entry only when a real caller lands.

    /// Persist the notification detail level (Signal's "Show: Name and
    /// message / Name only / No name or message") and apply it now.
    ///
    /// Persist first: a level that takes effect but does not survive a restart
    /// silently reverts to the private default, and one that is saved but not
    /// applied leaks until the next launch. Startup reads the same key
    /// (`app/core_services.rs`, `NOTIFICATION_DETAIL_LEVEL`).
    pub fn set_notification_detail_level(
        &self,
        level: crate::services::notification_service::NotificationDetailLevel,
    ) -> Result<(), crate::error::StorageError> {
        use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
        SettingsRepository::new(self.services.database.clone())
            .set_string(keys::NOTIFICATION_DETAIL_LEVEL, level.as_str())?;
        self.services.notification_service.set_detail_level(level);
        Ok(())
    }

    pub fn notification_detail_level(
        &self,
    ) -> crate::services::notification_service::NotificationDetailLevel {
        self.services.notification_service.detail_level()
    }

}
