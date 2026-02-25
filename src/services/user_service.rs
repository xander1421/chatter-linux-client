//! User service.
//!
//! Profile, friends, blocking, and presence management.

use std::sync::Arc;

use tokio_stream::StreamExt;

use chrono::Utc;

use crate::domain::ids::UserId;
use crate::domain::user::{Friendship, FriendshipStatus, PresenceStatus, Profile, StatusMessage, User};
use crate::error::{NetworkError, StorageError};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::user_client::UserClient;
use crate::proto::chat_v1::{friend_requests_filter, user_event, FriendStatus, UserProfile};
use crate::services::current_user::{CurrentUserError, CurrentUserProvider};
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::user_repo::UserRepository;

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
    fn to_proto(self) -> friend_requests_filter::Filter {
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

    /// Get a user's profile by ID.
    ///
    /// Fetches from server and upserts to local storage.
    ///
    /// # Arguments
    ///
    /// * `user_id` - ID of user whose profile to fetch
    pub async fn get_user_profile(&self, user_id: &UserId) -> Result<Profile, UserError> {
        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        // Call server (unary, returns proto UserProfile)
        let mut user_client = self.user_client();
        let proto_profile = user_client
            .get_user_profile(&user_id.to_string())
            .await?;

        // Convert to domain Profile
        let profile = proto_to_domain_profile(&proto_profile)?;

        // Ensure user record exists (profiles table has FK to users)
        self.user_repo.ensure_user_exists(user_id)?;

        // Upsert to local storage
        self.user_repo.upsert_profile(&profile)?;

        Ok(profile)
    }

    /// Update the current user's profile.
    ///
    /// # Arguments
    ///
    /// * `display_name` - Optional new display name
    /// * `avatar_url` - Optional new avatar URL
    /// * `status_message` - Optional new status message
    pub async fn update_user_profile(
        &self,
        display_name: Option<&str>,
        avatar_url: Option<&str>,
        status_message: Option<&StatusMessage>,
    ) -> Result<Profile, UserError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Get current profile from local DB (needed to build proto)
        let existing = self
            .user_repo
            .get_profile(&current_user)?
            .ok_or_else(|| UserError::UserNotFound(current_user))?;

        // Build proto UserProfile with updated fields
        let proto_profile = UserProfile {
            user_id: current_user.to_string(),
            display_name: display_name
                .unwrap_or(&existing.display_name)
                .to_string(),
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

        // Build FieldMask with fields being updated
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

        // Call server (streaming)
        let mut user_client = self.user_client();
        let mut stream = user_client
            .update_user_profile(proto_profile, update_mask)
            .await?;

        // Consume stream looking for ProfileUpdated event
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

        // Upsert to local storage
        self.user_repo.upsert_profile(&profile)?;

        Ok(profile)
    }

    // ========================================================================
    // Friend Management
    // ========================================================================

    /// Send a friend request to another user.
    ///
    /// Calls the server to initiate the friend request, then stores the
    /// pending friendship locally.
    ///
    /// # Arguments
    ///
    /// * `friend_id` - ID of user to send friend request to
    /// * `message` - Optional message to include with request
    pub async fn send_friend_request(
        &self,
        friend_id: &UserId,
        message: Option<&str>,
    ) -> Result<Friendship, UserError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Call server (streaming response)
        let mut user_client = self.user_client();
        let friend_id_str = friend_id.to_string();
        let mut stream = user_client
            .send_friend_request(&friend_id_str, message)
            .await?;

        // Consume stream looking for FriendStatusChanged event
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::FriendStatusChanged(ref _status_response)) = event.event
            {
                // Server confirmed the request was sent
                break;
            }
        }

        // Create local friendship with Pending status
        let friendship = Friendship {
            user_id: current_user,
            friend_id: *friend_id,
            status: FriendshipStatus::Pending,
            created_at: Utc::now(),
        };

        // Insert to local storage (ignore if already exists)
        let _ = self.user_repo.insert_friendship(&friendship);

        Ok(friendship)
    }

    /// Respond to a friend request.
    ///
    /// Accepts or declines a pending friend request. If accepted, updates
    /// local storage with accepted status. If declined, removes the friendship.
    ///
    /// # Arguments
    ///
    /// * `friend_id` - ID of user who sent the request
    /// * `accept` - Whether to accept (true) or decline (false) the request
    pub async fn respond_friend_request(
        &self,
        friend_id: &UserId,
        accept: bool,
    ) -> Result<FriendResponseResult, UserError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Determine FriendStatus for proto
        let status = if accept {
            FriendStatus::Accepted
        } else {
            FriendStatus::Declined
        };

        // Call server (streaming response)
        let mut user_client = self.user_client();
        let friend_id_str = friend_id.to_string();
        let mut stream = user_client
            .respond_friend_request(&friend_id_str, status, None)
            .await?;

        // Consume stream for confirmation and extract channel_id
        // Server sends PENDING first, then ACCEPTED/DECLINED with channel_id
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

                // Extract channel_id if present
                if !status_response.channel_id.is_empty() {
                    dm_channel_id = Some(status_response.channel_id.clone());
                    tracing::info!(
                        "Friend response returned DM channel ID: {}",
                        status_response.channel_id
                    );
                }

                // Only break on final status (not PENDING)
                // PENDING is sent first, then ACCEPTED/DECLINED with channel_id
                if status != FriendStatus::Pending {
                    tracing::info!("Friend response final status: {:?}", status);
                    break;
                }
            }
        }

        if accept {
            // Upsert friendship with Accepted status
            let friendship = Friendship {
                user_id: current_user,
                friend_id: *friend_id,
                status: FriendshipStatus::Accepted,
                created_at: Utc::now(),
            };

            // Try update first, then insert if not exists
            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            Ok(FriendResponseResult {
                friendship: Some(friendship),
                dm_channel_id,
            })
        } else {
            // Delete friendship from local storage if exists
            let _ = self.user_repo.delete_friendship(&current_user, friend_id);
            Ok(FriendResponseResult {
                friendship: None,
                dm_channel_id: None,
            })
        }
    }

    /// List accepted friends.
    ///
    /// Fetches friends from server and upserts to local storage.
    ///
    /// # Arguments
    ///
    /// * `limit` - Maximum number of friends to return
    /// * `offset` - Pagination offset
    pub async fn list_friends(&self, limit: u32, offset: u32) -> Result<Vec<Friendship>, UserError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Call server (unary)
        let mut user_client = self.user_client();
        let response = user_client
            .list_friends(limit as i32, offset as i32)
            .await?;

        // Convert each friend to domain Friendship and upsert
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

            // Upsert to local storage
            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            friendships.push(friendship);
        }

        Ok(friendships)
    }

    /// List friend requests with optional filtering.
    ///
    /// Fetches friend requests from server and upserts to local storage.
    ///
    /// # Arguments
    ///
    /// * `filter` - Filter for request types
    /// * `limit` - Maximum number of requests to return
    /// * `offset` - Pagination offset
    pub async fn list_friend_requests(
        &self,
        filter: FriendRequestFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Friendship>, UserError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Convert filter to proto
        let proto_filter = filter.to_proto();

        // Call server (unary)
        let mut user_client = self.user_client();
        let response = user_client
            .list_friend_requests(proto_filter, limit as i32, offset as i32)
            .await?;

        // Convert each request to domain Friendship and upsert
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

            // Upsert to local storage
            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            friendships.push(friendship);
        }

        Ok(friendships)
    }

    /// Get friends from local storage only.
    ///
    /// Does not make any network calls.
    pub fn get_friends_local(&self) -> Result<Vec<Friendship>, UserError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Return from local storage
        self.user_repo
            .list_friends(&current_user)
            .map_err(UserError::Storage)
    }

    /// Remove a friend.
    ///
    /// Calls server to remove friendship, then deletes from local storage.
    ///
    /// # Arguments
    ///
    /// * `friend_id` - ID of friend to remove
    pub async fn remove_friend(&self, friend_id: &UserId) -> Result<(), UserError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Call server (unary)
        let mut user_client = self.user_client();
        user_client.remove_friend(&friend_id.to_string()).await?;

        // Delete from local storage (ignore NotFound)
        let _ = self.user_repo.delete_friendship(&current_user, friend_id);

        Ok(())
    }

    // ========================================================================
    // Blocking Operations
    // ========================================================================

    /// Block a user.
    ///
    /// Calls server to block user, then updates local storage with Blocked status.
    ///
    /// # Arguments
    ///
    /// * `user_id` - ID of user to block
    pub async fn block_user(&self, user_id: &UserId) -> Result<(), UserError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Call server (streaming response)
        let mut user_client = self.user_client();
        let user_id_str = user_id.to_string();
        let mut stream = user_client.block_user(&user_id_str).await?;

        // Consume stream for confirmation event
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::FriendStatusChanged(ref _status)) = event.event {
                // Server confirmed the block
                break;
            }
        }

        // Update local friendship to Blocked status (or create if missing)
        let friendship = Friendship {
            user_id: current_user,
            friend_id: *user_id,
            status: FriendshipStatus::Blocked,
            created_at: Utc::now(),
        };

        // Try update first, then insert if not exists
        if self.user_repo.update_friendship(&friendship).is_err() {
            let _ = self.user_repo.insert_friendship(&friendship);
        }

        Ok(())
    }

    /// Unblock a user.
    ///
    /// Calls server to unblock user, then removes the friendship from local storage.
    ///
    /// # Arguments
    ///
    /// * `user_id` - ID of user to unblock
    pub async fn unblock_user(&self, user_id: &UserId) -> Result<(), UserError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Call server (streaming response)
        let mut user_client = self.user_client();
        let user_id_str = user_id.to_string();
        let mut stream = user_client.unblock_user(&user_id_str).await?;

        // Consume stream for confirmation event
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::FriendStatusChanged(ref _status)) = event.event {
                // Server confirmed the unblock
                break;
            }
        }

        // Delete the friendship from local storage (unblock = remove relationship)
        let _ = self.user_repo.delete_friendship(&current_user, user_id);

        Ok(())
    }

    /// List blocked users.
    ///
    /// Fetches blocked users from server and updates local storage.
    ///
    /// # Arguments
    ///
    /// * `limit` - Maximum number of blocked users to return
    /// * `offset` - Pagination offset
    pub async fn list_blocked_users(&self, limit: u32, offset: u32) -> Result<Vec<UserId>, UserError> {
        // Check logged in
        let current_user = self.settings_repo.current_user_id()?;

        // Call server (unary)
        let mut user_client = self.user_client();
        let response = user_client
            .list_blocked_users(limit as i32, offset as i32)
            .await?;

        // Parse user IDs from response (proto User has id field)
        let mut blocked_ids = Vec::with_capacity(response.blocked_users.len());
        for proto_user in &response.blocked_users {
            let user_id: UserId = proto_user
                .id
                .parse()
                .map_err(|_| UserError::InvalidInput("invalid blocked user ID".to_string()))?;

            // Update local friendship to Blocked status
            let friendship = Friendship {
                user_id: current_user,
                friend_id: user_id,
                status: FriendshipStatus::Blocked,
                created_at: Utc::now(),
            };

            // Try update first, then insert if not exists
            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            blocked_ids.push(user_id);
        }

        Ok(blocked_ids)
    }

    // ========================================================================
    // Local Query Methods (no network, no auth check needed)
    // ========================================================================

    /// Get a user from local storage only.
    ///
    /// Does not make any network calls.
    pub fn get_user_local(&self, user_id: &UserId) -> Result<Option<User>, UserError> {
        self.user_repo.get(user_id).map_err(UserError::Storage)
    }

    /// Get a user's profile from local storage only.
    ///
    /// Does not make any network calls.
    pub fn get_profile_local(&self, user_id: &UserId) -> Result<Option<Profile>, UserError> {
        self.user_repo.get_profile(user_id).map_err(UserError::Storage)
    }

    /// Search users in local storage.
    ///
    /// Searches known users (friends) by username. This is limited to users
    /// that have been fetched from the server (friends, etc.). For discovery
    /// of new users, use the `search_users` method which makes a network call.
    ///
    /// Does not make any network calls.
    pub fn search_users_local(&self, query: &str) -> Result<Vec<User>, UserError> {
        // Get current user for friendship lookup
        let current_user = self.settings_repo.current_user_id()?;

        // Get all friendships for current user
        let friendships = self.user_repo.list_friends(&current_user)?;

        // For each friend, fetch user and filter by username
        let query_lower = query.to_lowercase();
        let mut matching_users = Vec::new();

        for friendship in friendships {
            if let Ok(Some(user)) = self.user_repo.get(&friendship.friend_id) {
                // Case-insensitive username contains check
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
    ///
    /// Searches all users on the server. Results are NOT stored locally
    /// as they are discovery results, not established relationships.
    ///
    /// # Arguments
    ///
    /// * `query` - Search query string
    /// * `limit` - Maximum number of results to return
    /// * `offset` - Pagination offset
    pub async fn search_users(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Profile>, UserError> {
        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        // Call server (unary)
        let mut user_client = self.user_client();
        let response = user_client
            .search_users(query, limit as i32, offset as i32)
            .await?;

        // Convert each proto UserProfile to domain Profile
        // Note: Do NOT store locally (search results are discovery, not relationships)
        let mut profiles = Vec::with_capacity(response.results.len());
        for proto_profile in &response.results {
            let profile = proto_to_domain_profile(proto_profile)?;
            profiles.push(profile);
        }

        Ok(profiles)
    }
}

// ============================================================================
// Proto <-> Domain Conversion Helpers
// ============================================================================

/// Convert a proto UserProfile to a domain Profile.
fn proto_to_domain_profile(proto: &UserProfile) -> Result<Profile, UserError> {
    // Parse user_id
    let user_id: UserId = proto
        .user_id
        .parse()
        .map_err(|_| UserError::InvalidInput("invalid user ID in profile".to_string()))?;

    // Map proto presence (i32) to domain PresenceStatus
    let presence = presence_from_proto(proto.presence);

    // Map status_message (empty string -> None)
    let status_message = if proto.status_message.is_empty() {
        None
    } else {
        Some(StatusMessage::new(proto.status_message.clone()))
    };

    // avatar_url (empty string -> None)
    let avatar_url = if proto.avatar_url.is_empty() {
        None
    } else {
        Some(proto.avatar_url.clone())
    };

    Ok(Profile {
        user_id,
        display_name: proto.display_name.clone(),
        avatar_url,
        status_message,
        presence,
    })
}

/// Convert a domain Profile to a proto UserProfile.
#[allow(dead_code)]
fn domain_to_proto_profile(profile: &Profile) -> UserProfile {
    UserProfile {
        user_id: profile.user_id.to_string(),
        display_name: profile.display_name.clone(),
        avatar_url: profile.avatar_url.clone().unwrap_or_default(),
        status_message: profile
            .status_message
            .as_ref()
            .map(|s| s.text.clone())
            .unwrap_or_default(),
        presence: domain_presence_to_proto(profile.presence),
    }
}

/// Convert proto UserPresence (i32) to domain PresenceStatus.
fn presence_from_proto(value: i32) -> PresenceStatus {
    match value {
        // 0 = PresenceUnspecified -> Offline
        // 1 = Offline
        // 2 = Online
        // 3 = Away
        // 4 = DoNotDisturb
        1 => PresenceStatus::Offline,
        2 => PresenceStatus::Online,
        3 => PresenceStatus::Away,
        4 => PresenceStatus::DoNotDisturb,
        _ => PresenceStatus::Offline, // Default for unspecified or unknown
    }
}

/// Convert domain PresenceStatus to proto UserPresence (i32).
fn domain_presence_to_proto(status: PresenceStatus) -> i32 {
    match status {
        PresenceStatus::Offline => 1,
        PresenceStatus::Online => 2,
        PresenceStatus::Away => 3,
        PresenceStatus::DoNotDisturb => 4,
    }
}

/// Convert proto FriendStatus (i32) to domain FriendshipStatus.
///
/// Proto values:
/// - 3 = Accepted
/// - Others (including Pending=2, Declined=4) -> Pending
fn proto_friend_status_to_domain(status: i32) -> FriendshipStatus {
    match status {
        3 => FriendshipStatus::Accepted,
        5 => FriendshipStatus::Blocked,
        _ => FriendshipStatus::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_error_display() {
        let err = UserError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = UserError::InvalidInput("empty name".to_string());
        assert!(err.to_string().contains("empty name"));

        let user_id = UserId::new();
        let err = UserError::UserNotFound(user_id);
        assert!(err.to_string().contains("user not found"));

        let err = UserError::NotAuthorized;
        assert!(err.to_string().contains("not authorized"));
    }

    #[test]
    fn test_presence_from_proto() {
        assert_eq!(presence_from_proto(0), PresenceStatus::Offline); // Unspecified
        assert_eq!(presence_from_proto(1), PresenceStatus::Offline);
        assert_eq!(presence_from_proto(2), PresenceStatus::Online);
        assert_eq!(presence_from_proto(3), PresenceStatus::Away);
        assert_eq!(presence_from_proto(4), PresenceStatus::DoNotDisturb);
        assert_eq!(presence_from_proto(99), PresenceStatus::Offline); // Unknown
    }

    #[test]
    fn test_domain_presence_to_proto() {
        assert_eq!(domain_presence_to_proto(PresenceStatus::Offline), 1);
        assert_eq!(domain_presence_to_proto(PresenceStatus::Online), 2);
        assert_eq!(domain_presence_to_proto(PresenceStatus::Away), 3);
        assert_eq!(domain_presence_to_proto(PresenceStatus::DoNotDisturb), 4);
    }

    #[test]
    fn test_presence_roundtrip() {
        // Test roundtrip: domain -> proto -> domain
        for status in [
            PresenceStatus::Offline,
            PresenceStatus::Online,
            PresenceStatus::Away,
            PresenceStatus::DoNotDisturb,
        ] {
            let proto = domain_presence_to_proto(status);
            let back = presence_from_proto(proto);
            assert_eq!(back, status);
        }
    }

    #[test]
    fn test_proto_to_domain_profile() {
        let proto = UserProfile {
            user_id: "00000000-0000-0000-0000-000000000001".to_string(),
            display_name: "Test User".to_string(),
            avatar_url: "https://example.com/avatar.png".to_string(),
            status_message: "Hello world".to_string(),
            presence: 2, // Online
        };

        let profile = proto_to_domain_profile(&proto).unwrap();
        assert_eq!(profile.display_name, "Test User");
        assert_eq!(
            profile.avatar_url,
            Some("https://example.com/avatar.png".to_string())
        );
        assert_eq!(
            profile.status_message.as_ref().map(|s| s.text.as_str()),
            Some("Hello world")
        );
        assert_eq!(profile.presence, PresenceStatus::Online);
    }

    #[test]
    fn test_proto_to_domain_profile_empty_strings() {
        let proto = UserProfile {
            user_id: "00000000-0000-0000-0000-000000000001".to_string(),
            display_name: "Test User".to_string(),
            avatar_url: "".to_string(),         // Empty
            status_message: "".to_string(),     // Empty
            presence: 1,                        // Offline
        };

        let profile = proto_to_domain_profile(&proto).unwrap();
        assert_eq!(profile.avatar_url, None);
        assert_eq!(profile.status_message, None);
        assert_eq!(profile.presence, PresenceStatus::Offline);
    }

    #[test]
    fn test_domain_to_proto_profile() {
        let user_id: UserId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let profile = Profile {
            user_id,
            display_name: "Test User".to_string(),
            avatar_url: Some("https://example.com/avatar.png".to_string()),
            status_message: Some(StatusMessage::new("Hello".to_string())),
            presence: PresenceStatus::Away,
        };

        let proto = domain_to_proto_profile(&profile);
        assert_eq!(proto.display_name, "Test User");
        assert_eq!(proto.avatar_url, "https://example.com/avatar.png");
        assert_eq!(proto.status_message, "Hello");
        assert_eq!(proto.presence, 3); // Away
    }

    #[test]
    fn test_domain_to_proto_profile_none_fields() {
        let user_id: UserId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
        let profile = Profile {
            user_id,
            display_name: "Test User".to_string(),
            avatar_url: None,
            status_message: None,
            presence: PresenceStatus::DoNotDisturb,
        };

        let proto = domain_to_proto_profile(&profile);
        assert_eq!(proto.avatar_url, "");
        assert_eq!(proto.status_message, "");
        assert_eq!(proto.presence, 4); // DoNotDisturb
    }

    #[test]
    fn test_friend_request_filter_values() {
        // Verify all filter variants exist and convert to proto correctly
        assert_eq!(
            FriendRequestFilter::All.to_proto(),
            friend_requests_filter::Filter::All
        );
        assert_eq!(
            FriendRequestFilter::Pending.to_proto(),
            friend_requests_filter::Filter::Pending
        );
        assert_eq!(
            FriendRequestFilter::Accepted.to_proto(),
            friend_requests_filter::Filter::Accepted
        );
        assert_eq!(
            FriendRequestFilter::Sent.to_proto(),
            friend_requests_filter::Filter::Sent
        );
        assert_eq!(
            FriendRequestFilter::Received.to_proto(),
            friend_requests_filter::Filter::Received
        );
    }

    #[test]
    fn test_proto_friend_status_conversion() {
        // Proto FriendStatus values:
        // 0 = Unspecified, 1 = None, 2 = Pending, 3 = Accepted, 4 = Declined, 5 = Blocked

        // Accepted maps to Accepted
        assert_eq!(
            proto_friend_status_to_domain(3),
            FriendshipStatus::Accepted
        );

        // Blocked maps to Blocked
        assert_eq!(
            proto_friend_status_to_domain(5),
            FriendshipStatus::Blocked
        );

        // Pending maps to Pending
        assert_eq!(
            proto_friend_status_to_domain(2),
            FriendshipStatus::Pending
        );

        // Other values default to Pending
        assert_eq!(
            proto_friend_status_to_domain(0),
            FriendshipStatus::Pending
        );
        assert_eq!(
            proto_friend_status_to_domain(1),
            FriendshipStatus::Pending
        );
        assert_eq!(
            proto_friend_status_to_domain(4),
            FriendshipStatus::Pending
        );
        assert_eq!(
            proto_friend_status_to_domain(99),
            FriendshipStatus::Pending
        );
    }

    #[test]
    fn test_user_service_construction() {
        // This test verifies the UserService struct type signatures compile correctly.
        // We can't create a real UserService without actual repositories,
        // but we verify the construction pattern is correct.
        fn _verify_types(
            grpc_pool: Arc<crate::network::grpc::client_pool::GrpcClientPool>,
            settings_repo: Arc<SettingsRepository>,
            user_repo: Arc<UserRepository>,
        ) {
            let _service = UserService::new(grpc_pool, settings_repo, user_repo);
        }
    }

    #[test]
    fn test_blocked_status_mapping() {
        // Proto FriendStatus::Blocked = 5 should map to FriendshipStatus::Blocked
        assert_eq!(
            proto_friend_status_to_domain(5),
            FriendshipStatus::Blocked
        );

        // Verify Blocked is distinct from other statuses
        assert_ne!(
            proto_friend_status_to_domain(5),
            FriendshipStatus::Pending
        );
        assert_ne!(
            proto_friend_status_to_domain(5),
            FriendshipStatus::Accepted
        );
    }

    #[test]
    fn test_friend_request_filter_all_variants() {
        // Verify all FriendRequestFilter variants can be converted and are distinct
        let filters = [
            FriendRequestFilter::All,
            FriendRequestFilter::Pending,
            FriendRequestFilter::Accepted,
            FriendRequestFilter::Sent,
            FriendRequestFilter::Received,
        ];

        // Each variant should produce a distinct proto value
        let proto_values: Vec<_> = filters.iter().map(|f| f.to_proto() as i32).collect();
        for i in 0..proto_values.len() {
            for j in (i + 1)..proto_values.len() {
                assert_ne!(
                    proto_values[i], proto_values[j],
                    "Filter variants should produce distinct values"
                );
            }
        }
    }
}
