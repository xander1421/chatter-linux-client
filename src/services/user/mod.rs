//! User service.
//!
//! Profile, friends, blocking, and presence management.
//!
//! This module is organized into:
//! - Service struct and core methods (this file)
//! - `conversions` - Proto <-> Domain conversion helpers
//! - `tests` - Unit tests

use std::sync::Arc;

use tokio_stream::StreamExt;

use chrono::Utc;

use crate::domain::ids::UserId;
use crate::domain::user::{Friendship, FriendshipStatus, Profile, StatusMessage, User};
use crate::error::{CryptoError, NetworkError, StorageError};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::user_client::UserClient;
use crate::proto::chat_v1::{friend_requests_filter, user_event, FriendStatus, UserProfile};
use crate::services::current_user::{CurrentUserError, CurrentUserProvider};
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
use crate::storage::repositories::user_repo::UserRepository;

mod conversions;
#[cfg(test)]
mod tests;

use conversions::{domain_presence_to_proto, proto_friend_status_to_domain, proto_to_domain_profile};

/// Filter for friend request queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FriendRequestFilter {
    /// All friend requests.
    All,
    /// Pending requests only.
    Pending,
    /// Accepted requests only.
    Accepted,
    /// Requests sent by current user.
    Sent,
    /// Requests received by current user.
    Received,
}

/// Result of responding to a friend request.
#[derive(Clone, Debug)]
pub struct FriendResponseResult {
    /// The friendship if accepted, None if declined.
    pub friendship: Option<Friendship>,
    /// The DM channel ID created when accepting a friend request.
    pub dm_channel_id: Option<String>,
}

impl FriendRequestFilter {
    /// Convert to proto filter enum value.
    pub fn to_proto(self) -> friend_requests_filter::Filter {
        match self {
            FriendRequestFilter::All => friend_requests_filter::Filter::All,
            FriendRequestFilter::Pending => friend_requests_filter::Filter::Pending,
            FriendRequestFilter::Accepted => friend_requests_filter::Filter::Accepted,
            FriendRequestFilter::Sent => friend_requests_filter::Filter::Sent,
            FriendRequestFilter::Received => friend_requests_filter::Filter::Received,
        }
    }
}

/// User service errors.
#[derive(Debug, thiserror::Error)]
pub enum UserError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Target user does not exist.
    #[error("user not found: {0}")]
    UserNotFound(UserId),

    /// User is not authorized for operation.
    #[error("not authorized")]
    NotAuthorized,

    /// Input validation failed.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Network operation failed.
    #[error("network error: {0}")]
    Network(#[from] NetworkError),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// Cryptographic operation failed.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
}

impl From<CurrentUserError> for UserError {
    fn from(err: CurrentUserError) -> Self {
        match err {
            CurrentUserError::Storage(e) => UserError::Storage(e),
            CurrentUserError::NotLoggedIn => UserError::NotLoggedIn,
        }
    }
}

/// User service for all user operations.
///
/// Orchestrates profile management, friends, blocking, and presence by
/// coordinating network client and local storage.
#[derive(Clone)]
pub struct UserService {
    grpc_pool: Arc<GrpcClientPool>,
    settings_repo: Arc<SettingsRepository>,
    user_repo: Arc<UserRepository>,
}

impl UserService {
    /// Create a new UserService with dependencies.
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        settings_repo: Arc<SettingsRepository>,
        user_repo: Arc<UserRepository>,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            user_repo,
        }
    }

    /// Create a user client for network operations.
    fn user_client(&self) -> UserClient {
        UserClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone())
    }

    // ========================================================================
    // Profile Operations
    // ========================================================================

    /// Get a user's profile by ID.
    ///
    /// Fetches from server and upserts to local storage.
    pub async fn get_user_profile(&self, user_id: &UserId) -> Result<Profile, UserError> {
        let _ = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let proto_profile = user_client
            .get_user_profile(&user_id.to_string())
            .await?;

        let profile = proto_to_domain_profile(&proto_profile)?;

        self.user_repo.ensure_user_exists(user_id)?;
        self.user_repo.upsert_profile(&profile)?;

        Ok(profile)
    }

    /// Update the current user's profile.
    ///
    /// # Security Note
    ///
    /// If display_name is provided, it is encrypted with the profile_key before sending.
    /// Server stores only ciphertext - it cannot read profile data.
    pub async fn update_user_profile(
        &self,
        display_name: Option<&str>,
        avatar_url: Option<&str>,
        status_message: Option<&StatusMessage>,
    ) -> Result<Profile, UserError> {
        use base64::Engine;

        let current_user = self.settings_repo.current_user_id()?;

        let existing = self
            .user_repo
            .get_profile(&current_user)?
            .ok_or_else(|| UserError::UserNotFound(current_user))?;

        // SECURITY: Encrypt display_name with profile_key if being updated
        // Server never sees plaintext profile data - only the encrypted blob
        let encrypted_display_name = if let Some(name) = display_name {
            let profile_key_bytes = self
                .settings_repo
                .get_bytes(keys::PROFILE_KEY)?
                .ok_or(UserError::NotLoggedIn)?;
            let profile_key: [u8; 32] = profile_key_bytes
                .try_into()
                .map_err(|_| UserError::InvalidInput("corrupt profile_key".to_string()))?;

            let encrypted = crate::crypto::profile_crypto::encrypt_display_name(&profile_key, name)?;
            Some(base64::engine::general_purpose::STANDARD.encode(encrypted))
        } else {
            None
        };

        let proto_profile = UserProfile {
            user_id: current_user.to_string(),
            display_name: encrypted_display_name
                .unwrap_or_else(|| existing.display_name.clone()),
            avatar_url: avatar_url
                .map(|s| s.to_string())
                .or_else(|| existing.avatar_url.clone())
                .unwrap_or_default(),
            status_message: status_message
                .map(|s| s.text.clone())
                .or_else(|| existing.status_message.as_ref().map(|s| s.text.clone()))
                .unwrap_or_default(),
            presence: domain_presence_to_proto(existing.presence),
        };

        let mut paths = Vec::new();
        if display_name.is_some() {
            paths.push("display_name".to_string());
        }
        if avatar_url.is_some() {
            paths.push("avatar_url".to_string());
        }
        if status_message.is_some() {
            paths.push("status_message".to_string());
        }

        let update_mask = if paths.is_empty() {
            None
        } else {
            Some(prost_types::FieldMask { paths })
        };

        let mut user_client = self.user_client();
        let mut stream = user_client
            .update_user_profile(proto_profile, update_mask)
            .await?;

        let mut profile_opt: Option<Profile> = None;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::ProfileUpdated(proto_profile)) = event.event {
                profile_opt = Some(proto_to_domain_profile(&proto_profile)?);
                break;
            }
        }

        let profile = profile_opt.ok_or_else(|| {
            UserError::InvalidInput("no profile updated event received".to_string())
        })?;

        self.user_repo.upsert_profile(&profile)?;

        Ok(profile)
    }

    // ========================================================================
    // Friend Management
    // ========================================================================

    /// Send a friend request to another user.
    pub async fn send_friend_request(
        &self,
        friend_id: &UserId,
        message: Option<&str>,
    ) -> Result<Friendship, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let friend_id_str = friend_id.to_string();
        let mut stream = user_client
            .send_friend_request(&friend_id_str, message)
            .await?;

        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::FriendStatusChanged(ref _status_response)) = event.event
            {
                break;
            }
        }

        let friendship = Friendship {
            user_id: current_user,
            friend_id: *friend_id,
            status: FriendshipStatus::Pending,
            created_at: Utc::now(),
        };

        let _ = self.user_repo.insert_friendship(&friendship);

        Ok(friendship)
    }

    /// Respond to a friend request.
    pub async fn respond_friend_request(
        &self,
        friend_id: &UserId,
        accept: bool,
    ) -> Result<FriendResponseResult, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let status = if accept {
            FriendStatus::Accepted
        } else {
            FriendStatus::Declined
        };

        let mut user_client = self.user_client();
        let friend_id_str = friend_id.to_string();
        let mut stream = user_client
            .respond_friend_request(&friend_id_str, status, None)
            .await?;

        let mut dm_channel_id = None;
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::FriendStatusChanged(ref status_response)) = event.event
            {
                let status = status_response.status();
                tracing::debug!(
                    "Friend response event: status={:?}, channel_id={}",
                    status,
                    status_response.channel_id
                );

                if !status_response.channel_id.is_empty() {
                    dm_channel_id = Some(status_response.channel_id.clone());
                    tracing::info!(
                        "Friend response returned DM channel ID: {}",
                        status_response.channel_id
                    );
                }

                if status != FriendStatus::Pending {
                    tracing::info!("Friend response final status: {:?}", status);
                    break;
                }
            }
        }

        if accept {
            let friendship = Friendship {
                user_id: current_user,
                friend_id: *friend_id,
                status: FriendshipStatus::Accepted,
                created_at: Utc::now(),
            };

            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            Ok(FriendResponseResult {
                friendship: Some(friendship),
                dm_channel_id,
            })
        } else {
            let _ = self.user_repo.delete_friendship(&current_user, friend_id);
            Ok(FriendResponseResult {
                friendship: None,
                dm_channel_id: None,
            })
        }
    }

    /// List accepted friends.
    pub async fn list_friends(&self, limit: u32, offset: u32) -> Result<Vec<Friendship>, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let response = user_client
            .list_friends(limit as i32, offset as i32)
            .await?;

        let mut friendships = Vec::with_capacity(response.friends.len());
        for friend in &response.friends {
            let friend_id: UserId = friend
                .user_id
                .parse()
                .map_err(|_| UserError::InvalidInput("invalid friend user ID".to_string()))?;

            let friendship = Friendship {
                user_id: current_user,
                friend_id,
                status: FriendshipStatus::Accepted,
                created_at: Utc::now(),
            };

            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            friendships.push(friendship);
        }

        Ok(friendships)
    }

    /// List friend requests with optional filtering.
    pub async fn list_friend_requests(
        &self,
        filter: FriendRequestFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Friendship>, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let proto_filter = filter.to_proto();

        let mut user_client = self.user_client();
        let response = user_client
            .list_friend_requests(proto_filter, limit as i32, offset as i32)
            .await?;

        let mut friendships = Vec::with_capacity(response.requests.len());
        for request in &response.requests {
            let friend_id: UserId = request
                .user_id
                .parse()
                .map_err(|_| UserError::InvalidInput("invalid request user ID".to_string()))?;

            let status = proto_friend_status_to_domain(request.status);

            let friendship = Friendship {
                user_id: current_user,
                friend_id,
                status,
                created_at: Utc::now(),
            };

            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            friendships.push(friendship);
        }

        Ok(friendships)
    }

    /// Get friends from local storage only.
    pub fn get_friends_local(&self) -> Result<Vec<Friendship>, UserError> {
        let current_user = self.settings_repo.current_user_id()?;
        self.user_repo
            .list_friends(&current_user)
            .map_err(UserError::Storage)
    }

    /// Remove a friend.
    pub async fn remove_friend(&self, friend_id: &UserId) -> Result<(), UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        user_client.remove_friend(&friend_id.to_string()).await?;

        let _ = self.user_repo.delete_friendship(&current_user, friend_id);

        Ok(())
    }

    // ========================================================================
    // Blocking Operations
    // ========================================================================

    /// Block a user.
    pub async fn block_user(&self, user_id: &UserId) -> Result<(), UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let user_id_str = user_id.to_string();
        let mut stream = user_client.block_user(&user_id_str).await?;

        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::FriendStatusChanged(ref _status)) = event.event {
                break;
            }
        }

        let friendship = Friendship {
            user_id: current_user,
            friend_id: *user_id,
            status: FriendshipStatus::Blocked,
            created_at: Utc::now(),
        };

        if self.user_repo.update_friendship(&friendship).is_err() {
            let _ = self.user_repo.insert_friendship(&friendship);
        }

        Ok(())
    }

    /// Unblock a user.
    pub async fn unblock_user(&self, user_id: &UserId) -> Result<(), UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let user_id_str = user_id.to_string();
        let mut stream = user_client.unblock_user(&user_id_str).await?;

        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::FriendStatusChanged(ref _status)) = event.event {
                break;
            }
        }

        let _ = self.user_repo.delete_friendship(&current_user, user_id);

        Ok(())
    }

    /// List blocked users.
    pub async fn list_blocked_users(&self, limit: u32, offset: u32) -> Result<Vec<UserId>, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let response = user_client
            .list_blocked_users(limit as i32, offset as i32)
            .await?;

        let mut blocked_ids = Vec::with_capacity(response.blocked_users.len());
        for proto_user in &response.blocked_users {
            let user_id: UserId = proto_user
                .id
                .parse()
                .map_err(|_| UserError::InvalidInput("invalid blocked user ID".to_string()))?;

            let friendship = Friendship {
                user_id: current_user,
                friend_id: user_id,
                status: FriendshipStatus::Blocked,
                created_at: Utc::now(),
            };

            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            blocked_ids.push(user_id);
        }

        Ok(blocked_ids)
    }

    // ========================================================================
    // Local Query Methods
    // ========================================================================

    /// Get a user from local storage only.
    pub fn get_user_local(&self, user_id: &UserId) -> Result<Option<User>, UserError> {
        self.user_repo.get(user_id).map_err(UserError::Storage)
    }

    /// Get a user's profile from local storage only.
    pub fn get_profile_local(&self, user_id: &UserId) -> Result<Option<Profile>, UserError> {
        self.user_repo.get_profile(user_id).map_err(UserError::Storage)
    }

    /// Get a user's display name from local storage.
    pub fn get_display_name(&self, user_id: &UserId) -> String {
        if let Ok(Some(profile)) = self.user_repo.get_profile(user_id) {
            return profile.display_name;
        }
        if let Ok(Some(user)) = self.user_repo.get(user_id) {
            return user.username;
        }
        "Unknown".to_string()
    }

    /// Get the current user's ID.
    pub fn current_user_id(&self) -> Result<UserId, UserError> {
        self.settings_repo.current_user_id().map_err(UserError::from)
    }

    /// Search users in local storage.
    pub fn search_users_local(&self, query: &str) -> Result<Vec<User>, UserError> {
        let current_user = self.settings_repo.current_user_id()?;
        let friendships = self.user_repo.list_friends(&current_user)?;

        let query_lower = query.to_lowercase();
        let mut matching_users = Vec::new();

        for friendship in friendships {
            if let Ok(Some(user)) = self.user_repo.get(&friendship.friend_id) {
                if user.username.to_lowercase().contains(&query_lower) {
                    matching_users.push(user);
                }
            }
        }

        Ok(matching_users)
    }

    // ========================================================================
    // Search Operations
    // ========================================================================

    /// Search for users by query (network call).
    pub async fn search_users(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Profile>, UserError> {
        let _ = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let response = user_client
            .search_users(query, limit as i32, offset as i32)
            .await?;

        let mut profiles = Vec::with_capacity(response.results.len());
        for proto_profile in &response.results {
            let profile = proto_to_domain_profile(proto_profile)?;
            profiles.push(profile);
        }

        Ok(profiles)
    }
}
