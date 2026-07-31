//! Local AI service — HTTP backend orchestrator.
//!
//! Unifies Ollama (and future vLLM / MLC-LLM) backends behind a single service
//! that provides chat completion, model management, embedding generation,
//! semantic search, and health monitoring.
//!
//! This is separate from `services::ai::LocalAIService` which drives *in-process*
//! GPU inference via mistralrs.  This service talks to *external* inference
//! servers over HTTP.

use std::sync::Arc;

use crate::error::ServiceError;
use crate::local_ai::backend::{
    AvailableModel, BackendError, ChatMessage, ChatResponse, GenerateParams, LocalAIBackend,
};
use crate::ai_policy::channel_ai::{self, SiloPreference};
use crate::cloud_ai::{CloudMode, CloudProviderKind, CloudSettings};
use crate::local_ai::embeddings::EmbeddingGenerator;
use crate::local_ai::model_manager::ModelManager;
use crate::local_ai::ollama::OllamaBackend;
use crate::local_ai::settings::LocalAISettings;
use crate::local_ai::vector_store::VectorSearchResult;
use crate::services::semantic_search::SemanticSearchService;
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::vector_repo::VectorRepository;

/// HTTP-backed local AI service.
///
/// Wraps an Ollama (or future vLLM/MLC-LLM) server and provides:
/// - Chat completion
/// - Model management (list, pull, ensure, delete)
/// - Embedding generation and semantic search
/// - Health checks
pub struct HttpLocalAIService {
    backend: Arc<OllamaBackend>,
    model_manager: ModelManager<OllamaBackend>,
    semantic_search: SemanticSearchService,
    settings: LocalAISettings,
    settings_repo: Arc<SettingsRepository>,
}

/// Errors specific to the HTTP local AI service.
#[derive(Debug, thiserror::Error)]
pub enum HttpLocalAIError {
    #[error("backend error: {0}")]
    Backend(#[from] BackendError),
    #[error("service error: {0}")]
    Service(#[from] ServiceError),
    #[error("local AI is disabled")]
    Disabled,
}

impl HttpLocalAIService {
    /// Create a new service from infrastructure dependencies.
    ///
    /// Reads settings to determine which backend and models to use.
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        vector_repo: Arc<VectorRepository>,
    ) -> Self {
        let settings = Self::load_settings(&settings_repo);

        let backend = Arc::new(OllamaBackend::new(
            &settings.ollama_url,
            settings.request_timeout,
        ));

        // The model manager borrows a *separate* OllamaBackend because
        // ModelManager<B> owns its backend.  Cheap to construct (just an
        // HTTP client + URL).
        let manager_backend =
            OllamaBackend::new(&settings.ollama_url, settings.request_timeout);
        let model_manager = ModelManager::new(manager_backend);

        let embedding_gen = Arc::new(EmbeddingGenerator::new(
            OllamaBackend::new(&settings.ollama_url, settings.request_timeout),
            &settings,
        ));

        let semantic_search = SemanticSearchService::new(vector_repo, embedding_gen);

        Self {
            backend,
            model_manager,
            semantic_search,
            settings,
            settings_repo,
        }
    }

    // ==================== Chat ====================

    /// Generate a chat completion using the configured chat model.
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        params: Option<GenerateParams>,
    ) -> Result<ChatResponse, HttpLocalAIError> {
        self.check_enabled()?;

        let params = params.unwrap_or_else(|| GenerateParams {
            max_tokens: self.settings.max_tokens,
            temperature: self.settings.temperature,
            ..Default::default()
        });

        self.backend
            .chat(&self.settings.chat_model, messages, &params)
            .await
            .map_err(Into::into)
    }

    /// Simple prompt → response helper (wraps as a user message).
    pub async fn prompt(&self, text: &str) -> Result<String, HttpLocalAIError> {
        let messages = vec![ChatMessage {
            role: crate::local_ai::backend::ChatRole::User,
            content: text.to_string(),
        }];
        let response = self.chat(&messages, None).await?;
        Ok(response.content)
    }

    // ==================== Model Management ====================

    /// List models available on the backend server.
    pub async fn list_models(&self) -> Result<Vec<AvailableModel>, HttpLocalAIError> {
        self.model_manager.list_models().await.map_err(Into::into)
    }

    /// Pull/download a model on the backend server.
    pub async fn pull_model(&self, name: &str) -> Result<(), HttpLocalAIError> {
        self.model_manager.pull_model(name).await.map_err(Into::into)
    }

    /// Ensure the configured chat model is available, pulling if needed.
    pub async fn ensure_chat_model(&self) -> Result<AvailableModel, HttpLocalAIError> {
        self.model_manager
            .ensure_model(&self.settings.chat_model)
            .await
            .map_err(Into::into)
    }

    /// Ensure the configured embedding model is available, pulling if needed.
    pub async fn ensure_embedding_model(&self) -> Result<AvailableModel, HttpLocalAIError> {
        self.model_manager
            .ensure_model(&self.settings.embedding_model)
            .await
            .map_err(Into::into)
    }

    /// Delete a model from the backend server.
    pub async fn delete_model(&self, name: &str) -> Result<(), HttpLocalAIError> {
        self.model_manager
            .delete_model(name)
            .await
            .map_err(Into::into)
    }

    // ==================== Semantic Search ====================

    /// Search messages in a channel by semantic similarity.
    pub async fn search_channel(
        &self,
        channel_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<VectorSearchResult>, HttpLocalAIError> {
        self.check_enabled()?;
        self.semantic_search
            .search_channel(channel_id, query, limit)
            .await
            .map_err(Into::into)
    }

    /// Search across all indexed messages.
    pub async fn search_global(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<VectorSearchResult>, HttpLocalAIError> {
        self.check_enabled()?;
        self.semantic_search
            .search_global(query, limit)
            .await
            .map_err(Into::into)
    }

    /// Index a single message for semantic search.
    pub async fn index_message(
        &self,
        message_id: &str,
        channel_id: &str,
        content: &str,
    ) -> Result<bool, HttpLocalAIError> {
        self.semantic_search
            .index_message(message_id, channel_id, content)
            .await
            .map_err(Into::into)
    }

    /// Index a batch of messages for semantic search.
    ///
    /// Tuples are `(message_id, channel_id, content)`.
    pub async fn index_messages_batch(
        &self,
        messages: &[(String, String, String)],
    ) -> Result<u32, HttpLocalAIError> {
        self.semantic_search
            .index_messages_batch(messages)
            .await
            .map_err(Into::into)
    }

    /// Delete the semantic search index for a channel.
    pub fn delete_channel_index(&self, channel_id: &str) -> Result<u32, HttpLocalAIError> {
        self.semantic_search
            .delete_channel_index(channel_id)
            .map_err(Into::into)
    }

    /// Get indexing statistics.
    pub fn index_stats(
        &self,
        channel_id: Option<&str>,
    ) -> Result<crate::services::semantic_search::IndexStats, HttpLocalAIError> {
        self.semantic_search.stats(channel_id).map_err(Into::into)
    }

    // ==================== Health & Settings ====================

    /// Check if the backend server is healthy.
    pub async fn health_check(&self) -> bool {
        self.model_manager.is_backend_healthy().await
    }

    /// Get the backend name.
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Check if local AI is enabled in settings.
    pub fn is_enabled(&self) -> bool {
        self.settings.enabled
    }

    /// Whether auto-indexing is enabled.
    pub fn is_auto_index_enabled(&self) -> bool {
        self.settings.enabled && self.settings.auto_index
    }

    /// Get current settings (snapshot).
    pub fn settings(&self) -> &LocalAISettings {
        &self.settings
    }

    /// Get the underlying SemanticSearchService (for the indexer).
    pub fn semantic_search(&self) -> &SemanticSearchService {
        &self.semantic_search
    }

    /// Persist the active provider choice.
    pub fn set_active_provider(
        &self,
        kind: crate::local_ai::providers::ProviderKind,
    ) -> Result<(), crate::error::StorageError> {
        self.settings_repo
            .set_string("local_ai.active_provider", kind.as_str())
    }

    /// Persist the chat model choice.
    pub fn set_chat_model(&self, model: &str) -> Result<(), crate::error::StorageError> {
        self.settings_repo.set_string("local_ai.chat_model", model)
    }

    /// Persist the Anthropic API key. Empty clears it (cloud provider off).
    pub fn set_anthropic_api_key(&self, key: &str) -> Result<(), crate::error::StorageError> {
        let key = key.trim();
        if key.is_empty() {
            self.settings_repo.delete("cloud_ai.api_key")?;
            return Ok(());
        }
        self.settings_repo.set_string("cloud_ai.api_key", key)
    }

    /// Persist whether the cloud silo uses the user's own key or the metered
    /// server broker.
    pub fn set_cloud_mode(&self, mode: CloudMode) -> Result<(), crate::error::StorageError> {
        self.settings_repo.set_string("cloud_ai.mode", mode.as_str())
    }

    /// Persist the model used when the active provider is a cloud one.
    pub fn set_cloud_model(&self, model: &str) -> Result<(), crate::error::StorageError> {
        self.settings_repo.set_string("cloud_ai.model", model)
    }

    // ==================== Cloud silo ====================

    /// Current cloud-silo configuration, read fresh from the repo.
    ///
    /// Deliberately a separate struct from `LocalAISettings`: the API key is a
    /// cloud-only secret and has no business in the local provider's config.
    pub fn cloud_settings(&self) -> CloudSettings {
        Self::load_cloud_settings(&self.settings_repo)
    }

    fn load_cloud_settings(repo: &SettingsRepository) -> CloudSettings {
        let mut settings = CloudSettings::default();

        if let Ok(Some(m)) = repo.get_string("cloud_ai.mode") {
            if let Some(mode) = CloudMode::parse(&m) {
                settings.mode = mode;
            }
        }
        if let Ok(Some(p)) = repo.get_string("cloud_ai.provider") {
            if let Some(kind) = CloudProviderKind::parse(&p) {
                settings.provider = kind;
                settings.model = kind.default_model().to_string();
            }
        }
        if let Ok(Some(model)) = repo.get_string("cloud_ai.model") {
            settings.model = model;
        }
        if let Ok(Some(key)) = repo.get_string("cloud_ai.api_key") {
            let key = key.trim().to_string();
            if !key.is_empty() {
                settings.api_key = Some(key);
            }
        }

        settings
    }

    /// The user's silo preference. Local-only unless explicitly changed.
    pub fn silo_preference(&self) -> SiloPreference {
        self.settings_repo
            .get_string(channel_ai::SILO_PREFERENCE_KEY)
            .ok()
            .flatten()
            .and_then(|s| SiloPreference::parse(&s))
            .unwrap_or_default()
    }

    /// Persist the silo preference.
    pub fn set_silo_preference(
        &self,
        preference: SiloPreference,
    ) -> Result<(), crate::error::StorageError> {
        self.settings_repo
            .set_string(channel_ai::SILO_PREFERENCE_KEY, preference.as_str())
    }

    // ==================== Per-conversation AI ====================

    /// Is AI switched on for this conversation? Off unless explicitly enabled.
    pub fn ai_enabled_for(&self, conversation_id: &str) -> bool {
        self.settings_repo
            .get_bool(&channel_ai::enabled_key(conversation_id))
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// Turn AI on or off for one conversation.
    pub fn set_ai_enabled_for(
        &self,
        conversation_id: &str,
        enabled: bool,
    ) -> Result<(), crate::error::StorageError> {
        self.settings_repo
            .set_bool(&channel_ai::enabled_key(conversation_id), enabled)
    }

    /// What should actually run for this conversation, after policy.
    ///
    /// The single entry point a caller needs: combines the per-conversation
    /// switch, the global provider, and the mesh/DM policy clamp. A
    /// `Resolved::Use(_)` here is safe to run; anything else must not.
    pub fn resolve_ai_for(
        &self,
        conversation_id: &str,
        conversation: &crate::domain::message::ConversationType,
        delivered_via_mesh: bool,
    ) -> channel_ai::Resolved {
        let access = crate::ai_policy::policy::ai_access(conversation, delivered_via_mesh);
        let settings = self.fresh_settings();

        let _ = settings;
        channel_ai::resolve(
            self.ai_enabled_for(conversation_id),
            self.silo_preference(),
            self.cloud_settings().is_available(),
            access,
        )
    }

    /// Re-read settings from the repository (post-change snapshot).
    pub fn fresh_settings(&self) -> LocalAISettings {
        Self::load_settings(&self.settings_repo)
    }

    // ==================== Internal ====================

    fn check_enabled(&self) -> Result<(), HttpLocalAIError> {
        if !self.settings.enabled {
            return Err(HttpLocalAIError::Disabled);
        }
        Ok(())
    }

    fn load_settings(repo: &SettingsRepository) -> LocalAISettings {
        // Try to load from settings repo; fall back to defaults
        let mut settings = LocalAISettings::default();

        if let Ok(Some(url)) = repo.get_string("local_ai.ollama_url") {
            settings.ollama_url = url;
        }
        if let Ok(Some(provider)) = repo.get_string("local_ai.active_provider") {
            if let Some(kind) = crate::local_ai::providers::ProviderKind::parse(&provider) {
                settings.active_provider = kind;
            }
        }
        if let Ok(Some(model)) = repo.get_string("local_ai.chat_model") {
            settings.chat_model = model;
        }
        if let Ok(Some(model)) = repo.get_string("local_ai.embedding_model") {
            settings.embedding_model = model;
        }
        if let Ok(false) = repo.get_local_ai_enabled() {
            // Only disable if explicitly set to false
            if repo.get_string("local_ai.enabled").ok().flatten().is_some() {
                settings.enabled = false;
            }
        }

        settings
    }
}

#[cfg(test)]
mod per_conversation_ai_tests {
    use super::*;
    use crate::domain::message::ConversationType;
    use crate::ai_policy::channel_ai::Resolved;
    use crate::ai_policy::policy::Silo;
    use crate::cloud_ai::CloudMode;
    use crate::local_ai::providers::ProviderKind;
    use crate::storage::Database;

    /// Same pattern as the other repository tests: SQLCipher needs a real path.
    fn service() -> HttpLocalAIService {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(
            Database::open(&dir.path().join("local_ai_test.db"), "testpass").expect("open"),
        );
        db.run_migrations().expect("migrations");
        Box::leak(Box::new(dir));

        let settings_repo = Arc::new(SettingsRepository::new(db.clone()));
        let vector_repo = Arc::new(VectorRepository::new(db));
        HttpLocalAIService::new(settings_repo, vector_repo)
    }

    #[test]
    fn ai_is_off_for_a_conversation_until_explicitly_enabled() {
        let svc = service();
        assert!(!svc.ai_enabled_for("chan-a"));
        assert_eq!(
            svc.resolve_ai_for("chan-a", &ConversationType::Channel, false),
            Resolved::Off
        );
    }

    #[test]
    fn enabling_one_conversation_does_not_enable_another() {
        let svc = service();
        svc.set_ai_enabled_for("chan-a", true).expect("set");

        assert!(svc.ai_enabled_for("chan-a"));
        assert!(!svc.ai_enabled_for("chan-b"));

        assert_eq!(
            svc.resolve_ai_for("chan-a", &ConversationType::Channel, false),
            Resolved::Use(Silo::Local)
        );
        assert_eq!(
            svc.resolve_ai_for("chan-b", &ConversationType::Channel, false),
            Resolved::Off
        );
    }

    #[test]
    fn enabled_conversation_still_refuses_mesh_delivered_traffic() {
        let svc = service();
        svc.set_ai_enabled_for("chan-a", true).expect("set");

        assert_eq!(
            svc.resolve_ai_for("chan-a", &ConversationType::Channel, true),
            Resolved::Off,
        );
    }

    #[test]
    fn enabled_dm_resolves_to_the_local_silo_even_with_cloud_configured() {
        let svc = service();
        svc.set_ai_enabled_for("dm-1", true).expect("set");
        // Cloud fully configured and preferred — the DM must still not use it.
        svc.set_anthropic_api_key("sk-ant-example").expect("key");
        svc.set_silo_preference(SiloPreference::PreferCloud).expect("pref");

        assert!(svc.cloud_settings().is_available());
        assert_eq!(
            svc.resolve_ai_for("dm-1", &ConversationType::ServerDm, false),
            Resolved::Use(Silo::Local),
        );
        // Same configuration on a channel does reach the cloud silo.
        svc.set_ai_enabled_for("chan-x", true).expect("set");
        assert_eq!(
            svc.resolve_ai_for("chan-x", &ConversationType::Channel, false),
            Resolved::Use(Silo::Cloud),
        );
    }

    #[test]
    fn cloud_preference_without_a_key_stays_local_on_a_channel() {
        let svc = service();
        svc.set_ai_enabled_for("chan-x", true).expect("set");
        svc.set_silo_preference(SiloPreference::PreferCloud).expect("pref");

        assert_eq!(
            svc.resolve_ai_for("chan-x", &ConversationType::Channel, false),
            Resolved::Use(Silo::Local),
        );
    }

    #[test]
    fn cloud_mode_round_trips_and_changes_availability() {
        let svc = service();

        // Default is bring-your-own-key, and with no key the silo is off.
        assert_eq!(svc.cloud_settings().mode, CloudMode::DirectAnthropic);
        assert!(!svc.cloud_settings().is_available());

        // Broker mode needs no key - the server holds the org key.
        svc.set_cloud_mode(CloudMode::ServerBroker).expect("set mode");
        assert_eq!(svc.cloud_settings().mode, CloudMode::ServerBroker);
        assert!(svc.cloud_settings().is_available());

        // Back to BYOK: unavailable again until a key is set.
        svc.set_cloud_mode(CloudMode::DirectAnthropic).expect("set mode");
        assert!(!svc.cloud_settings().is_available());
        svc.set_anthropic_api_key("sk-ant-x").expect("set key");
        assert!(svc.cloud_settings().is_available());
    }

    #[test]
    fn silo_preference_round_trips_and_defaults_local(){
        let svc = service();
        assert_eq!(svc.silo_preference(), SiloPreference::LocalOnly);

        svc.set_silo_preference(SiloPreference::PreferCloud).expect("set");
        assert_eq!(svc.silo_preference(), SiloPreference::PreferCloud);
    }

    #[test]
    fn toggling_off_again_disables_it() {
        let svc = service();
        svc.set_ai_enabled_for("chan-a", true).expect("on");
        svc.set_ai_enabled_for("chan-a", false).expect("off");
        assert!(!svc.ai_enabled_for("chan-a"));
    }

    #[test]
    fn api_key_round_trips_and_clears() {
        let svc = service();
        svc.set_anthropic_api_key("sk-ant-example").expect("set");
        assert_eq!(
            svc.cloud_settings().api_key.as_deref(),
            Some("sk-ant-example")
        );
        assert!(svc.cloud_settings().is_available());

        svc.set_anthropic_api_key("   ").expect("clear via blank");
        assert!(svc.cloud_settings().api_key.is_none());
        assert!(!svc.cloud_settings().is_available());
    }
}
