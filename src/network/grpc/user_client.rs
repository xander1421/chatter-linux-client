//! User gRPC client.
//!
//! Client for user service operations including profile management,
//! presence, friends, blocking, and channel navigation.

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    user_service_client::UserServiceClient, GetUserProfileRequest, UpdateUserProfileRequest,
    UserEvent, UserProfile, UserSearchRequest, UserSearchResponse,
};

/// User service client wrapper.
///
/// Wraps the generated UserServiceClient with ergonomic APIs and error mapping.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut user = UserClient::new(pool.channel(), pool.auth_clone());
/// let profile = user.get_user_profile("user-123").await?;
/// ```
pub struct UserClient {
    inner: UserServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl UserClient {
    /// Create a new UserClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: UserServiceClient::with_interceptor(channel, interceptor),
        }
    }

    // ==================== Profile RPCs ====================

    /// Get a user's profile by ID.
    pub async fn get_user_profile(&mut self, user_id: &str) -> Result<UserProfile, NetworkError> {
        let request = GetUserProfileRequest {
            user_id: user_id.to_string(),
        };

        self.inner
            .get_user_profile(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Update current user's profile (streaming response).
    ///
    /// Returns stream of UserEvent with update progress.
    /// Uses FieldMask to specify which fields to update.
    pub async fn update_user_profile(
        &mut self,
        profile: UserProfile,
        update_mask: Option<prost_types::FieldMask>,
    ) -> Result<impl Stream<Item = Result<UserEvent, NetworkError>>, NetworkError> {
        let request = UpdateUserProfileRequest {
            profile: Some(profile),
            update_mask,
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .update_user_profile(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Search for users by query.
    pub async fn search_users(
        &mut self,
        query: &str,
        limit: i32,
        offset: i32,
    ) -> Result<UserSearchResponse, NetworkError> {
        let request = UserSearchRequest {
            query: query.to_string(),
            limit,
            offset,
        };

        self.inner
            .search_users(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }
}
