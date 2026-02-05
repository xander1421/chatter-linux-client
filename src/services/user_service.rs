//! User service.
//!
//! Profile, friends, blocking, and presence management.

use std::sync::Arc;

use tokio_stream::StreamExt;

use chrono::Utc;

use crate::domain::ids::UserId;
use crate::domain::user::{Friendship, FriendshipStatus, PresenceStatus, Profile, StatusMessage};
use crate::error::{NetworkError, StorageError};
use crate::network::grpc::user_client::UserClient;
use crate::proto::chat_v1::{user_event, FriendStatus, UserProfile};
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
use crate::storage::repositories::user_repo::UserRepository;

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

/// User service for all user operations.
///
/// Orchestrates profile management, friends, blocking, and presence by
/// coordinating network client and local storage.
pub struct UserService {
    settings_repo: Arc<SettingsRepository>,
    user_repo: Arc<UserRepository>,
}

impl UserService {
    /// Create a new UserService with dependencies.
    pub fn new(settings_repo: Arc<SettingsRepository>, user_repo: Arc<UserRepository>) -> Self {
        Self {
            settings_repo,
            user_repo,
        }
    }

    /// Get the currently logged-in user ID.
    fn current_user_id(&self) -> Result<UserId, UserError> {
        let user_id_str = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(UserError::Storage)?
            .ok_or(UserError::NotLoggedIn)?;

        user_id_str
            .parse()
            .map_err(|_| UserError::InvalidInput("invalid user ID in settings".to_string()))
    }

    /// Get a user's profile by ID.
    ///
    /// Fetches from server and upserts to local storage.
    ///
    /// # Arguments
    ///
    /// * `user_client` - Mutable reference to user client
    /// * `user_id` - ID of user whose profile to fetch
    pub async fn get_user_profile(
        &self,
        user_client: &mut UserClient,
        user_id: &UserId,
    ) -> Result<Profile, UserError> {
        // Check logged in
        let _ = self.current_user_id()?;

        // Call server (unary, returns proto UserProfile)
        let proto_profile = user_client
            .get_user_profile(&user_id.to_string())
            .await?;

        // Convert to domain Profile
        let profile = proto_to_domain_profile(&proto_profile)?;

        // Upsert to local storage
        self.user_repo.upsert_profile(&profile)?;

        Ok(profile)
    }

    /// Update the current user's profile.
    ///
    /// # Arguments
    ///
    /// * `user_client` - Mutable reference to user client
    /// * `display_name` - Optional new display name
    /// * `avatar_url` - Optional new avatar URL
    /// * `status_message` - Optional new status message
    pub async fn update_user_profile(
        &self,
        user_client: &mut UserClient,
        display_name: Option<&str>,
        avatar_url: Option<&str>,
        status_message: Option<&StatusMessage>,
    ) -> Result<Profile, UserError> {
        // Check logged in
        let current_user = self.current_user_id()?;

        // Get current profile from local DB (needed to build proto)
        let existing = self
            .user_repo
            .get_profile(&current_user)?
            .ok_or_else(|| UserError::UserNotFound(current_user.clone()))?;

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
    /// * `user_client` - Mutable reference to user client
    /// * `friend_id` - ID of user to send friend request to
    /// * `message` - Optional message to include with request
    pub async fn send_friend_request(
        &self,
        user_client: &mut UserClient,
        friend_id: &UserId,
        message: Option<&str>,
    ) -> Result<Friendship, UserError> {
        // Check logged in
        let current_user = self.current_user_id()?;

        // Call server (streaming response)
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
            friend_id: friend_id.clone(),
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
    /// * `user_client` - Mutable reference to user client
    /// * `friend_id` - ID of user who sent the request
    /// * `accept` - Whether to accept (true) or decline (false) the request
    pub async fn respond_friend_request(
        &self,
        user_client: &mut UserClient,
        friend_id: &UserId,
        accept: bool,
    ) -> Result<Option<Friendship>, UserError> {
        // Check logged in
        let current_user = self.current_user_id()?;

        // Determine FriendStatus for proto
        let status = if accept {
            FriendStatus::Accepted
        } else {
            FriendStatus::Declined
        };

        // Call server (streaming response)
        let friend_id_str = friend_id.to_string();
        let mut stream = user_client
            .respond_friend_request(&friend_id_str, status, None)
            .await?;

        // Consume stream for confirmation
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::FriendStatusChanged(ref _status_response)) = event.event
            {
                // Server confirmed the response
                break;
            }
        }

        if accept {
            // Upsert friendship with Accepted status
            let friendship = Friendship {
                user_id: current_user,
                friend_id: friend_id.clone(),
                status: FriendshipStatus::Accepted,
                created_at: Utc::now(),
            };

            // Try update first, then insert if not exists
            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            Ok(Some(friendship))
        } else {
            // Delete friendship from local storage if exists
            let _ = self.user_repo.delete_friendship(&current_user, friend_id);
            Ok(None)
        }
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
}
