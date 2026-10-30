//! AI broker gRPC client.
//!
//! Talks to the server's `AIService`, which holds the organization API key and
//! meters token spend. Used by the cloud silo's broker backend when the user
//! is on managed tokens rather than their own API key.

use crate::network::grpc::transport::GrpcTransport;
use tonic::service::interceptor::InterceptedService;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    ChatTurn, CompleteRequest, CompleteResponse, GetQuotaRequest, GetQuotaResponse,
    ListServedModelsRequest, ListServedModelsResponse, ai_service_client::AiServiceClient,
};

/// AI broker client wrapper.
#[derive(Clone)]
pub struct AiClient {
    inner: AiServiceClient<InterceptedService<GrpcTransport, AuthInterceptor>>,
}

impl AiClient {
    /// Create a new AiClient from pool components.
    pub fn new(channel: GrpcTransport, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: AiServiceClient::with_interceptor(channel, interceptor),
        }
    }

    /// Run one metered completion on the server's organization key.
    ///
    /// The prompt is assembled on this device; the server never reads the
    /// conversation it came from.
    pub async fn complete(
        &self,
        model: &str,
        system: Option<&str>,
        turns: Vec<ChatTurn>,
        max_tokens: u32,
    ) -> Result<CompleteResponse, NetworkError> {
        let request = CompleteRequest {
            model: model.to_string(),
            messages: turns,
            max_tokens,
            system: system.unwrap_or_default().to_string(),
        };

        let response = self
            .inner
            .clone()
            .complete(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response.into_inner())
    }

    /// Remaining token balance and lifetime spend for this user.
    pub async fn get_quota(&self) -> Result<GetQuotaResponse, NetworkError> {
        let response = self
            .inner
            .clone()
            .get_quota(GetQuotaRequest {})
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response.into_inner())
    }

    /// Models the broker is willing to bill for.
    pub async fn list_served_models(&self) -> Result<ListServedModelsResponse, NetworkError> {
        let response = self
            .inner
            .clone()
            .list_served_models(ListServedModelsRequest {})
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response.into_inner())
    }
}
