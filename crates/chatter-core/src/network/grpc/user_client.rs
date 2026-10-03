//! User gRPC client.
//!
//! Client for user service operations including profile management,
//! presence, friends, and blocking.

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use crate::network::grpc::transport::GrpcTransport;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    friend_requests_filter, user_service_client::UserServiceClient, BlockUserRequest,
    FriendRequest, FriendRequestsFilter, FriendResponse, FriendStatus,
    DeleteUsernameLinkRequest, GetUserProfileRequest, GetUsernameDiscoverabilityRequest,
    GetUsernameDiscoverabilityResponse, GetUsernameLinkRequest,
    GetUsernameLinkResponse, ListBlockedUsersRequest, ListBlockedUsersResponse,
    ListFriendRequestsRequest, ListFriendRequestsResponse, ListFriendsRequest, ListFriendsResponse,
    LookupUsernameRequest, LookupUsernameResponse,
    RemoveFriendRequest, SetUsernameDiscoverabilityRequest,
    SetUsernameDiscoverabilityResponse, SetUsernameLinkRequest,
    SetUsernameLinkResponse,
    UnblockUserRequest,
    UpdatePresenceRequest, UpdateUserProfileRequest, UserEvent, UserEventRequest, UserPresence,
    UserProfile,
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
    inner: UserServiceClient<InterceptedService<GrpcTransport, AuthInterceptor>>,
}

impl UserClient {
    /// Create a new UserClient from pool components.
    pub fn new(channel: GrpcTransport, interceptor: AuthInterceptor) -> Self {
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

    /// Signal-style exact username lookup by 32-byte discovery hash. Returns
    /// `Ok(None)` when the server has no account registered under that hash.
    pub async fn lookup_username(
        &mut self,
        username_hash: [u8; 32],
    ) -> Result<Option<String>, NetworkError> {
        let request = LookupUsernameRequest {
            username_hash: username_hash.to_vec(),
        };
        let LookupUsernameResponse { user_id } = self
            .inner
            .lookup_username(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;
        if user_id.is_empty() {
            Ok(None)
        } else {
            Ok(Some(user_id))
        }
    }

    /// Read the caller's "Who can find me by my username" setting from the
    /// server (user_profiles.username_discoverable). `true` = Everyone,
    /// `false` = Nobody.
    pub async fn get_username_discoverability(&mut self) -> Result<bool, NetworkError> {
        let GetUsernameDiscoverabilityResponse { discoverable } = self
            .inner
            .get_username_discoverability(GetUsernameDiscoverabilityRequest {})
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;
        Ok(discoverable)
    }

    /// Set the caller's "Who can find me by my username" setting on the
    /// server. With `false` the server's LookupUsername answers "no match"
    /// for this account to everyone else. Returns the persisted value.
    pub async fn set_username_discoverability(
        &mut self,
        discoverable: bool,
    ) -> Result<bool, NetworkError> {
        let SetUsernameDiscoverabilityResponse { discoverable } = self
            .inner
            .set_username_discoverability(SetUsernameDiscoverabilityRequest { discoverable })
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;
        Ok(discoverable)
    }

    // ==================== Username-link RPCs ====================

    /// Upload (or rotate) the caller's username-link ciphertext blob. Returns
    /// the server-generated opaque link id. The decryption entropy stays
    /// client-side (URL fragment) — the server only ever sees ciphertext.
    pub async fn set_username_link(
        &mut self,
        encrypted_username: Vec<u8>,
    ) -> Result<String, NetworkError> {
        let request = SetUsernameLinkRequest { encrypted_username };
        let SetUsernameLinkResponse { link_id } = self
            .inner
            .set_username_link(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))?;
        Ok(link_id)
    }

    /// Fetch the ciphertext blob stored under a link id, with the link
    /// owner's user id. `Ok(None)` when the link doesn't exist (deleted or
    /// rotated away).
    pub async fn get_username_link(
        &mut self,
        link_id: &str,
    ) -> Result<Option<(String, Vec<u8>)>, NetworkError> {
        let request = GetUsernameLinkRequest {
            link_id: link_id.to_string(),
        };
        match self.inner.get_username_link(request).await {
            Ok(r) => {
                let GetUsernameLinkResponse { encrypted_username, user_id } = r.into_inner();
                Ok(Some((user_id, encrypted_username)))
            }
            Err(s) if s.code() == tonic::Code::NotFound => Ok(None),
            Err(s) => Err(NetworkError::from_status(&s)),
        }
    }

    /// Delete the caller's username link. Idempotent.
    pub async fn delete_username_link(&mut self) -> Result<(), NetworkError> {
        let request = DeleteUsernameLinkRequest {};
        self.inner
            .delete_username_link(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;
        Ok(())
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

    // ==================== Friend Management RPCs ====================

    /// Send a friend request to a user (streaming response).
    pub async fn send_friend_request(
        &mut self,
        user_id: &str,
        encrypted_message: Option<&[u8]>,
    ) -> Result<impl Stream<Item = Result<UserEvent, NetworkError>>, NetworkError> {
        let request = FriendRequest {
            user_id: user_id.to_string(),
            encrypted_message: encrypted_message.unwrap_or(&[]).to_vec(),
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

    // ==================== Blocking RPCs ====================

    /// Block a user (streaming response).
    pub async fn block_user(
        &mut self,
        user_id: &str,
    ) -> Result<impl Stream<Item = Result<UserEvent, NetworkError>>, NetworkError> {
        let request = BlockUserRequest {
            user_id: user_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .block_user(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Unblock a user (streaming response).
    pub async fn unblock_user(
        &mut self,
        user_id: &str,
    ) -> Result<impl Stream<Item = Result<UserEvent, NetworkError>>, NetworkError> {
        let request = UnblockUserRequest {
            user_id: user_id.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        let response = self
            .inner
            .unblock_user(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// List blocked users with pagination.
    pub async fn list_blocked_users(
        &mut self,
        limit: i32,
        offset: i32,
    ) -> Result<ListBlockedUsersResponse, NetworkError> {
        let request = ListBlockedUsersRequest { limit, offset };

        self.inner
            .list_blocked_users(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    // ==================== Event Streaming ====================

    /// Stream all user events (friends, blocks, profile changes, etc).
    ///
    /// Long-lived stream for real-time updates.
    ///
    /// # Arguments
    ///
    /// * `user_id` - User ID to stream events for (typically current user)
    pub async fn stream_user_events(
        &mut self,
        user_id: &str,
    ) -> Result<impl Stream<Item = Result<UserEvent, NetworkError>>, NetworkError> {
        let request = UserEventRequest {
            user_id: user_id.to_string(),
        };

        let response = self
            .inner
            .stream_user_events(request)
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
    fn test_user_client_construction_types() {
        // This test verifies the type signatures compile correctly.
        // We can't actually create a client without a real channel,
        // but we verify the wrapper pattern is correct.
        fn _verify_types(channel: GrpcTransport, interceptor: AuthInterceptor) {
            let _client = UserClient::new(channel, interceptor);
        }
    }

    #[test]
    fn test_presence_status_values() {
        // Verify UserPresence enum is accessible and has expected values
        assert_eq!(UserPresence::PresenceUnspecified as i32, 0);
        assert_eq!(UserPresence::Offline as i32, 1);
        assert_eq!(UserPresence::Online as i32, 2);
        assert_eq!(UserPresence::Away as i32, 3);
        assert_eq!(UserPresence::DoNotDisturb as i32, 4);
    }

    #[test]
    fn test_friend_status_values() {
        // Verify FriendStatus enum is accessible
        assert_eq!(FriendStatus::Unspecified as i32, 0);
        assert_eq!(FriendStatus::None as i32, 1);
        assert_eq!(FriendStatus::Pending as i32, 2);
        assert_eq!(FriendStatus::Accepted as i32, 3);
        assert_eq!(FriendStatus::Declined as i32, 4);
        assert_eq!(FriendStatus::Blocked as i32, 5);
    }

    #[test]
    fn test_friend_requests_filter_values() {
        // Verify filter enum is accessible
        use friend_requests_filter::Filter;
        assert_eq!(Filter::All as i32, 0);
        assert_eq!(Filter::Pending as i32, 1);
        assert_eq!(Filter::Accepted as i32, 2);
        assert_eq!(Filter::Sent as i32, 3);
        assert_eq!(Filter::Received as i32, 4);
    }

    #[test]
    fn test_user_profile_construction() {
        // Verify UserProfile can be constructed
        let profile = UserProfile {
            user_id: "user-123".to_string(),
            display_name: "Test User".to_string(),
            status_message: "Hello!".to_string(),
            presence: UserPresence::Online as i32,
            encrypted_name: Vec::new(),
            encrypted_about: Vec::new(),
            encrypted_avatar: Vec::new(),
        };

        assert_eq!(profile.user_id, "user-123");
        assert_eq!(profile.display_name, "Test User");
        assert_eq!(profile.presence, UserPresence::Online as i32);
    }

    #[test]
    fn test_friend_request_construction() {
        // Verify FriendRequest can be constructed
        let request = FriendRequest {
            user_id: "friend-456".to_string(),
            encrypted_message: "Let's be friends!".as_bytes().to_vec(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        assert_eq!(request.user_id, "friend-456");
        assert!(!request.request_id.is_empty());
    }
}
