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
    friend_requests_filter, user_service_client::UserServiceClient, FriendRequest,
    FriendRequestsFilter, FriendResponse, FriendStatus, GetUserProfileRequest,
    ListFriendRequestsRequest, ListFriendRequestsResponse, ListFriendsRequest, ListFriendsResponse,
    PresenceUpdate, RemoveFriendRequest, StreamPresenceRequest, UpdatePresenceRequest,
    UpdateUserProfileRequest, UserEvent, UserPresence, UserProfile, UserSearchRequest,
    UserSearchResponse,
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

    // ==================== Presence RPCs ====================

    /// Update current user's presence status.
    pub async fn update_presence(&mut self, presence: UserPresence) -> Result<(), NetworkError> {
        let request = UpdatePresenceRequest {
            presence: presence.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .update_presence(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Stream presence updates for all friends.
    ///
    /// Returns a long-lived stream of presence changes.
    pub async fn stream_presence(
        &mut self,
    ) -> Result<impl Stream<Item = Result<PresenceUpdate, NetworkError>>, NetworkError> {
        let request = StreamPresenceRequest {};

        let response = self
            .inner
            .stream_presence(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    // ==================== Friend Management RPCs ====================

    /// Send a friend request to a user (streaming response).
    pub async fn send_friend_request(
        &mut self,
        user_id: &str,
        message: Option<&str>,
    ) -> Result<impl Stream<Item = Result<UserEvent, NetworkError>>, NetworkError> {
        let request = FriendRequest {
            user_id: user_id.to_string(),
            message: message.unwrap_or("").to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .send_friend_request(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Respond to a friend request (streaming response).
    pub async fn respond_friend_request(
        &mut self,
        user_id: &str,
        status: FriendStatus,
        message: Option<&str>,
    ) -> Result<impl Stream<Item = Result<UserEvent, NetworkError>>, NetworkError> {
        let request = FriendResponse {
            user_id: user_id.to_string(),
            status: status.into(),
            message: message.unwrap_or("").to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .respond_friend_request(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Remove a friend.
    pub async fn remove_friend(&mut self, user_id: &str) -> Result<(), NetworkError> {
        let request = RemoveFriendRequest {
            user_id: user_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .remove_friend(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List friends with pagination.
    pub async fn list_friends(
        &mut self,
        limit: i32,
        offset: i32,
    ) -> Result<ListFriendsResponse, NetworkError> {
        let request = ListFriendsRequest { limit, offset };

        self.inner
            .list_friends(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// List pending friend requests.
    ///
    /// # Arguments
    ///
    /// * `filter` - Filter type (All, Pending, Accepted, Sent, Received)
    /// * `limit` - Max results to return
    /// * `offset` - Pagination offset
    pub async fn list_friend_requests(
        &mut self,
        filter: friend_requests_filter::Filter,
        limit: i32,
        offset: i32,
    ) -> Result<ListFriendRequestsResponse, NetworkError> {
        let request = ListFriendRequestsRequest {
            filter: Some(FriendRequestsFilter {
                filter: filter.into(),
                limit,
                offset,
            }),
        };

        self.inner
            .list_friend_requests(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }
}
