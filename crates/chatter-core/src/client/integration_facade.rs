use super::*;

impl ChatterClient {
    // ── Thread support ─────────────────────────────────────────────

    // ── AI Integration ─────────────────────────────────────────────

    /// List available AI models for a provider.
    pub async fn list_ai_models(
        &self,
        provider: &str,
    ) -> Result<Vec<(String, String)>, ClientError> {
        self.ai()
            .list_models_by_name(provider)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Create an AI integration and attach it to a channel.
    pub async fn create_ai_integration(
        &self,
        name: &str,
        provider: &str,
        model: &str,
        channel_id: &str,
    ) -> Result<(), ClientError> {
        self.ai()
            .create_integration_for_channel(name, provider, model, channel_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

}
