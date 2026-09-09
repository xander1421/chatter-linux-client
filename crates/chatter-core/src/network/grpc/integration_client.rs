//! Integration gRPC client.
//!
//! Client for AI integration service operations including CRUD operations,
//! webhook management, channel integrations, and event streaming.

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use crate::network::grpc::transport::GrpcTransport;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    integration_service_client::IntegrationServiceClient, AddIntegrationToChannelRequest,
    AddIntegrationToChannelResponse, CreateIntegrationRequest, CreateIntegrationResponse,
    DeleteIntegrationRequest, DeleteIntegrationResponse, DisableIntegrationRequest,
    DisableIntegrationResponse, EnableIntegrationRequest, EnableIntegrationResponse,
    GetChannelIntegrationsRequest, GetChannelIntegrationsResponse, GetIntegrationRequest,
    GetIntegrationResponse, GetModelLimitsRequest, GetModelLimitsResponse,
    HandleWebhookCallbackRequest, HandleWebhookCallbackResponse, Integration, IntegrationConfig,
    IntegrationType, ListAvailableModelsRequest, ListAvailableModelsResponse,
    ListIntegrationsRequest, ListIntegrationsResponse, RegisterWebhookRequest,
    RegisterWebhookResponse, RemoveIntegrationFromChannelRequest,
    RemoveIntegrationFromChannelResponse, StreamIntegrationEventsRequest,
    StreamIntegrationEventsResponse, TestIntegrationRequest, TestIntegrationResponse,
    UnregisterWebhookRequest, UnregisterWebhookResponse, UpdateIntegrationRequest,
    UpdateIntegrationResponse, WebhookConfig,
};

/// Integration service client wrapper.
///
/// Wraps the generated IntegrationServiceClient with ergonomic APIs and error mapping.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut integration = IntegrationClient::new(pool.channel(), pool.auth_clone());
/// let response = integration.create_integration("my-integration", IntegrationType::Claude as i32, None).await?;
/// ```
pub struct IntegrationClient {
    inner: IntegrationServiceClient<InterceptedService<GrpcTransport, AuthInterceptor>>,
}

impl IntegrationClient {
    /// Create a new IntegrationClient from pool components.
    pub fn new(channel: GrpcTransport, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: IntegrationServiceClient::with_interceptor(channel, interceptor),
        }
    }

    // ==================== Integration CRUD RPCs ====================

    /// Create a new integration.
    ///
    /// # Arguments
    ///
    /// * `name` - Integration name
    /// * `integration_type` - Integration type (from IntegrationType enum)
    /// * `config` - Optional integration configuration
    pub async fn create_integration(
        &mut self,
        name: &str,
        integration_type: i32,
        config: Option<IntegrationConfig>,
    ) -> Result<CreateIntegrationResponse, NetworkError> {
        let request = CreateIntegrationRequest {
            name: name.to_string(),
            r#type: integration_type,
            config,
            secrets: Default::default(),
            channel_ids: vec![],
        };

        self.inner
            .create_integration(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get an integration by ID.
    pub async fn get_integration(
        &mut self,
        integration_id: &str,
    ) -> Result<GetIntegrationResponse, NetworkError> {
        let request = GetIntegrationRequest {
            integration_id: integration_id.to_string(),
        };

        self.inner
            .get_integration(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List integrations with optional filters and pagination.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Optional channel ID filter
    /// * `page_token` - Optional pagination token
    /// * `page_size` - Optional page size
    pub async fn list_integrations(
        &mut self,
        channel_id: Option<&str>,
        page_token: Option<&str>,
        page_size: Option<i32>,
    ) -> Result<ListIntegrationsResponse, NetworkError> {
        let request = ListIntegrationsRequest {
            types: vec![],
            statuses: vec![],
            channel_ids: channel_id.map(|c| vec![c.to_string()]).unwrap_or_default(),
            page_size: page_size.unwrap_or(0) as u32,
            page_token: page_token.unwrap_or("").to_string(),
        };

        self.inner
            .list_integrations(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Update an integration.
    ///
    /// # Arguments
    ///
    /// * `integration_id` - Integration ID to update
    /// * `name` - Optional new name
    /// * `config` - Optional new configuration
    pub async fn update_integration(
        &mut self,
        integration_id: &str,
        name: Option<&str>,
        config: Option<IntegrationConfig>,
    ) -> Result<UpdateIntegrationResponse, NetworkError> {
        // Build integration with updated fields
        let integration = Integration {
            id: integration_id.to_string(),
            name: name.unwrap_or("").to_string(),
            r#type: 0,
            status: 0,
            config,
            secrets: Default::default(),
            webhook_config: None,
            capabilities: vec![],
            channel_ids: vec![],
            installed_by: String::new(),
            created_at: 0,
            updated_at: 0,
            last_active_at: 0,
            config_version: 0,
            config_history: vec![],
        };

        let request = UpdateIntegrationRequest {
            integration_id: integration_id.to_string(),
            integration: Some(integration),
            update_mask: None,
        };

        self.inner
            .update_integration(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Delete an integration.
    pub async fn delete_integration(
        &mut self,
        integration_id: &str,
    ) -> Result<DeleteIntegrationResponse, NetworkError> {
        let request = DeleteIntegrationRequest {
            integration_id: integration_id.to_string(),
        };

        self.inner
            .delete_integration(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Integration Management RPCs ====================

    /// Enable an integration.
    pub async fn enable_integration(
        &mut self,
        integration_id: &str,
    ) -> Result<EnableIntegrationResponse, NetworkError> {
        let request = EnableIntegrationRequest {
            integration_id: integration_id.to_string(),
        };

        self.inner
            .enable_integration(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Disable an integration.
    pub async fn disable_integration(
        &mut self,
        integration_id: &str,
    ) -> Result<DisableIntegrationResponse, NetworkError> {
        let request = DisableIntegrationRequest {
            integration_id: integration_id.to_string(),
            reason: String::new(),
        };

        self.inner
            .disable_integration(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Test an integration.
    pub async fn test_integration(
        &mut self,
        integration_id: &str,
    ) -> Result<TestIntegrationResponse, NetworkError> {
        let request = TestIntegrationRequest {
            integration_id: integration_id.to_string(),
            test_payload: None,
        };

        self.inner
            .test_integration(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Webhook RPCs ====================

    /// Register a webhook for an integration.
    ///
    /// # Arguments
    ///
    /// * `integration_id` - Integration to register webhook for
    /// * `config` - Webhook configuration
    pub async fn register_webhook(
        &mut self,
        integration_id: &str,
        config: WebhookConfig,
    ) -> Result<RegisterWebhookResponse, NetworkError> {
        let request = RegisterWebhookRequest {
            integration_id: integration_id.to_string(),
            config: Some(config),
        };

        self.inner
            .register_webhook(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Unregister a webhook.
    ///
    /// # Arguments
    ///
    /// * `integration_id` - Integration the webhook belongs to
    /// * `webhook_id` - Webhook ID to unregister
    pub async fn unregister_webhook(
        &mut self,
        integration_id: &str,
        webhook_id: &str,
    ) -> Result<UnregisterWebhookResponse, NetworkError> {
        let request = UnregisterWebhookRequest {
            integration_id: integration_id.to_string(),
            webhook_id: webhook_id.to_string(),
        };

        self.inner
            .unregister_webhook(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Handle an incoming webhook callback.
    ///
    /// # Arguments
    ///
    /// * `webhook_id` - Webhook ID
    /// * `headers` - HTTP headers from the callback
    /// * `body` - Raw body bytes
    pub async fn handle_webhook_callback(
        &mut self,
        webhook_id: &str,
        headers: std::collections::HashMap<String, String>,
        body: Vec<u8>,
    ) -> Result<HandleWebhookCallbackResponse, NetworkError> {
        let request = HandleWebhookCallbackRequest {
            webhook_id: webhook_id.to_string(),
            headers,
            body,
        };

        self.inner
            .handle_webhook_callback(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Channel Integration RPCs ====================

    /// Add an integration to a channel.
    ///
    /// # Arguments
    ///
    /// * `integration_id` - Integration to add
    /// * `channel_id` - Channel to add integration to
    pub async fn add_integration_to_channel(
        &mut self,
        integration_id: &str,
        channel_id: &str,
    ) -> Result<AddIntegrationToChannelResponse, NetworkError> {
        let request = AddIntegrationToChannelRequest {
            integration_id: integration_id.to_string(),
            channel_id: channel_id.to_string(),
        };

        self.inner
            .add_integration_to_channel(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Remove an integration from a channel.
    ///
    /// # Arguments
    ///
    /// * `integration_id` - Integration to remove
    /// * `channel_id` - Channel to remove integration from
    pub async fn remove_integration_from_channel(
        &mut self,
        integration_id: &str,
        channel_id: &str,
    ) -> Result<RemoveIntegrationFromChannelResponse, NetworkError> {
        let request = RemoveIntegrationFromChannelRequest {
            integration_id: integration_id.to_string(),
            channel_id: channel_id.to_string(),
        };

        self.inner
            .remove_integration_from_channel(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Get all integrations for a channel.
    pub async fn get_channel_integrations(
        &mut self,
        channel_id: &str,
    ) -> Result<GetChannelIntegrationsResponse, NetworkError> {
        let request = GetChannelIntegrationsRequest {
            channel_id: channel_id.to_string(),
        };

        self.inner
            .get_channel_integrations(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== AI Model RPCs ====================

    /// Get token/parameter limits for a specific AI model.
    ///
    /// # Arguments
    ///
    /// * `integration_type` - The type of integration (e.g., IntegrationType::Claude)
    /// * `model` - The model ID (e.g., "claude-3-opus")
    pub async fn get_model_limits(
        &mut self,
        integration_type: IntegrationType,
        model: &str,
    ) -> Result<GetModelLimitsResponse, NetworkError> {
        let request = GetModelLimitsRequest {
            integration_type: integration_type as i32,
            model: model.to_string(),
        };

        self.inner
            .get_model_limits(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List available AI models for an integration type.
    ///
    /// # Arguments
    ///
    /// * `integration_type` - The type of integration (e.g., IntegrationType::Claude)
    /// * `api_key` - Optional API key (uses existing integration secret if not provided)
    /// * `integration_id` - Optional integration ID to use for fetching API key
    pub async fn list_available_models(
        &mut self,
        integration_type: IntegrationType,
        api_key: Option<&str>,
        integration_id: Option<&str>,
    ) -> Result<ListAvailableModelsResponse, NetworkError> {
        let request = ListAvailableModelsRequest {
            integration_type: integration_type as i32,
            api_key: api_key.unwrap_or("").to_string(),
            integration_id: integration_id.unwrap_or("").to_string(),
        };

        self.inner
            .list_available_models(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Event Streaming ====================

    /// Stream integration events for integrations this user owns.
    ///
    /// `integration_ids` MUST name at least one integration. The empty list
    /// used to mean "every integration on the server" and the server now
    /// rejects it with `InvalidArgument` (integration/handlers/handlers.go:
    /// StreamIntegrationEvents) — it was a cross-tenant leak, not a wildcard.
    /// This wrapper used to send `unwrap_or_default()` and document the old
    /// semantics, so the "stream everything" call compiled and failed at the
    /// server (INFRA-38); it is now rejected here, with the reason.
    pub async fn stream_integration_events(
        &mut self,
        integration_ids: &[String],
    ) -> Result<impl Stream<Item = Result<StreamIntegrationEventsResponse, NetworkError>>, NetworkError>
    {
        if integration_ids.is_empty() {
            return Err(NetworkError::InvalidRequest(
                "stream_integration_events requires at least one integration id; \
                 the server rejects an empty list (it is not a wildcard)"
                    .to_string(),
            ));
        }
        let request = StreamIntegrationEventsRequest {
            integration_ids: integration_ids.to_vec(),
            event_types: vec![],
            since: 0,
        };

        let response = self
            .inner
            .stream_integration_events(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Full integration tests require a running server.
    // These tests verify compilation and basic structure.

    #[test]
    fn test_integration_client_construction_types() {
        // This test verifies the type signatures compile correctly.
        fn _verify_types(channel: GrpcTransport, interceptor: AuthInterceptor) {
            let _client = IntegrationClient::new(channel, interceptor);
        }
    }

    #[test]
    fn test_create_integration_request_construction() {
        let request = CreateIntegrationRequest {
            name: "test-integration".to_string(),
            r#type: IntegrationType::Claude as i32,
            config: None,
            secrets: Default::default(),
            channel_ids: vec![],
        };

        assert_eq!(request.name, "test-integration");
        assert_eq!(request.r#type, IntegrationType::Claude as i32);
    }

    #[test]
    fn test_register_webhook_request_construction() {
        use crate::proto::chat_v1::WebhookEvent;

        let events = vec![
            WebhookEvent {
                r#type: 1, // Example event type
                filters: None,
            },
            WebhookEvent {
                r#type: 2,
                filters: None,
            },
        ];

        let config = WebhookConfig {
            url: "https://example.com/webhook".to_string(),
            secret: "secret123".to_string(),
            events,
            headers: Default::default(),
            timeout_seconds: 30,
            retry_count: 3,
        };

        let request = RegisterWebhookRequest {
            integration_id: "integration-123".to_string(),
            config: Some(config),
        };

        assert_eq!(request.integration_id, "integration-123");
        assert!(request.config.is_some());
        let config = request.config.unwrap();
        assert_eq!(config.url, "https://example.com/webhook");
        assert_eq!(config.events.len(), 2);
    }

    #[test]
    fn test_add_integration_to_channel_request_construction() {
        let request = AddIntegrationToChannelRequest {
            integration_id: "integration-456".to_string(),
            channel_id: "channel-789".to_string(),
        };

        assert_eq!(request.integration_id, "integration-456");
        assert_eq!(request.channel_id, "channel-789");
    }

    #[test]
    fn test_get_model_limits_request_construction() {
        let request = GetModelLimitsRequest {
            integration_type: IntegrationType::Openai as i32,
            model: "gpt-4".to_string(),
        };

        assert_eq!(request.integration_type, IntegrationType::Openai as i32);
        assert_eq!(request.model, "gpt-4");
    }

    #[test]
    fn test_integration_type_values() {
        // Verify IntegrationType enum has expected values
        assert_eq!(IntegrationType::Unspecified as i32, 0);
        assert_eq!(IntegrationType::Webhook as i32, 1);
        assert_eq!(IntegrationType::Claude as i32, 10);
        assert_eq!(IntegrationType::Openai as i32, 11);
        assert_eq!(IntegrationType::GoogleAi as i32, 12);
    }

    #[test]
    fn test_integration_config_construction() {
        let config = IntegrationConfig {
            provider_type: "ai".to_string(),
            provider_name: "Anthropic".to_string(),
            initialized_at: "2024-01-01".to_string(),
            model: "claude-3-opus".to_string(),
            max_tokens: 4096,
            temperature: 0.7,
            system_prompt: "You are a helpful assistant.".to_string(),
            auto_respond: true,
            bot_name: "Claude Bot".to_string(),
            supported_models: vec!["claude-3-opus".to_string(), "claude-3-sonnet".to_string()],
            include_history: true,
            history_limit: 10,
            respond_to_all: false,
            webhook_url: String::new(),
            timeout_seconds: 30,
            retry_enabled: true,
            retry_count: 3,
            client_id: String::new(),
            redirect_uri: String::new(),
            scopes: vec![],
            extra_config: Default::default(),
        };

        assert_eq!(config.model, "claude-3-opus");
        assert_eq!(config.max_tokens, 4096);
        assert!((config.temperature - 0.7).abs() < f64::EPSILON);
        assert!(config.auto_respond);
        assert_eq!(config.supported_models.len(), 2);
    }
}
