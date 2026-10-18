use super::*;

impl ChatterClient {
    // ── Settings ────────────────────────────────────────────────────

    /// Enable or disable sending read receipts, on every one of our devices.
    pub fn set_read_receipts_enabled(&self, enabled: bool) -> Result<(), SessionError> {
        self.sessions()
            .set_bool_setting(settings_keys::SEND_READ_RECEIPTS, enabled)?;
        if !enabled {
            self.pending_receipts.lock().retain(|entry| !entry.is_read());
        }
        self.sync_configuration_to_own_devices();
        Ok(())
    }

    /// Enable or disable typing indicators, on every one of our devices.
    pub fn set_typing_indicators_enabled(&self, enabled: bool) -> Result<(), SessionError> {
        self.sessions()
            .set_bool_setting(settings_keys::SHOW_TYPING_INDICATORS, enabled)?;
        self.sync_configuration_to_own_devices();
        Ok(())
    }

    /// Send the current privacy settings to our other devices
    /// (`services::configuration_sync`). Runs on the client runtime: the
    /// setters are called from the UI thread. Best effort, like every
    /// own-device sync; a device that misses it keeps the old values until
    /// the next change.
    fn sync_configuration_to_own_devices(&self) {
        let messages = Arc::clone(self.messages());
        let content = crate::services::configuration_sync::content(
            &messages.settings_repo,
            crate::util::time::TimestampNanos::now().as_u64(),
        );
        self.services.runtime_handle.spawn(async move {
            let devices = messages
                .sync_to_own_devices(&mut messages.auth_client(), content)
                .await;
            tracing::info!(devices, "ConfigurationSync sent to our other devices");
        });
    }

    /// Check if read receipts are enabled (defaults to true).
    pub fn read_receipts_enabled(&self) -> Result<bool, SessionError> {
        Ok(self
            .sessions()
            .get_bool_setting(settings_keys::SEND_READ_RECEIPTS)?
            .unwrap_or(true))
    }

    /// Check if typing indicators are enabled (defaults to true).
    pub fn typing_indicators_enabled(&self) -> Result<bool, SessionError> {
        Ok(self
            .sessions()
            .get_bool_setting(settings_keys::SHOW_TYPING_INDICATORS)?
            .unwrap_or(true))
    }

}
