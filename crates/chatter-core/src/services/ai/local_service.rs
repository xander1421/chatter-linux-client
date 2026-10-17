//! Local AI Service - High-level orchestrator for local AI operations.
//!
//! Provides a clean API for UI to interact with local AI without dealing
//! with actor details or feature flags.

use std::sync::Arc;

use crate::storage::repositories::settings_repo::SettingsRepository;

use super::{LocalAIActorHandle, LocalAIError, ModelInfo};

/// Default model to use when none is configured.
/// Qwen2.5-7B-Instruct is a good balance of quality and speed on consumer GPUs.
pub const DEFAULT_MODEL_ID: &str = "Qwen/Qwen2.5-7B-Instruct-GGUF";

/// High-level service for local AI operations.
///
/// Wraps the LocalAIActorHandle and provides convenience methods for common
/// operations like summarization and smart replies. Handles settings persistence
/// for model preferences.
pub struct LocalAIService {
    actor: LocalAIActorHandle,
    settings_repo: Arc<SettingsRepository>,
    http_service:
        parking_lot::Mutex<Option<Arc<crate::services::local_ai_service::HttpLocalAIService>>>,
}

impl LocalAIService {
    /// Create a new LocalAIService.
    pub fn new(actor: LocalAIActorHandle, settings_repo: Arc<SettingsRepository>) -> Self {
        Self {
            actor,
            settings_repo,
            http_service: parking_lot::Mutex::new(None),
        }
    }

    /// Handle to the underlying inference actor (for engine rebuilds).
    pub fn actor(&self) -> LocalAIActorHandle {
        self.actor.clone()
    }

    /// Check if local AI is available and enabled.
    ///
    /// This checks both the compile-time feature flag and the user's preference.
    pub fn is_available(&self) -> bool {
        #[cfg(not(feature = "local-ai"))]
        return false;

        #[cfg(feature = "local-ai")]
        // Fail closed: a settings-read error must not silently turn on a
        // multi-GB on-device model download the user never opted into.
        self.settings_repo.get_local_ai_enabled().unwrap_or(false)
    }

    /// Load the user's preferred model from settings, or default model.
    ///
    /// Local AI is off by default (opt-in) — get_local_ai_enabled() is the
    /// single source of truth for that default. Uses DEFAULT_MODEL_ID if no
    /// model configured.
    pub async fn load_preferred_model(&self) -> Result<Option<ModelInfo>, LocalAIError> {
        if !self.is_available() {
            tracing::debug!("Local AI not enabled, skipping model load");
            return Ok(None);
        }

        // Get configured model or use default
        let (model_id, quantized) = match self.settings_repo.get_local_ai_model() {
            Ok(Some((id, q))) => (id, q),
            Ok(None) => {
                tracing::info!("No model configured, using default: {}", DEFAULT_MODEL_ID);
                (DEFAULT_MODEL_ID.to_string(), None)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to read model settings, using default");
                (DEFAULT_MODEL_ID.to_string(), None)
            }
        };

        // Load the model
        let info = self.actor.load_model(model_id, quantized).await?;
        Ok(Some(info))
    }

    /// Load a specific model.
    pub async fn load_model(
        &self,
        model_id: String,
        quantized_file: Option<String>,
    ) -> Result<ModelInfo, LocalAIError> {
        self.actor.load_model(model_id, quantized_file).await
    }

    /// Unload the current model to free GPU memory.
    pub async fn unload_model(&self) -> Result<(), LocalAIError> {
        self.actor.unload_model().await
    }

    /// Get information about the currently loaded model.
    pub async fn get_model_info(&self) -> Result<Option<ModelInfo>, LocalAIError> {
        self.actor.get_model_info().await
    }

    /// Set the user's preferred model and persist to settings.
    pub fn set_preferred_model(
        &self,
        model_id: &str,
        quantized_file: Option<&str>,
    ) -> Result<(), LocalAIError> {
        self.settings_repo
            .set_local_ai_model(model_id, quantized_file)
            .map_err(|e| LocalAIError::LoadFailed(format!("Failed to save model settings: {}", e)))
    }

    pub fn attach_http_service(
        &self,
        service: Arc<crate::services::local_ai_service::HttpLocalAIService>,
    ) {
        *self.http_service.lock() = Some(service);
    }

    /// Enable or disable local AI.
    pub fn set_enabled(&self, enabled: bool) -> Result<(), LocalAIError> {
        self.settings_repo
            .set_local_ai_enabled(enabled)
            .map_err(|e| {
                LocalAIError::LoadFailed(format!("Failed to save enabled state: {}", e))
            })?;
        if let Some(service) = self.http_service.lock().as_ref() {
            service.refresh_indexing();
        }
        Ok(())
    }

    /// Read the persisted opt-in independently of compile-time model availability.
    pub fn enabled(&self) -> Result<bool, LocalAIError> {
        self.settings_repo
            .get_local_ai_enabled()
            .map_err(|e| LocalAIError::LoadFailed(format!("Failed to read enabled state: {}", e)))
    }
}
