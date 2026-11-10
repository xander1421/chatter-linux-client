//! Model manager.
//!
//! Download, update, and switch models for local AI backends.
//! Handles model lifecycle: checking availability, pulling if needed,
//! listing installed models, and cleanup.

use crate::ai_protocol::{AvailableModel, BackendError, LocalAIBackend};

/// Model manager for local AI backends.
pub struct ModelManager<B: LocalAIBackend> {
    backend: B,
}

impl<B: LocalAIBackend> ModelManager<B> {
    /// Create a new ModelManager with the given backend.
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    /// Check if a specific model is available locally.
    pub async fn is_model_available(&self, model_name: &str) -> Result<bool, BackendError> {
        let models = self.backend.list_models().await?;
        Ok(models.iter().any(|m| m.name == model_name))
    }

    /// Ensure a model is available, pulling it if not present.
    ///
    /// Returns the model info if found/pulled successfully.
    pub async fn ensure_model(&self, model_name: &str) -> Result<AvailableModel, BackendError> {
        let models = self.backend.list_models().await?;

        if let Some(model) = models.into_iter().find(|m| m.name == model_name) {
            return Ok(model);
        }

        tracing::info!(model = model_name, "Model not found locally, pulling...");
        self.backend.pull_model(model_name).await?;

        // Re-list to get model info after pull
        let models = self.backend.list_models().await?;
        models
            .into_iter()
            .find(|m| m.name == model_name)
            .ok_or_else(|| {
                BackendError::ModelNotFound(format!("Model {} not found after pull", model_name))
            })
    }

    /// List all locally available models.
    pub async fn list_models(&self) -> Result<Vec<AvailableModel>, BackendError> {
        self.backend.list_models().await
    }

    /// Pull/download a model.
    pub async fn pull_model(&self, model_name: &str) -> Result<(), BackendError> {
        self.backend.pull_model(model_name).await
    }

    /// Delete a model from local storage.
    pub async fn delete_model(&self, model_name: &str) -> Result<(), BackendError> {
        self.backend.delete_model(model_name).await
    }

    /// Check if the backend is healthy.
    pub async fn is_backend_healthy(&self) -> bool {
        self.backend.health_check().await.unwrap_or(false)
    }

    /// Get the backend name.
    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }
}
