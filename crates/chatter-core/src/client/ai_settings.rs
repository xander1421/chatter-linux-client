use super::*;

impl ChatterClient {
    // ── Local AI settings façade ─────────────────────────────────────────

    pub fn set_local_ai_enabled(
        &self,
        enabled: bool,
    ) -> Result<(), crate::services::ai::LocalAIError> {
        self.local_ai().set_enabled(enabled)
    }

    pub fn local_ai_enabled(&self) -> Result<bool, crate::services::ai::LocalAIError> {
        self.local_ai().enabled()
    }

    // ── Cloud AI silo façade ─────────────────────────────────────────────
    //
    // Thin delegation: every decision (policy clamp, availability, key
    // handling) lives in chatter-core services, per the architecture rule.

    /// Current cloud-silo configuration.
    pub fn cloud_ai_settings(&self) -> crate::cloud_ai::CloudSettings {
        self.http_local_ai().cloud_settings()
    }

    /// Choose between the user's own API key and the metered server broker.
    pub fn set_cloud_ai_mode(
        &self,
        mode: crate::cloud_ai::CloudMode,
    ) -> Result<(), crate::error::StorageError> {
        self.http_local_ai().set_cloud_mode(mode)
    }

    /// Store (or clear, when blank) the user's own provider API key.
    pub fn set_cloud_ai_api_key(&self, key: &str) -> Result<(), crate::error::StorageError> {
        self.http_local_ai().set_anthropic_api_key(key)
    }

    /// Which silo the user prefers where policy allows a choice.
    pub fn ai_silo_preference(&self) -> crate::ai_policy::channel_ai::SiloPreference {
        self.http_local_ai().silo_preference()
    }

    pub fn set_ai_silo_preference(
        &self,
        preference: crate::ai_policy::channel_ai::SiloPreference,
    ) -> Result<(), crate::error::StorageError> {
        self.http_local_ai().set_silo_preference(preference)
    }

    /// Is AI switched on for this conversation? Off unless explicitly enabled.
    pub fn ai_enabled_for(&self, conversation_id: &str) -> bool {
        self.http_local_ai().ai_enabled_for(conversation_id)
    }

    pub fn set_ai_enabled_for(
        &self,
        conversation_id: &str,
        enabled: bool,
    ) -> Result<(), crate::error::StorageError> {
        self.http_local_ai()
            .set_ai_enabled_for(conversation_id, enabled)
    }

    /// What would actually run for this conversation, after the mesh/DM policy
    /// clamp. The UI uses this to explain *why* a channel shows local-only.
    pub fn resolve_ai_for(
        &self,
        conversation_id: &str,
        conversation: &crate::domain::message::ConversationType,
        delivered_via_mesh: bool,
    ) -> crate::ai_policy::channel_ai::Resolved {
        self.http_local_ai()
            .resolve_ai_for(conversation_id, conversation, delivered_via_mesh)
    }

    /// Token balance and lifetime spend on the metered broker.
    ///
    /// Only meaningful in `CloudMode::ServerBroker`; in bring-your-own-key
    /// mode the user is billed by the provider directly and we have no view of
    /// it.
    pub async fn cloud_ai_quota(&self) -> Result<(i64, i64), crate::error::NetworkError> {
        let quota = self.services.grpc_pool.ai_client().get_quota().await?;
        Ok((quota.balance, quota.spent_total))
    }

    pub fn set_local_ai_preferred_model(
        &self,
        model_id: &str,
        quantized_file: Option<&str>,
    ) -> Result<(), crate::services::ai::LocalAIError> {
        self.local_ai()
            .set_preferred_model(model_id, quantized_file)
    }

    pub async fn load_local_ai_model(
        &self,
        model_id: String,
        quantized_file: Option<String>,
    ) -> Result<crate::services::ai::ModelInfo, crate::services::ai::LocalAIError> {
        self.local_ai().load_model(model_id, quantized_file).await
    }

    pub async fn unload_local_ai_model(&self) -> Result<(), crate::services::ai::LocalAIError> {
        self.local_ai().unload_model().await
    }

}
