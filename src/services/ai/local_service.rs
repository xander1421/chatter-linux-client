//! Local AI Service - High-level orchestrator for local AI operations.
//!
//! Provides a clean API for UI to interact with local AI without dealing
//! with actor details or feature flags.

use std::sync::Arc;

use crate::storage::repositories::settings_repo::SettingsRepository;

use super::{LocalAIActorHandle, LocalAIError, ModelInfo, SampleParams};

/// High-level service for local AI operations.
///
/// Wraps the LocalAIActorHandle and provides convenience methods for common
/// operations like summarization and smart replies. Handles settings persistence
/// for model preferences.
pub struct LocalAIService {
    actor: LocalAIActorHandle,
    settings_repo: Arc<SettingsRepository>,
}

impl LocalAIService {
    /// Create a new LocalAIService.
    pub fn new(actor: LocalAIActorHandle, settings_repo: Arc<SettingsRepository>) -> Self {
        Self { actor, settings_repo }
    }

    /// Check if local AI is available and enabled.
    ///
    /// This checks both the compile-time feature flag and the user's preference.
    pub fn is_available(&self) -> bool {
        self.settings_repo.get_local_ai_enabled().unwrap_or(false)
    }

    /// Load the user's preferred model from settings.
    ///
    /// Returns `None` if local AI is disabled or no model is configured.
    pub async fn load_preferred_model(&self) -> Result<Option<ModelInfo>, LocalAIError> {
        if !self.settings_repo.get_local_ai_enabled().unwrap_or(false) {
            return Ok(None);
        }

        match self.settings_repo.get_local_ai_model() {
            Ok(Some((model_id, quantized))) => {
                let info = self.actor.load_model(model_id, quantized).await?;
                Ok(Some(info))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load local AI model settings");
                Ok(None)
            }
        }
    }

    /// Summarize a conversation.
    ///
    /// Takes a list of message strings and returns a 2-3 sentence summary.
    pub async fn summarize(&self, messages: &[String]) -> Result<String, LocalAIError> {
        if messages.is_empty() {
            return Ok(String::new());
        }

        let prompt = format!(
            "Summarize this conversation in 2-3 sentences:\n\n{}",
            messages.join("\n")
        );

        self.actor.complete(prompt, SampleParams::default()).await
    }

    /// Generate a smart reply suggestion.
    ///
    /// Given conversation context and the last message, suggests a brief, natural reply.
    pub async fn smart_reply(&self, context: &str, last_message: &str) -> Result<String, LocalAIError> {
        let prompt = format!(
            "Given this conversation context:\n{}\n\nSuggest a brief, natural reply to: \"{}\"\n\nReply:",
            context, last_message
        );

        let params = SampleParams {
            max_tokens: 100, // Keep replies concise
            ..Default::default()
        };

        self.actor.complete(prompt, params).await
    }

    /// Generate text completion with custom prompt and parameters.
    pub async fn complete(&self, prompt: String, params: SampleParams) -> Result<String, LocalAIError> {
        self.actor.complete(prompt, params).await
    }

    /// Load a specific model.
    pub async fn load_model(&self, model_id: String, quantized_file: Option<String>) -> Result<ModelInfo, LocalAIError> {
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
    pub fn set_preferred_model(&self, model_id: &str, quantized_file: Option<&str>) -> Result<(), LocalAIError> {
        self.settings_repo
            .set_local_ai_model(model_id, quantized_file)
            .map_err(|e| LocalAIError::LoadFailed(format!("Failed to save model settings: {}", e)))
    }

    /// Enable or disable local AI.
    pub fn set_enabled(&self, enabled: bool) -> Result<(), LocalAIError> {
        self.settings_repo
            .set_local_ai_enabled(enabled)
            .map_err(|e| LocalAIError::LoadFailed(format!("Failed to save enabled state: {}", e)))
    }
}
