//! AI service.
//!
//! Orchestrates AI integration operations using the IntegrationClient for server-side
//! AI chat (Claude, OpenAI, etc.), model queries, and event streaming.

use std::sync::Arc;

use futures::Stream;
use tokio_stream::StreamExt;

use crate::error::NetworkError;
use crate::network::grpc::integration_client::IntegrationClient;
use crate::proto::chat_v1::{
    AvailableModel, Integration, IntegrationConfig, IntegrationEvent, IntegrationType, ModelLimits,
};
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};

/// AI service errors.
#[derive(Debug, thiserror::Error)]
pub enum AIError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Model not found or unavailable.
    #[error("invalid model: model not found or unavailable")]
    InvalidModel,

    /// Invalid AI configuration.
    #[error("configuration error: {0}")]
    ConfigurationError(String),
}

/// AI service for cloud AI integrations.
///
/// Orchestrates AI integration operations using the network client for server-side
/// AI chat (Claude, OpenAI, etc.). Provides integration CRUD, model queries, and
/// event streaming for real-time AI responses.
///
/// # Example
///
/// ```ignore
/// let service = AIService::new(settings_repo);
///
/// // List available models for Claude
/// let models = service.list_available_models(&mut client, IntegrationType::Claude).await?;
///
/// // Create an integration
/// let integration = service.create_integration(&mut client, "my-claude-bot", IntegrationType::Claude, None).await?;
///
/// // Subscribe to AI streaming events
/// let stream = service.subscribe_events(&mut client, Some(&integration.id)).await?;
/// ```
pub struct AIService {
    settings_repo: Arc<SettingsRepository>,
}

impl AIService {
    /// Create a new AIService with dependencies.
    pub fn new(settings_repo: Arc<SettingsRepository>) -> Self {
        Self { settings_repo }
    }

    /// Get the currently logged-in user ID.
    fn current_user_id(&self) -> Result<String, AIError> {
        self.settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(|_| AIError::NotLoggedIn)?
            .ok_or(AIError::NotLoggedIn)
    }

    // ========================================================================
    // Model Queries
    // ========================================================================

    /// List available models for an AI provider.
    ///
    /// Queries the server for available models that can be used with the
    /// specified AI provider (Claude, OpenAI, etc.).
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to integration client
    /// * `provider` - AI provider type (Claude, OpenAI, GoogleAi, etc.)
    pub async fn list_available_models(
        &self,
        client: &mut IntegrationClient,
        provider: IntegrationType,
    ) -> Result<Vec<AvailableModel>, AIError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        let response = client.list_available_models(provider, None, None).await?;

        Ok(response.models)
    }

    /// Get token and parameter limits for a specific model.
    ///
    /// Returns the model's input/output token limits and other constraints.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to integration client
    /// * `provider` - AI provider type
    /// * `model` - Model identifier (e.g., "claude-3-opus", "gpt-4")
    pub async fn get_model_limits(
        &self,
        client: &mut IntegrationClient,
        provider: IntegrationType,
        model: &str,
    ) -> Result<ModelLimits, AIError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        let response = client.get_model_limits(provider, model).await?;

        // Return limits or error if not found
        response.limits.ok_or(AIError::InvalidModel)
    }

    // ========================================================================
    // Channel Integration Management
    // ========================================================================

    /// List all integrations associated with a channel.
    ///
    /// Returns all integrations (AI and non-AI) configured for the channel.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to integration client
    /// * `channel_id` - Channel to query integrations for
    pub async fn list_channel_integrations(
        &self,
        client: &mut IntegrationClient,
        channel_id: &str,
    ) -> Result<Vec<Integration>, AIError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        let response = client.get_channel_integrations(channel_id).await?;

        Ok(response.integrations)
    }

    /// Add an integration to a channel.
    ///
    /// Associates an existing integration with a channel, enabling its
    /// functionality within that channel.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to integration client
    /// * `integration_id` - Integration to add
    /// * `channel_id` - Target channel
    pub async fn add_integration_to_channel(
        &self,
        client: &mut IntegrationClient,
        integration_id: &str,
        channel_id: &str,
    ) -> Result<(), AIError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        client
            .add_integration_to_channel(integration_id, channel_id)
            .await?;

        Ok(())
    }

    /// Remove an integration from a channel.
    ///
    /// Disassociates an integration from a channel, disabling its
    /// functionality within that channel.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to integration client
    /// * `integration_id` - Integration to remove
    /// * `channel_id` - Target channel
    pub async fn remove_integration_from_channel(
        &self,
        client: &mut IntegrationClient,
        integration_id: &str,
        channel_id: &str,
    ) -> Result<(), AIError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        client
            .remove_integration_from_channel(integration_id, channel_id)
            .await?;

        Ok(())
    }

    // ========================================================================
    // Integration CRUD
    // ========================================================================

    /// Create a new integration.
    ///
    /// Creates an AI integration with the specified provider and configuration.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to integration client
    /// * `name` - Display name for the integration
    /// * `provider` - AI provider type (Claude, OpenAI, etc.)
    /// * `config` - Optional configuration (model, max_tokens, etc.)
    pub async fn create_integration(
        &self,
        client: &mut IntegrationClient,
        name: &str,
        provider: IntegrationType,
        config: Option<IntegrationConfig>,
    ) -> Result<Integration, AIError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        let response = client
            .create_integration(name, provider as i32, config)
            .await?;

        // Return integration from response
        response.integration.ok_or_else(|| {
            AIError::ConfigurationError("server did not return integration".to_string())
        })
    }

    /// Get an integration by ID.
    ///
    /// Retrieves the full integration details including configuration.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to integration client
    /// * `integration_id` - Integration to retrieve
    pub async fn get_integration(
        &self,
        client: &mut IntegrationClient,
        integration_id: &str,
    ) -> Result<Integration, AIError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        let response = client.get_integration(integration_id).await?;

        // Return integration from response
        response.integration.ok_or_else(|| {
            AIError::ConfigurationError("integration not found".to_string())
        })
    }

    /// Test an integration's connectivity.
    ///
    /// Verifies the integration can connect to its provider (API key valid, etc.).
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to integration client
    /// * `integration_id` - Integration to test
    ///
    /// # Returns
    ///
    /// A tuple of (success, message) where success indicates if the test passed.
    pub async fn test_integration(
        &self,
        client: &mut IntegrationClient,
        integration_id: &str,
    ) -> Result<(bool, String), AIError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        let response = client.test_integration(integration_id).await?;

        Ok((response.success, response.message))
    }

    /// Enable an integration.
    ///
    /// Activates an integration that was previously disabled.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to integration client
    /// * `integration_id` - Integration to enable
    pub async fn enable_integration(
        &self,
        client: &mut IntegrationClient,
        integration_id: &str,
    ) -> Result<Integration, AIError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        let response = client.enable_integration(integration_id).await?;

        // Return updated integration
        response.integration.ok_or_else(|| {
            AIError::ConfigurationError("server did not return integration".to_string())
        })
    }

    /// Disable an integration.
    ///
    /// Deactivates an integration without deleting it.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to integration client
    /// * `integration_id` - Integration to disable
    pub async fn disable_integration(
        &self,
        client: &mut IntegrationClient,
        integration_id: &str,
    ) -> Result<Integration, AIError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        let response = client.disable_integration(integration_id).await?;

        // Return updated integration
        response.integration.ok_or_else(|| {
            AIError::ConfigurationError("server did not return integration".to_string())
        })
    }

    // ========================================================================
    // Event Streaming
    // ========================================================================

    /// Subscribe to integration events.
    ///
    /// Returns a stream of integration events (AI streaming chunks, etc.)
    /// for real-time updates.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to integration client
    /// * `integration_id` - Optional integration ID to filter events.
    ///                      If None, receives events for all integrations.
    pub async fn subscribe_events(
        &self,
        client: &mut IntegrationClient,
        integration_id: Option<&str>,
    ) -> Result<impl Stream<Item = Result<IntegrationEvent, AIError>>, AIError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server
        let stream = client.stream_integration_events(integration_id).await?;

        // Map stream items to extract event and convert errors
        Ok(stream.map(|result| {
            result
                .map_err(AIError::Network)
                .and_then(|response| {
                    response
                        .event
                        .ok_or_else(|| AIError::ConfigurationError("empty event".to_string()))
                })
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ai_error_display() {
        let err = AIError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = AIError::InvalidModel;
        assert!(err.to_string().contains("invalid model"));

        let err = AIError::ConfigurationError("test error".to_string());
        assert!(err.to_string().contains("configuration error"));
        assert!(err.to_string().contains("test error"));

        let network_err = AIError::Network(NetworkError::Timeout);
        assert!(network_err.to_string().contains("network error"));
    }

    #[test]
    fn test_ai_error_from_network() {
        let network_err = NetworkError::Unauthenticated;
        let ai_err: AIError = network_err.into();
        assert!(matches!(ai_err, AIError::Network(_)));
    }

    #[test]
    fn test_ai_service_construction() {
        // Verify the AIService struct type signatures compile correctly.
        fn _verify_types(settings_repo: Arc<SettingsRepository>) {
            let _service = AIService::new(settings_repo);
        }
    }

    #[test]
    fn test_integration_type_ai_providers() {
        // Verify AI provider types exist with expected values
        assert_eq!(IntegrationType::Claude as i32, 10);
        assert_eq!(IntegrationType::Openai as i32, 11);
        assert_eq!(IntegrationType::GoogleAi as i32, 12);
        assert_eq!(IntegrationType::AzureOpenai as i32, 13);
        assert_eq!(IntegrationType::Cohere as i32, 14);
        assert_eq!(IntegrationType::Huggingface as i32, 15);
    }
}
