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
        if let Ok(Some(model)) = repo.get_string("local_ai.cloud_model") {
            settings.cloud_model = model;
        }
        // Cloud API key. Absent → `AnyBackend::for_kind` refuses to build the
        // Anthropic backend, so the cloud provider stays off until a key is set.
        if let Ok(Some(key)) = repo.get_string("local_ai.anthropic_api_key") {
            let key = key.trim().to_string();
            if !key.is_empty() {
                settings.anthropic_api_key = Some(key);
            }
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
