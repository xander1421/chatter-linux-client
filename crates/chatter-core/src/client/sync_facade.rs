use super::*;

impl ChatterClient {
    // ── Data sync ───────────────────────────────────────────────────

    /// Sync channels from server and prefetch tokens.
    ///
    /// Call after `start()` / `login()` to ensure local data is fresh.
    /// The UI's `load_initial_data` calls this internally — SDK consumers
    /// should call it if they need server-fresh data.
    pub async fn sync(&self) {
        // Sync channels from server
        match self.channels().list_channels(100, 0, None).await {
            Ok(channels) => tracing::info!("Synced {} channels from server", channels.len()),
            Err(e) => tracing::warn!("Failed to sync channels: {}", e),
        }

        // Prefetch anonymous tokens for sealed sender
        if let Err(e) = self.prefetch_tokens(10).await {
            tracing::warn!("Failed to prefetch tokens: {}", e);
        }
    }

}
