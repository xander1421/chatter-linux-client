//! User service.
//!
//! Profile, friends, blocking, local queries, and account discovery.
//!
//! This module is organized into:
//! - Service types and shared helpers (this file)
//! - `profile` - Profile fetch, decryption, and updates
//! - `friends` - Friend requests and friendships
//! - `blocking` - Block and unblock operations
//! - `local` - Local user and display-name queries
//! - `lookup` - Username lookup and discoverability
//! - `codes` - Friend codes, QR codes, and username links
//! - `conversions` - Proto <-> Domain conversion helpers
//! - `tests` - Unit tests

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_stream::StreamExt;

use chrono::Utc;
use rand::TryRngCore;

use crate::domain::ids::UserId;
use crate::domain::user::{Friendship, FriendshipStatus, Profile, StatusMessage, User};
use crate::error::{CryptoError, NetworkError, StorageError};
use crate::network::grpc::client_pool::GrpcClientPool;
use crate::network::grpc::user_client::UserClient;
use crate::proto::chat_v1::{FriendStatus, UserProfile, friend_requests_filter, user_event};
use crate::services::current_user::{CurrentUserError, CurrentUserProvider};
use crate::storage::repositories::anon_dm_repo::AnonDmRepository;
use crate::storage::repositories::anon_group_repo::AnonGroupRepository;
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};
use crate::storage::repositories::user_repo::UserRepository;

pub(crate) mod conversions;
#[cfg(test)]
mod tests;

use conversions::{
    domain_presence_to_proto, proto_friend_status_to_domain, proto_to_domain_profile,
};

/// Decrypt a friend's server `display_name` (encrypted base64) with their
/// stored profile key.
///
/// Server display names are always ciphertext; the plaintext is recoverable
/// only with the friend's profile key (exchanged via the Signal-style
/// handshake). Returns `None` when we have no key, the value isn't decodable,
/// or the plaintext is empty. This is the single source of truth for friend
/// name decryption — callers pick the fallback ([`friend_display_name_placeholder`]
/// or an existing local name). Never guesses from length, never returns ciphertext.
pub fn decrypt_friend_display_name(
    user_repo: &UserRepository,
    current_user: &UserId,
    friend_id: &UserId,
    encrypted_base64: &str,
) -> Option<String> {
    use base64::Engine;

    if encrypted_base64.is_empty() {
        return None;
    }
    let key = user_repo
        .get_friend_profile_key(current_user, friend_id)
        .ok()
        .flatten()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encrypted_base64)
        .ok()?;
    let name = crate::crypto::profile_crypto::decrypt_display_name(&key, &bytes).ok()?;
    (!name.is_empty()).then_some(name)
}

/// Stable `User <id8>` placeholder for a friend whose name can't be resolved
/// (no profile key yet). Single source so no caller ever renders ciphertext.
pub fn friend_display_name_placeholder(user_id: &UserId) -> String {
    let id = user_id.to_string();
    format!("User {}", &id[..8.min(id.len())])
}

/// Decrypt a friend's status message (encrypted base64) with their profile
/// key. Same contract as [`decrypt_friend_display_name`]: `None` when we have
/// no key, the value isn't decodable, or the plaintext is empty. Callers drop
/// the status (show nothing) on `None` — never render ciphertext.
pub fn decrypt_friend_status(
    user_repo: &UserRepository,
    current_user: &UserId,
    friend_id: &UserId,
    encrypted_base64: &str,
) -> Option<String> {
    use base64::Engine;

    if encrypted_base64.is_empty() {
        return None;
    }
    let key = user_repo
        .get_friend_profile_key(current_user, friend_id)
        .ok()
        .flatten()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encrypted_base64)
        .ok()?;
    let status = crate::crypto::profile_crypto::decrypt_status(&key, &bytes).ok()?;
    (!status.is_empty()).then_some(status)
}

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

    /// Operation is only permitted in anonymous (shield) mode. Generating a
    /// private/anon friend code outside shield mode would leak the setup over
    /// the identified/direct path, defeating its purpose.
    #[error("requires anon (shield) mode")]
    RequiresAnonMode,

    /// Operation reveals the account's identity (an identified friend
    /// request) and is refused while anon (shield) mode is on.
    #[error("not available in anon (shield) mode")]
    ForbiddenInAnonMode,

    /// A username / username-link lookup, refused in anon (shield) mode: it
    /// ties "who is this account looking for" to our identity.
    #[error("handle lookup not available in anon (shield) mode")]
    HandleLookupForbiddenInAnonMode,

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

impl UserError {
    /// Text for a toast or status line; the `Display` chain
    /// ("invalid input: ... server error: ...") is for logs.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::NotLoggedIn => "You're signed out.".to_string(),
            Self::UserNotFound(_) => "No account found.".to_string(),
            Self::NotAuthorized | Self::Network(NetworkError::PermissionDenied) => {
                "You can't contact this user.".to_string()
            }
            Self::RequiresAnonMode => "Turn on anonymous mode first.".to_string(),
            Self::ForbiddenInAnonMode => "Not available in anonymous mode.".to_string(),
            Self::HandleLookupForbiddenInAnonMode => {
                "Looking up people by handle isn't available in Shield mode.".to_string()
            }
            Self::InvalidInput(m) => {
                let mut c = m.chars();
                c.next()
                    .map(|f| f.to_uppercase().chain(c).collect())
                    .unwrap_or_default()
            }
            Self::Network(NetworkError::NotFound(_)) => "No account found.".to_string(),
            Self::Network(NetworkError::RateLimited) => {
                "Too many attempts. Try again in a minute.".to_string()
            }
            Self::Network(_) => "Couldn't reach the server. Try again.".to_string(),
            Self::Storage(_) | Self::Crypto(_) => "Something went wrong. Try again.".to_string(),
        }
    }
}

/// The recipient's account identity key from an all-devices bundle fetch.
fn friend_request_identity_key(
    resp: crate::proto::chat_v1::GetPreKeyBundleResponse,
) -> Result<Vec<u8>, UserError> {
    use crate::proto::chat_v1::get_pre_key_bundle_response::Result as BundleResult;
    let key = match resp.result {
        Some(BundleResult::Devices(d)) => d.identity_key,
        Some(BundleResult::Bundle(b)) => b.identity_key,
        None => Vec::new(),
    };
    if key.is_empty() {
        return Err(UserError::InvalidInput(
            "this user has no registered devices".to_string(),
        ));
    }
    Ok(key)
}

impl From<CurrentUserError> for UserError {
    fn from(err: CurrentUserError) -> Self {
        match err {
            CurrentUserError::Storage(e) => UserError::Storage(e),
            CurrentUserError::NotLoggedIn => UserError::NotLoggedIn,
        }
    }
}

/// Decrypted profile of a friend-request sender, for the Received-requests
/// tab. All fields best-effort — `None` when the envelope couldn't be
/// decrypted or the field is absent. Produced by
/// [`UserService::ingest_received_friend_request`].
#[derive(Debug, Clone, Default)]
pub struct ReceivedFriendRequestProfile {
    pub display_name: Option<String>,
    pub avatar_bytes: Option<Vec<u8>>,
    pub note: Option<String>,
}

/// True when `q` is a complete Signal `nickname.discriminator` handle — the only
/// shape [`UserService::lookup_by_username`] sends to the server. Partial input
/// (a bare nickname, a dangling `.`) never reaches the existence oracle.
pub fn is_lookup_handle(q: &str) -> bool {
    match q.trim().rsplit_once('.') {
        Some((nick, disc)) => {
            !nick.is_empty() && !disc.is_empty() && disc.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
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
    /// When each profile was last fetched from the server this session.
    /// In memory on purpose: a restart should refetch.
    profile_fetched_at: Arc<parking_lot::Mutex<HashMap<UserId, Instant>>>,
}

/// Signal-Desktop's `throttledGetProfiles` window: opening a conversation
/// refetches the peer's profile at most once per this interval.
pub const PROFILE_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Whether a profile last fetched at `last` is due for a refetch at `now`.
fn profile_fetch_due(last: Option<Instant>, now: Instant) -> bool {
    last.map_or(true, |t| {
        now.saturating_duration_since(t) >= PROFILE_REFRESH_INTERVAL
    })
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
            profile_fetched_at: Arc::default(),
        }
    }

    /// Create a user client for network operations.
    fn user_client(&self) -> UserClient {
        self.grpc_pool.user_client()
    }
}

mod blocking;
mod codes;
mod friends;
mod local;
mod lookup;
mod profile;
pub(crate) use profile::restore_own_profile;
