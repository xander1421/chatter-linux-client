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
use crate::storage::repositories::anon_dm_repo::AnonDmRepository;
use crate::storage::repositories::anon_group_repo::AnonGroupRepository;
use crate::storage::repositories::user_repo::UserRepository;

pub(crate) mod conversions;
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
///
/// DMs are channel-free (Signal-style) — there is no server DM channel ID
/// to return. The DM conversation is keyed by the friend's `UserId`, which
/// callers already have.
#[derive(Clone, Debug)]
pub struct FriendResponseResult {
    /// The friendship if accepted, None if declined.
    pub friendship: Option<Friendship>,
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
    /// Peers of anonymous DMs and members of anonymous groups are stored here
    /// instead of `user_repo` (they are outside the user directory).
    /// `get_display_name` consults these to avoid raw-UUID fallback.
    anon_dm_repo: Arc<AnonDmRepository>,
    anon_group_repo: Arc<AnonGroupRepository>,
}

impl UserService {
    /// Create a new UserService with dependencies.
    pub fn new(
        grpc_pool: Arc<GrpcClientPool>,
        settings_repo: Arc<SettingsRepository>,
        user_repo: Arc<UserRepository>,
        anon_dm_repo: Arc<AnonDmRepository>,
        anon_group_repo: Arc<AnonGroupRepository>,
    ) -> Self {
        Self {
            grpc_pool,
            settings_repo,
            user_repo,
            anon_dm_repo,
            anon_group_repo,
        }
    }

    /// Create a user client for network operations.
    fn user_client(&self) -> UserClient {
        self.grpc_pool.user_client()
    }

    // ========================================================================
    // Profile Operations
    // ========================================================================

    /// Get a user's profile by ID.
    ///
    /// Fetches from server and upserts to local storage.
    /// If we have the user's profile key (from friendship), decrypts display_name.
    pub async fn get_user_profile(&self, user_id: &UserId) -> Result<Profile, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let proto_profile = user_client
            .get_user_profile(&user_id.to_string())
            .await?;

        let mut profile = proto_to_domain_profile(&proto_profile)?;

        // Try to decrypt display_name if we have their profile key
        profile.display_name = self.try_decrypt_display_name(
            &current_user,
            user_id,
            &proto_profile.display_name,
        ).unwrap_or_else(|| {
            // No profile key or decryption failed
            // Never show encrypted ciphertext - use a placeholder instead
            if proto_profile.display_name.is_empty() {
                format!("User {}", &user_id.to_string()[..8.min(user_id.to_string().len())])
            } else if proto_profile.display_name.len() > 50 {
                // Likely encrypted (base64 encoded ciphertext is typically long)
                // Use a short placeholder instead of showing ciphertext
                format!("User {}", &user_id.to_string()[..8.min(user_id.to_string().len())])
            } else {
                // Might be unencrypted legacy name - keep it
                proto_profile.display_name.clone()
            }
        });

        // Decrypt the avatar with the friend's profile key (if we have it).
        profile.avatar_bytes = self.try_decrypt_avatar(
            &current_user,
            user_id,
            &proto_profile.encrypted_avatar,
        );

        self.user_repo.ensure_user_exists(user_id)?;
        self.user_repo.upsert_profile(&profile)?;

        Ok(profile)
    }

    /// Try to decrypt a friend's avatar using their profile key.
    ///
    /// Returns Some(avatar_bytes) if we have their profile key and decryption
    /// succeeds; None if no key, empty blob, or decryption fails. Mirrors
    /// [`Self::try_decrypt_display_name`] but the avatar travels as raw bytes
    /// (no base64) in the proto's `encrypted_avatar` field.
    fn try_decrypt_avatar(
        &self,
        current_user_id: &UserId,
        friend_id: &UserId,
        encrypted_avatar: &[u8],
    ) -> Option<Vec<u8>> {
        if encrypted_avatar.is_empty() {
            return None;
        }

        let profile_key = self.user_repo
            .get_friend_profile_key(current_user_id, friend_id)
            .ok()
            .flatten()?;

        crate::crypto::profile_crypto::decrypt_avatar(&profile_key, encrypted_avatar).ok()
    }

    /// Try to decrypt a friend's display_name using their profile key.
    ///
    /// Returns Some(decrypted_name) if we have their profile key and decryption succeeds.
    /// Returns None if no profile key or decryption fails.
    fn try_decrypt_display_name(
        &self,
        current_user_id: &UserId,
        friend_id: &UserId,
        encrypted_base64: &str,
    ) -> Option<String> {
        use base64::Engine;

        // Skip if empty
        if encrypted_base64.is_empty() {
            return None;
        }

        // Get friend's profile key from our local storage
        let profile_key = self.user_repo
            .get_friend_profile_key(current_user_id, friend_id)
            .ok()
            .flatten()?;

        // Try to decode from base64
        let encrypted_bytes = base64::engine::general_purpose::STANDARD
            .decode(encrypted_base64)
            .ok()?;

        // Try to decrypt
        crate::crypto::profile_crypto::decrypt_display_name(&profile_key, &encrypted_bytes).ok()
    }

    /// Update the current user's profile.
    ///
    /// # Security Note
    ///
    /// If display_name is provided, it is encrypted with the profile_key before sending.
    /// Server stores only ciphertext - it cannot read profile data.
    /// Update the current user's profile.
    ///
    /// Owned `Option<String>` (rather than `Option<&str>`) is intentional —
    /// `Option<&str>` plays badly with the async-Rust borrow checker because
    /// the future captures references that must outlive every `.await`. The
    /// owned variant lets callers pass `name.clone()` once and forget.
    pub async fn update_user_profile(
        &self,
        display_name: Option<String>,
        status_message: Option<StatusMessage>,
    ) -> Result<Profile, UserError> {
        use base64::Engine;

        let current_user = self.settings_repo.current_user_id()?;

        let existing = self
            .user_repo
            .get_profile(&current_user)?
            .ok_or_else(|| UserError::UserNotFound(current_user))?;

        // Load our profile_key once — needed to encrypt both display_name and
        // avatar. Server never sees plaintext profile data, only opaque blobs.
        let profile_key: Option<[u8; 32]> = match self.settings_repo.get_bytes(keys::PROFILE_KEY)? {
            Some(bytes) => bytes.try_into().ok(),
            None => None,
        };

        // SECURITY: Encrypt display_name with profile_key if being updated
        let encrypted_display_name = if let Some(ref name) = display_name {
            let profile_key = profile_key.ok_or(UserError::NotLoggedIn)?;
            let encrypted = crate::crypto::profile_crypto::encrypt_display_name(&profile_key, name)?;
            Some(base64::engine::general_purpose::STANDARD.encode(encrypted))
        } else {
            None
        };

        // Encrypt our avatar (from the local AVATAR settings key) for E2E
        // delivery to friends — same profile-key crypto the anon path uses, but
        // carried in the server UserProfile's `encrypted_avatar` blob. Normalize
        // (downscale/re-encode to WebP via the canonical avatar normalizer) when
        // it exceeds the ~64 KB target, then encrypt. If the encrypted blob would
        // still exceed the wire cap, skip it so the rest of the update succeeds.
        let (encrypted_avatar, own_avatar_bytes): (Vec<u8>, Option<Vec<u8>>) =
            match (profile_key, self.settings_repo.get_bytes(keys::AVATAR)?) {
                (Some(pk), Some(avatar)) if !avatar.is_empty() => {
                    const AVATAR_TARGET_BYTES: usize = 64 * 1024;
                    const ENCRYPTED_AVATAR_CAP: usize = 96_000;
                    let normalized = if avatar.len() > AVATAR_TARGET_BYTES {
                        crate::media::ImageProcessor::process_avatar_from_memory(&avatar)
                            .unwrap_or(avatar)
                    } else {
                        avatar
                    };
                    match crate::crypto::profile_crypto::encrypt_avatar(&pk, &normalized) {
                        Ok(enc) if enc.len() <= ENCRYPTED_AVATAR_CAP => (enc, Some(normalized)),
                        Ok(enc) => {
                            tracing::warn!(
                                encrypted_len = enc.len(),
                                cap = ENCRYPTED_AVATAR_CAP,
                                "Encrypted avatar exceeds wire cap; omitting from profile update"
                            );
                            (Vec::new(), Some(normalized))
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to encrypt avatar; omitting from profile update");
                            (Vec::new(), None)
                        }
                    }
                }
                _ => (Vec::new(), None),
            };

        let proto_profile = UserProfile {
            user_id: current_user.to_string(),
            display_name: encrypted_display_name
                .unwrap_or_else(|| existing.display_name.clone()),
            status_message: status_message
                .as_ref()
                .map(|s| s.text.clone())
                .or_else(|| existing.status_message.as_ref().map(|s| s.text.clone()))
                .unwrap_or_default(),
            presence: domain_presence_to_proto(existing.presence),
            encrypted_name: Vec::new(),
            encrypted_about: Vec::new(),
            encrypted_avatar: encrypted_avatar.clone(),
            profile_key_version: String::new(),
        };

        let mut paths = Vec::new();
        if display_name.is_some() {
            paths.push("display_name".to_string());
        }
        if status_message.is_some() {
            paths.push("status_message".to_string());
        }
        if !encrypted_avatar.is_empty() {
            paths.push("encrypted_avatar".to_string());
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

        let mut profile = profile_opt.ok_or_else(|| {
            UserError::InvalidInput("no profile updated event received".to_string())
        })?;

        // Decrypt the display name before storing locally
        // The server returns the encrypted version, but we want plaintext in local storage
        if let Some(plaintext_name) = display_name {
            profile.display_name = plaintext_name;
        }

        // Store our own (normalized) avatar bytes locally — the returned event
        // only carries the encrypted blob, which we can't decrypt for ourselves
        // via the friend-key path.
        if own_avatar_bytes.is_some() {
            profile.avatar_bytes = own_avatar_bytes;
        }

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
            requester_id: current_user,
            addressee_id: *friend_id,
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

        // DMs are channel-free (Signal-style) — the server no longer
        // returns a DM channel_id here, just the terminal friend status.
        while let Some(event_result) = stream.next().await {
            let event = event_result?;
            if let Some(user_event::Event::FriendStatusChanged(ref status_response)) = event.event
            {
                let status = status_response.status();
                tracing::debug!("Friend response event: status={:?}", status);

                if status != FriendStatus::Pending {
                    tracing::info!("Friend response final status: {:?}", status);
                    break;
                }
            }
        }

        if accept {
            let friendship = Friendship {
                requester_id: current_user,
                addressee_id: *friend_id,
                status: FriendshipStatus::Accepted,
                created_at: Utc::now(),
            };

            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            Ok(FriendResponseResult {
                friendship: Some(friendship),
            })
        } else {
            let _ = self.user_repo.delete_friendship(&current_user, friend_id);
            Ok(FriendResponseResult { friendship: None })
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
                requester_id: current_user,
                addressee_id: friend_id,
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

    /// List accepted friends with their profile data.
    ///
    /// Returns `(Friendship, Profile)` pairs by converting the `UserProfile`
    /// objects already present in the server response, avoiding extra RPCs.
    pub async fn list_friends_with_profiles(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<(Friendship, Profile)>, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let response = user_client
            .list_friends(limit as i32, offset as i32)
            .await?;

        let mut results = Vec::with_capacity(response.friends.len());
        for friend in &response.friends {
            let friend_id: UserId = friend
                .user_id
                .parse()
                .map_err(|_| UserError::InvalidInput("invalid friend user ID".to_string()))?;

            let friendship = Friendship {
                requester_id: current_user,
                addressee_id: friend_id,
                status: FriendshipStatus::Accepted,
                created_at: Utc::now(),
            };

            if self.user_repo.update_friendship(&friendship).is_err() {
                let _ = self.user_repo.insert_friendship(&friendship);
            }

            // Build profile from the proto UserProfile already in the response.
            let mut profile = proto_to_domain_profile(friend)?;
            profile.display_name = self.try_decrypt_display_name(
                &current_user,
                &friend_id,
                &friend.display_name,
            ).unwrap_or_else(|| {
                if friend.display_name.is_empty() || friend.display_name.len() > 50 {
                    format!("User {}", &friend_id.to_string()[..8.min(friend_id.to_string().len())])
                } else {
                    friend.display_name.clone()
                }
            });
            profile.avatar_bytes = self.try_decrypt_avatar(
                &current_user,
                &friend_id,
                &friend.encrypted_avatar,
            );

            self.user_repo.ensure_user_exists(&friend_id).ok();
            self.user_repo.upsert_profile(&profile).ok();

            results.push((friendship, profile));
        }

        Ok(results)
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
                requester_id: current_user,
                addressee_id: friend_id,
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

    /// Persist an accepted friendship locally with us as the requester.
    ///
    /// The accepter records the friendship inline in `respond_friend_request`,
    /// but the ORIGINAL requester only learns of acceptance asynchronously via
    /// the `FriendAccepted` user-event. Since the channel-free DM sidebar is
    /// built from the local friends list, the requester must write this row or
    /// the peer never appears. Idempotent (update-then-insert).
    pub fn record_accepted_friend_local(&self, friend_id: &UserId) -> Result<(), UserError> {
        let current_user = self.settings_repo.current_user_id()?;
        let friendship = Friendship {
            requester_id: current_user,
            addressee_id: *friend_id,
            status: FriendshipStatus::Accepted,
            created_at: Utc::now(),
        };
        if self.user_repo.update_friendship(&friendship).is_err() {
            self.user_repo
                .insert_friendship(&friendship)
                .map_err(UserError::Storage)?;
        }
        Ok(())
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
            requester_id: current_user,
            addressee_id: *user_id,
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
                requester_id: current_user,
                addressee_id: user_id,
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
    ///
    /// Resolution order:
    /// 1. User directory profile (server DMs, channel members, contacts)
    /// 2. User directory username (any registered peer)
    /// 3. Anon DM peer (peer isn't in the user directory — stored on the
    ///    anon DM conversation row)
    /// 4. Anon group member (stored on the group's member row in any group)
    /// 5. Raw UUID fallback
    pub fn get_display_name(&self, user_id: &UserId) -> String {
        if let Ok(Some(profile)) = self.user_repo.get_profile(user_id) {
            return profile.display_name;
        }
        if let Ok(Some(user)) = self.user_repo.get(user_id) {
            return user.username;
        }
        if let Ok(Some(anon_dm)) = self.anon_dm_repo.get_conversation_by_peer(user_id) {
            if let Some(name) = anon_dm.peer_display_name {
                if !name.is_empty() {
                    return name;
                }
            }
        }
        if let Ok(Some(name)) = self.anon_group_repo.find_display_name_for_user(user_id) {
            if !name.is_empty() {
                return name;
            }
        }
        user_id.to_string()
    }

    /// Get the current user's ID.
    pub fn current_user_id(&self) -> Result<UserId, UserError> {
        self.settings_repo.current_user_id().map_err(UserError::from)
    }

    // ========================================================================
    // Lookup Operations
    // ========================================================================

    /// Signal-style username lookup.
    ///
    /// `username` must be the full `nickname.discriminator` form (e.g.
    /// `alice.4711`); plain nicknames are rejected by libsignal's
    /// `Username::new`. Returns `Ok(None)` when no account is registered
    /// under the name.
    pub async fn lookup_by_username(
        &self,
        username: &str,
    ) -> Result<Option<crate::domain::UserId>, UserError> {
        let _ = self.settings_repo.current_user_id()?;
        let hash = crate::crypto::auth_crypto::hash_username_lookup(username)
            .map_err(|e| UserError::InvalidInput(format!("invalid username: {e}")))?;
        let mut user_client = self.user_client();
        match user_client.lookup_username(hash).await? {
            Some(id_str) => Ok(id_str.parse().ok()),
            None => Ok(None),
        }
    }

    // ========================================================================
    // Friend Code Operations (Privacy-first discovery)
    // ========================================================================

    /// Generate a friend code URL for the current user.
    ///
    /// Format: `chatter://connect/{user_id}`
    ///
    /// This code can be shared or encoded as a QR code. It only contains
    /// the user ID - no username is exposed. Profile key is exchanged
    /// via E2E message after connection is established.
    pub fn generate_friend_code(&self) -> Result<String, UserError> {
        let current_user = self.settings_repo.current_user_id()?;
        Ok(format!("chatter://connect/{}", current_user))
    }

    /// Generate a friend code as plain text (for copying).
    ///
    /// Format: `{user_id}` (just the UUID, no URL scheme)
    pub fn generate_friend_code_plain(&self) -> Result<String, UserError> {
        let current_user = self.settings_repo.current_user_id()?;
        Ok(current_user.to_string())
    }

    /// Generate a QR code image (PNG) containing the friend code URL.
    ///
    /// Returns PNG image bytes that can be displayed in the UI.
    /// The QR code encodes `chatter://connect/{user_id}`.
    ///
    /// # Arguments
    ///
    /// * `size` - Size of the QR code in pixels (width = height)
    pub fn generate_friend_qr(&self, size: u32) -> Result<Vec<u8>, UserError> {
        use image::{ImageEncoder, Luma};
        use qrcode::QrCode;

        let code_url = self.generate_friend_code()?;

        // Generate QR code
        let qr = QrCode::new(code_url.as_bytes()).map_err(|e| {
            UserError::InvalidInput(format!("Failed to generate QR code: {}", e))
        })?;

        // Render to image with quiet zone
        let qr_image = qr.render::<Luma<u8>>().quiet_zone(true).build();

        // Scale to requested size
        let scaled = image::imageops::resize(
            &qr_image,
            size,
            size,
            image::imageops::FilterType::Nearest,
        );

        // Encode as PNG
        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(
                scaled.as_raw(),
                size,
                size,
                image::ExtendedColorType::L8,
            )
            .map_err(|e| UserError::InvalidInput(format!("Failed to encode PNG: {}", e)))?;

        Ok(png_bytes)
    }

    /// Generate a QR code as base64-encoded PNG (for embedding in UI).
    ///
    /// Returns a data URL that can be used directly in image sources.
    pub fn generate_friend_qr_base64(&self, size: u32) -> Result<String, UserError> {
        use base64::Engine;
        let png_bytes = self.generate_friend_qr(size)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
        Ok(format!("data:image/png;base64,{}", b64))
    }

    // ========================================================================
    // NDSS'21 Private Friend Codes (includes identity key + delivery token)
    // ========================================================================

    /// Generate a private friend code for NDSS'21 compliant anonymous DM.
    ///
    /// Format: `chatter://private/{base32(user_id || identity_key || delivery_token)}`
    ///
    /// This code embeds all cryptographic material needed for initial contact:
    /// - User ID (16 bytes)
    /// - Identity key (33 bytes - 0x05 + Curve25519)
    /// - Delivery token (12 bytes - derived from profile key)
    ///
    /// # Privacy Guarantees
    ///
    /// When someone scans this code and connects:
    /// - NO prekey fetch is required (keys are embedded)
    /// - Server learns NOTHING about who is contacting whom
    /// - Initial message is HPKE-encrypted to identity key
    ///
    /// # Security Note
    ///
    /// This code contains your identity key. Only share with people you want
    /// to be able to contact you privately. The code can be regenerated with
    /// a new profile key to invalidate old delivery tokens.
    pub fn generate_private_friend_code(&self) -> Result<String, UserError> {
        use crate::domain::PrivateFriendCode;

        let current_user = self.settings_repo.current_user_id()?;

        // Get identity key
        let identity_key_bytes = self
            .settings_repo
            .get_bytes(keys::IDENTITY_KEY_PUBLIC)?
            .ok_or(UserError::NotLoggedIn)?;

        if identity_key_bytes.len() != 33 {
            return Err(UserError::InvalidInput(format!(
                "Invalid identity key length: {}",
                identity_key_bytes.len()
            )));
        }

        let mut identity_key = [0u8; 33];
        identity_key.copy_from_slice(&identity_key_bytes);

        // Get profile key and derive delivery token
        let profile_key_bytes = self
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)?
            .ok_or(UserError::NotLoggedIn)?;

        if profile_key_bytes.len() != 32 {
            return Err(UserError::InvalidInput(format!(
                "Invalid profile key length: {}",
                profile_key_bytes.len()
            )));
        }

        let mut profile_key = [0u8; 32];
        profile_key.copy_from_slice(&profile_key_bytes);
        let delivery_token = PrivateFriendCode::derive_delivery_token(&profile_key);

        // Build the private friend code
        let code = PrivateFriendCode::new(current_user, identity_key, delivery_token);

        Ok(code.to_url())
    }

    /// Generate a QR code for private friend code.
    ///
    /// Returns PNG image bytes encoding the private friend code URL.
    pub fn generate_private_friend_qr(&self, size: u32) -> Result<Vec<u8>, UserError> {
        use image::{ImageEncoder, Luma};
        use qrcode::QrCode;

        let code_url = self.generate_private_friend_code()?;

        // Generate QR code (private codes are longer, may need higher error correction)
        let qr = QrCode::with_error_correction_level(
            code_url.as_bytes(),
            qrcode::EcLevel::M,
        )
        .map_err(|e| {
            UserError::InvalidInput(format!("Failed to generate private QR code: {}", e))
        })?;

        // Render to image with quiet zone
        let qr_image = qr.render::<Luma<u8>>().quiet_zone(true).build();

        // Scale to requested size
        let scaled = image::imageops::resize(
            &qr_image,
            size,
            size,
            image::imageops::FilterType::Nearest,
        );

        // Encode as PNG
        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(
                scaled.as_raw(),
                size,
                size,
                image::ExtendedColorType::L8,
            )
            .map_err(|e| UserError::InvalidInput(format!("Failed to encode PNG: {}", e)))?;

        Ok(png_bytes)
    }

    /// Parse a friend code and extract the user ID.
    ///
    /// Accepts formats:
    /// - `chatter://connect/{user_id}` (URL format)
    /// - `{user_id}` (plain UUID)
    pub fn parse_friend_code(code: &str) -> Result<UserId, UserError> {
        let code = code.trim();

        // Try URL format first
        if let Some(id_str) = code.strip_prefix("chatter://connect/") {
            return id_str.parse::<UserId>().map_err(|_| {
                UserError::InvalidInput(format!("Invalid user ID in friend code: {}", id_str))
            });
        }

        // Try plain UUID
        code.parse::<UserId>().map_err(|_| {
            UserError::InvalidInput(format!("Invalid friend code format: {}", code))
        })
    }

    /// Connect with a user via friend code.
    ///
    /// This initiates the friend request flow using only the user ID.
    /// The profile key will be exchanged via E2E message once connected.
    ///
    /// # Arguments
    ///
    /// * `code` - Friend code (URL or plain user ID)
    /// * `message` - Optional friend request message
    pub async fn connect_via_friend_code(
        &self,
        code: &str,
        message: Option<&str>,
    ) -> Result<Friendship, UserError> {
        // Parse the friend code to get user ID
        let friend_id = Self::parse_friend_code(code)?;

        // Check we're not adding ourselves
        let current_user = self.settings_repo.current_user_id()?;
        if friend_id == current_user {
            return Err(UserError::InvalidInput(
                "Cannot add yourself as a friend".to_string(),
            ));
        }

        // Check if already a friend
        if let Ok(Some(existing)) = self.user_repo.get_friendship(&current_user, &friend_id) {
            if existing.status == FriendshipStatus::Accepted {
                return Err(UserError::InvalidInput(
                    "Already friends with this user".to_string(),
                ));
            }
            if existing.status == FriendshipStatus::Pending {
                return Err(UserError::InvalidInput(
                    "Friend request already pending".to_string(),
                ));
            }
        }

        // Send friend request
        self.send_friend_request(&friend_id, message).await
    }
}
