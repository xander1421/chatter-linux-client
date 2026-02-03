//! Integration gRPC client.
//!
//! Client for AI integration service operations including CRUD operations,
//! webhook management, channel integrations, and event streaming.

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    integration_service_client::IntegrationServiceClient, AddIntegrationToChannelRequest,
    AddIntegrationToChannelResponse, CreateIntegrationRequest, CreateIntegrationResponse,
    DeleteIntegrationRequest, DeleteIntegrationResponse, DisableIntegrationRequest,
    DisableIntegrationResponse, EnableIntegrationRequest, EnableIntegrationResponse,
    GetChannelIntegrationsRequest, GetChannelIntegrationsResponse, GetIntegrationRequest,
    GetIntegrationResponse, HandleWebhookCallbackRequest, HandleWebhookCallbackResponse,
    Integration, IntegrationConfig, ListIntegrationsRequest, ListIntegrationsResponse,
    RegisterWebhookRequest, RegisterWebhookResponse, RemoveIntegrationFromChannelRequest,
    RemoveIntegrationFromChannelResponse, TestIntegrationRequest, TestIntegrationResponse,
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
    inner: IntegrationServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl IntegrationClient {
    /// Create a new IntegrationClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
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
}
