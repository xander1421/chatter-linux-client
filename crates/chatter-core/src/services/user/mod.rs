//! User service.
//!
//! Profile, friends, blocking, and presence management.
//!
//! This module is organized into:
//! - Service struct and core methods (this file)
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
        let proto_profile = user_client.get_user_profile(&user_id.to_string()).await?;

        let mut profile = proto_to_domain_profile(&proto_profile)?;

        if *user_id == current_user {
            self.restore_own_profile(&mut profile, &proto_profile)?;
            self.user_repo.ensure_user_exists(user_id)?;
            self.user_repo.upsert_profile(&profile)?;
            self.profile_fetched_at
                .lock()
                .insert(*user_id, Instant::now());
            return Ok(profile);
        }

        // Decrypt with their profile key. The server value is always
        // ciphertext, so we never fall back to it. A miss (no key on this
        // device, or a stale one) keeps what we already resolved — this is the
        // on-open refresh (`refresh_profile_if_stale`), and writing the
        // placeholder here turned a known name into "User 1c242673" mid-session
        // everywhere the stored profile is read. Same rule as `list_friends`.
        let stored = self.user_repo.get_profile(user_id).ok().flatten();
        profile.display_name = decrypt_friend_display_name(
            &self.user_repo,
            &current_user,
            user_id,
            &proto_profile.display_name,
        )
        .or_else(|| {
            stored
                .as_ref()
                .map(|p| p.display_name.clone())
                .filter(|n| !n.is_empty())
        })
        .unwrap_or_else(|| friend_display_name_placeholder(user_id));

        // Decrypt status the same way; proto now carries ciphertext.
        profile.status_message = decrypt_friend_status(
            &self.user_repo,
            &current_user,
            user_id,
            &proto_profile.status_message,
        )
        .map(StatusMessage::new)
        .or_else(|| {
            // Only a decrypt miss keeps the old one; an empty value is a clear.
            (!proto_profile.status_message.is_empty())
                .then(|| stored.as_ref().and_then(|p| p.status_message.clone()))
                .flatten()
        });

        // Decrypt the avatar with the friend's profile key (if we have it).
        profile.avatar_bytes = self
            .try_decrypt_avatar(&current_user, user_id, &proto_profile.encrypted_avatar)
            .or_else(|| {
                (!proto_profile.encrypted_avatar.is_empty())
                    .then(|| stored.and_then(|p| p.avatar_bytes))
                    .flatten()
            });

        self.user_repo.ensure_user_exists(user_id)?;
        self.user_repo.upsert_profile(&profile)?;
        self.profile_fetched_at
            .lock()
            .insert(*user_id, Instant::now());

        Ok(profile)
    }

    /// Fill our OWN fetched profile with plaintext and restore the local copies
    /// the UI reads (`keys::DISPLAY_NAME`, `keys::AVATAR`).
    ///
    /// The server holds our name, status and avatar encrypted under OUR profile
    /// key (`update_user_profile` encrypts with `keys::PROFILE_KEY`), not under
    /// a friend key — so the friend path would find no key and render a
    /// placeholder. A new device has none of these locally (it gets the profile
    /// key from the SVR restore), which is why its own profile showed initials
    /// while friends saw the photo. A field that is empty or fails to decrypt
    /// keeps the local value rather than being cleared.
    fn restore_own_profile(
        &self,
        profile: &mut Profile,
        proto: &UserProfile,
    ) -> Result<(), UserError> {
        use crate::crypto::profile_crypto;
        use base64::Engine;

        let key: Option<[u8; 32]> = self
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)?
            .and_then(|b| b.try_into().ok());
        let b64 = |s: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(s)
                .ok()
                .filter(|b| !b.is_empty())
        };

        let name = key.as_ref().and_then(|k| {
            let bytes = b64(&proto.display_name)?;
            profile_crypto::decrypt_display_name(k, &bytes)
                .ok()
                .filter(|n| !n.is_empty())
        });
        let status = key.as_ref().and_then(|k| {
            let bytes = b64(&proto.status_message)?;
            profile_crypto::decrypt_status(k, &bytes)
                .ok()
                .filter(|s| !s.is_empty())
        });
        let avatar = key.as_ref().and_then(|k| {
            if proto.encrypted_avatar.is_empty() {
                return None;
            }
            profile_crypto::decrypt_avatar(k, &proto.encrypted_avatar).ok()
        });

        if let Some(ref n) = name {
            self.settings_repo.set_string(keys::DISPLAY_NAME, n)?;
        }
        if let Some(ref a) = avatar {
            self.settings_repo.set_bytes(keys::AVATAR, a)?;
        }

        let existing = self.user_repo.get_profile(&profile.user_id)?;
        profile.display_name = name
            .or_else(|| {
                self.settings_repo
                    .get_string(keys::DISPLAY_NAME)
                    .ok()
                    .flatten()
                    .filter(|n| !n.is_empty())
            })
            .unwrap_or_else(|| friend_display_name_placeholder(&profile.user_id));
        profile.status_message = status
            .map(StatusMessage::new)
            .or_else(|| existing.as_ref().and_then(|p| p.status_message.clone()));
        profile.avatar_bytes =
            avatar.or_else(|| self.settings_repo.get_bytes(keys::AVATAR).ok().flatten());
        Ok(())
    }

    /// Refetch a user's profile if this device has not fetched it within
    /// [`PROFILE_REFRESH_INTERVAL`]; `Ok(None)` when it is still fresh.
    ///
    /// A peer's name/bio/avatar edit does not change their profile key, so
    /// nothing pushes it to us. Signal-Desktop covers this by calling
    /// `throttledGetProfiles` (a 5-minute throttle) whenever a conversation is
    /// opened (`ts/state/ducks/conversations.preload.ts`); this is the same
    /// decision. Call it on DM open and profile open.
    pub async fn refresh_profile_if_stale(
        &self,
        user_id: &UserId,
    ) -> Result<Option<Profile>, UserError> {
        let last = self.profile_fetched_at.lock().get(user_id).copied();
        if !profile_fetch_due(last, Instant::now()) {
            return Ok(None);
        }
        self.get_user_profile(user_id).await.map(Some)
    }

    /// Try to decrypt a friend's avatar using their profile key.
    ///
    /// Returns Some(avatar_bytes) if we have their profile key and decryption
    /// succeeds; None if no key, empty blob, or decryption fails. Mirrors
    /// [`decrypt_friend_display_name`] but the avatar travels as raw bytes
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

        let profile_key = self
            .user_repo
            .get_friend_profile_key(current_user_id, friend_id)
            .ok()
            .flatten()?;

        crate::crypto::profile_crypto::decrypt_avatar(&profile_key, encrypted_avatar).ok()
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
            let encrypted =
                crate::crypto::profile_crypto::encrypt_display_name(&profile_key, name)?;
            Some(base64::engine::general_purpose::STANDARD.encode(encrypted))
        } else {
            None
        };

        // SECURITY: Encrypt status_message with profile_key if being updated —
        // same E2E treatment as display_name (was previously sent plaintext).
        let encrypted_status = if let Some(ref s) = status_message {
            let profile_key = profile_key.ok_or(UserError::NotLoggedIn)?;
            let encrypted = crate::crypto::profile_crypto::encrypt_status(&profile_key, &s.text)?;
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
        let (encrypted_avatar, own_avatar_bytes): (Vec<u8>, Option<Vec<u8>>) = match (
            profile_key,
            self.settings_repo.get_bytes(keys::AVATAR)?,
        ) {
            (Some(pk), Some(avatar)) if !avatar.is_empty() => {
                const AVATAR_TARGET_BYTES: usize = 64 * 1024;
                // Match the anon-DM avatar ceiling; the profile RPC rides a
                // normal gRPC stream (MB-scale limit), so the old 96 KB was
                // needlessly tight — a 512² WebP-lossless photo (~120-200 KB)
                // was being dropped. WebP-lossless of a 512² image never
                // approaches 1 MB, so this is a safety net, not a real gate.
                const ENCRYPTED_AVATAR_CAP: usize = crate::domain::anon_dm_init::MAX_AVATAR_SIZE;
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
            display_name: encrypted_display_name.unwrap_or_else(|| existing.display_name.clone()),
            // Encrypted when we're updating status; otherwise a throwaway value
            // (status_message is only in the update mask when actually changed).
            status_message: encrypted_status
                .or_else(|| existing.status_message.as_ref().map(|s| s.text.clone()))
                .unwrap_or_default(),
            presence: domain_presence_to_proto(existing.presence),
            encrypted_name: Vec::new(),
            encrypted_about: Vec::new(),
            encrypted_avatar: encrypted_avatar.clone(),
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

        // Only masked fields go up. The server reads an absent or empty mask
        // as "every field", which would upload the stored plaintext name and
        // status (refused since server 8feb3585) and clear the avatar — so
        // with nothing to change, nothing is sent.
        if paths.is_empty() {
            return Ok(existing);
        }
        let update_mask = prost_types::FieldMask { paths };

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

        // Store plaintext locally. When we ARE changing the name, use the
        // plaintext we just sent. When we're NOT (status/avatar-only update),
        // the ProfileUpdated event echoes the ENCRYPTED display_name, so never
        // persist proto_profile.display_name — keep our own plaintext name
        // (settings holds it decrypted since login/registration).
        profile.display_name = match display_name {
            Some(plaintext_name) => plaintext_name,
            None => self
                .settings_repo
                .get_string(keys::DISPLAY_NAME)
                .ok()
                .flatten()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| friend_display_name_placeholder(&profile.user_id)),
        };

        // Same for status: the echo carries the ENCRYPTED value. Keep the
        // plaintext we just set, else the existing local plaintext.
        profile.status_message = status_message.or(existing.status_message);

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
        note: Option<&str>,
    ) -> Result<Friendship, UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        // E2E friend-request envelope, ECIES-encrypted to the recipient's
        // long-term identity key (same primitive as anon-DM init). No shared
        // key exists pre-friendship, so we encrypt to their public identity
        // key; only they can decrypt. Always sent — it carries OUR profile key
        // so the recipient can render our real name + avatar before accepting.
        // The optional note rides inside the same envelope.
        let envelope = self
            .encrypt_friend_request_envelope(friend_id, note.map(str::trim).unwrap_or(""))
            .await?;

        let mut user_client = self.user_client();
        let friend_id_str = friend_id.to_string();
        let mut stream = user_client
            .send_friend_request(&friend_id_str, Some(&envelope))
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

        // A peer known only by friend code has no users row yet, so without
        // this the pending insert FK-failed silently.
        self.user_repo.ensure_user_exists(friend_id).ok();
        let _ = self.user_repo.insert_friendship(&friendship);

        Ok(friendship)
    }

    /// Build + ECIES-encrypt the friend-request envelope to `recipient`'s
    /// long-term identity key. Reuses the anon-DM init ECIES primitive; server
    /// never sees plaintext.
    ///
    /// Plaintext layout: `has_key (1) || [profile_key (32)] || note (UTF-8,
    /// may be empty)`. The profile key lets the recipient decrypt our display
    /// name + avatar before accepting (Signal-style first-contact profile
    /// reveal); it rides only when the visibility setting lets a non-friend
    /// hold it ([`profile_visibility::shareable_profile_key`]).
    ///
    /// [`profile_visibility::shareable_profile_key`]: crate::services::profile_visibility::shareable_profile_key
    async fn encrypt_friend_request_envelope(
        &self,
        recipient: &UserId,
        note: &str,
    ) -> Result<Vec<u8>, UserError> {
        use crate::crypto::ecies;

        let profile_key = crate::services::profile_visibility::shareable_profile_key(
            &self.settings_repo,
            &self.user_repo,
            recipient,
        );

        // All-devices fetch: the identity key is account-wide, but a peer's
        // live device ids are unknown here (device 1 may be a retired
        // install), so never name one.
        let resp = self
            .grpc_pool
            .auth_client()
            .get_pre_key_bundles_all_devices(&recipient.to_string())
            .await?;
        let identity_key =
            libsignal_protocol::IdentityKey::decode(&friend_request_identity_key(resp)?)
                .map_err(|e| UserError::InvalidInput(format!("identity key decode failed: {e}")))?;

        let mut plaintext = Vec::with_capacity(1 + 32 + note.len());
        plaintext.push(u8::from(profile_key.is_some()));
        if let Some(key) = profile_key.as_ref() {
            plaintext.extend_from_slice(key);
        }
        plaintext.extend_from_slice(note.as_bytes());

        ecies::seal_to_identity(
            &identity_key,
            ecies::IdentityHpkeUse::FriendRequest,
            &plaintext,
        )
        .map_err(|e| UserError::InvalidInput(format!("envelope encryption failed: {e}")))
    }

    /// Resolve the sender's profile from an opened friend-request envelope.
    /// Protocol logic (profile-key extraction, key persistence, profile
    /// decrypt) lives here in the service; the facade opens the envelope in
    /// the Signal actor (the identity secret never leaves it) and delegates.
    ///
    /// `plaintext` = `has_key (1) || [profile_key (32)] || note (UTF-8)`
    /// (see `encrypt_friend_request_envelope`); `None` when there was no
    /// envelope or it would not open. Everything is best-effort: unopened or
    /// malformed → all-`None` (never surface ciphertext).
    pub async fn ingest_received_friend_request(
        &self,
        sender_id: &UserId,
        plaintext: Option<Vec<u8>>,
    ) -> ReceivedFriendRequestProfile {
        use zeroize::Zeroize;

        let mut out = ReceivedFriendRequestProfile::default();
        let Some(plaintext) = plaintext else {
            return out;
        };
        let (profile_key, note) = match plaintext.split_first() {
            Some((1, rest)) if rest.len() >= 32 => {
                let (key, note) = rest.split_at(32);
                (Some(<[u8; 32]>::try_from(key).expect("32 bytes")), note)
            }
            Some((0, note)) => (None, note), // their visibility setting withheld it
            _ => return out,                 // malformed
        };
        out.note = String::from_utf8(note.to_vec())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(mut profile_key) = profile_key else {
            return out;
        };

        // Persist the sender's key, then decrypt their real name + avatar.
        if self
            .store_friend_request_sender_key(sender_id, &profile_key)
            .is_ok()
        {
            if let Ok(profile) = self.get_user_profile(sender_id).await {
                out.display_name = Some(profile.display_name).filter(|s| !s.is_empty());
                out.avatar_bytes = profile.avatar_bytes;
            }
        }
        profile_key.zeroize();
        out
    }

    /// Persist a friend-request sender's profile key BEFORE we accept them, so
    /// their real display name + avatar can be decrypted for the Received tab.
    /// Also creates a `Pending` local friendship row keyed `(me, sender)`;
    /// accepting later updates the row's status.
    pub fn store_friend_request_sender_key(
        &self,
        sender: &UserId,
        profile_key: &[u8; 32],
    ) -> Result<(), UserError> {
        let current_user = self.settings_repo.current_user_id()?;
        self.user_repo.ensure_user_exists(sender)?;

        let pending = Friendship {
            requester_id: current_user,
            addressee_id: *sender,
            status: FriendshipStatus::Pending,
            created_at: Utc::now(),
        };
        // Row may already exist (repeated poll / prior request) — keep whatever
        // status it has.
        let _ = self.user_repo.insert_friendship(&pending);

        self.user_repo
            .set_friend_profile_key(&current_user, sender, profile_key)
            .map(|_| ())
            .map_err(UserError::Storage)
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
            if let Some(user_event::Event::FriendStatusChanged(ref status_response)) = event.event {
                let status = status_response.status();
                tracing::debug!("Friend response event: status={:?}", status);

                if status != FriendStatus::Pending {
                    tracing::info!("Friend response final status: {:?}", status);
                    break;
                }
            }
        }

        if accept {
            // Persist locally via the shared helper, which ensures the peer's
            // user row exists first (the friendships FK). A plain insert here
            // FK-failed silently when the accepter had no local user row for
            // the requester (added via friend code), leaving the DM out of the
            // sidebar.
            self.record_accepted_friend_local(friend_id)?;
            // Resolve their name before the caller rebuilds the sidebar from
            // storage, so the new DM row never starts as `User <id8>`
            // (OPEN-ITEMS §119). `get_user_profile` decrypts with the key the
            // request envelope carried and upserts; a miss keeps the stored
            // name. Best-effort: the friendship is already accepted.
            if let Err(e) = self.get_user_profile(friend_id).await {
                tracing::warn!(peer_id = %friend_id, error = %e, "Accept: friend profile fetch failed");
            }
            let friendship = Friendship {
                requester_id: current_user,
                addressee_id: *friend_id,
                status: FriendshipStatus::Accepted,
                created_at: Utc::now(),
            };
            Ok(FriendResponseResult {
                friendship: Some(friendship),
            })
        } else {
            let _ = self.user_repo.delete_friendship(&current_user, friend_id);
            Ok(FriendResponseResult { friendship: None })
        }
    }

    /// List accepted friends.
    pub async fn list_friends(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Friendship>, UserError> {
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

            // FK on users(id); then record the friendship without lifting a
            // block — the server lists blocked friends too.
            self.user_repo.ensure_user_exists(&friend_id).ok();
            let _ = self
                .user_repo
                .accept_friendship_unless_blocked(&current_user, &friend_id);

            // Build profile from the proto UserProfile already in the response.
            let mut profile = proto_to_domain_profile(friend)?;
            profile.display_name = decrypt_friend_display_name(
                &self.user_repo,
                &current_user,
                &friend_id,
                &friend.display_name,
            )
            // No key on this row (or the decrypt missed): keep the name we
            // already resolved, as `UserSync::sync_friends` does, rather than
            // write the placeholder over it.
            .or_else(|| {
                self.user_repo
                    .get_profile(&friend_id)
                    .ok()
                    .flatten()
                    .map(|stored| stored.display_name)
                    .filter(|n| !n.is_empty())
            })
            .unwrap_or_else(|| friend_display_name_placeholder(&friend_id));
            profile.status_message = decrypt_friend_status(
                &self.user_repo,
                &current_user,
                &friend_id,
                &friend.status_message,
            )
            .map(StatusMessage::new);
            // The server's copy is authoritative WHEN IT HAS ONE. It legitimately
            // may not: the friend never set an avatar, the response omits it, or
            // the decrypt misses. Falling through to None was not merely "no
            // picture in this list" — the `upsert_profile` below then wrote that
            // None straight over the locally stored avatar, the same row the DM
            // list reads. That is why one contact showed a photo in Chats and
            // bare initials in Contacts: opening Contacts erased it.
            profile.avatar_bytes = self
                .try_decrypt_avatar(&current_user, &friend_id, &friend.encrypted_avatar)
                .or_else(|| {
                    self.user_repo
                        .get_profile(&friend_id)
                        .ok()
                        .flatten()
                        .and_then(|stored| stored.avatar_bytes)
                });

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

    /// Usernames of accepted friends, best effort (local data only).
    /// Feeds the Jarvis app-context bridge.
    pub fn friend_usernames_local(&self) -> Vec<String> {
        let Ok(current) = self.settings_repo.current_user_id() else {
            return Vec::new();
        };
        let Ok(friends) = self.get_friends_local() else {
            return Vec::new();
        };
        friends
            .iter()
            .filter(|f| matches!(f.status, FriendshipStatus::Accepted))
            .filter_map(|f| {
                let other = if f.requester_id == current {
                    f.addressee_id
                } else {
                    f.requester_id
                };
                // A peer's local username can be a placeholder user id; never
                // hand that to Jarvis as a handle.
                self.user_repo
                    .get(&other)
                    .ok()
                    .flatten()
                    .map(|u| u.username)
                    .filter(|u| !u.is_empty() && u.parse::<uuid::Uuid>().is_err())
            })
            .take(20)
            .collect()
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
        // The friendships row has a FOREIGN KEY on users(id); the requester may
        // not have a local user row for a peer it only learned about via the
        // friend code. Ensure it exists before inserting (same as
        // list_friends_with_profiles does).
        self.user_repo.ensure_user_exists(friend_id).ok();
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

        // Drain to the end, as `unblock_user` does: the first event is the
        // PENDING sent before the block runs (friend.go `BlockUser`), and the
        // server closes the stream only once the block is committed (or with
        // the error). Breaking on that PENDING wrote the local Blocked row
        // before — or even when — the server block never happened.
        while let Some(event_result) = stream.next().await {
            event_result?;
        }

        let friendship = Friendship {
            requester_id: current_user,
            addressee_id: *user_id,
            status: FriendshipStatus::Blocked,
            created_at: Utc::now(),
        };

        // FK on users(id): a peer known only from a channel or DM has no
        // users row, so the insert failed silently and the Blocked tab stayed
        // empty until `sync_blocked_users` (which does this) ran on restart.
        self.user_repo.ensure_user_exists(user_id).ok();
        if self.user_repo.update_friendship(&friendship).is_err() {
            if let Err(e) = self.user_repo.insert_friendship(&friendship) {
                tracing::warn!(error = %e, "block_user: local block row not written");
            }
        }

        Ok(())
    }

    /// Unblock a user.
    pub async fn unblock_user(&self, user_id: &UserId) -> Result<(), UserError> {
        let current_user = self.settings_repo.current_user_id()?;

        let mut user_client = self.user_client();
        let user_id_str = user_id.to_string();
        let mut stream = user_client.unblock_user(&user_id_str).await?;

        // Drain to the end: the server's FIRST event is a PENDING sent before
        // it unblocks (friend.go `UnblockUser`), and it closes the stream only
        // once the unblock is committed (or with the error). Stopping at that
        // first event raced the commit: the friend-list check below still saw
        // the block, deleted the local row with the friend's profile key, and
        // the friend came back as "User 1c242673" with no DM row.
        while let Some(event_result) = stream.next().await {
            event_result?;
        }

        // The server kept the friendship through the block (Signal-style), and
        // locally the block is only the row's status. Flip it back rather than
        // delete the row: the row holds the friend's profile key, and deleting
        // it is what brought an unblocked friend back as "User 1c242673".
        let accepted = Friendship {
            requester_id: current_user,
            addressee_id: *user_id,
            status: FriendshipStatus::Accepted,
            created_at: Utc::now(),
        };
        let _ = self.user_repo.update_friendship(&accepted);

        // Someone who was never a friend has no business in the friends list.
        // judged on the first page only; with a full page (or
        // offline) the next `UserSync::sync_friends` prune removes the row.
        match self.list_friends_with_profiles(100, 0).await {
            Ok(friends)
                if friends.len() < 100
                    && !friends.iter().any(|(f, _)| f.addressee_id == *user_id) =>
            {
                let _ = self.user_repo.delete_friendship(&current_user, user_id);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "unblock_user: friend list not refreshed"),
        }

        Ok(())
    }

    // Pulling the server's block list into the local `friendships` table is
    // `UserSync::sync_blocked_users` (`sync/user_sync.rs`), which the sync
    // engine runs every cycle. A duplicate of it lived here with no caller at
    // all, so a block made on another device never reached this one.

    // ========================================================================
    // Local Query Methods
    // ========================================================================

    /// Get a user from local storage only.
    pub fn get_user_local(&self, user_id: &UserId) -> Result<Option<User>, UserError> {
        self.user_repo.get(user_id).map_err(UserError::Storage)
    }

    /// Get a user's profile from local storage only.
    /// Whether `user_id` is an accepted friend of the current user, from the
    /// local friendships table (either direction of the original request).
    pub fn is_friend_local(&self, user_id: &UserId) -> bool {
        use crate::domain::user::FriendshipStatus;
        let Ok(me) = self.settings_repo.current_user_id() else {
            return false;
        };
        [(me, *user_id), (*user_id, me)].iter().any(|(a, b)| {
            matches!(
                self.user_repo.get_friendship(a, b),
                Ok(Some(f)) if f.status == FriendshipStatus::Accepted
            )
        })
    }

    /// Server-side blocks this user made, from the durable friendships table
    /// (kept in step with the server by `UserSync::sync_blocked_users`).
    pub fn blocked_local(&self) -> Vec<UserId> {
        let Ok(me) = self.settings_repo.current_user_id() else {
            return Vec::new();
        };
        self.user_repo.list_blocked_by(&me).unwrap_or_default()
    }

    /// Whether this user blocked `user_id` server-side (durable table).
    pub fn is_blocked_local(&self, user_id: &UserId) -> bool {
        let Ok(me) = self.settings_repo.current_user_id() else {
            return false;
        };
        self.user_repo.is_blocked(&me, user_id).unwrap_or(false)
    }

    pub fn get_profile_local(&self, user_id: &UserId) -> Result<Option<Profile>, UserError> {
        self.user_repo
            .get_profile(user_id)
            .map_err(UserError::Storage)
    }

    /// Get a user's display name from local storage.
    ///
    /// Resolution order:
    /// 1. User directory profile (server DMs, channel members, contacts)
    /// 2. User directory username (any registered peer)
    /// 3. Anon DM peer (peer isn't in the user directory — stored on the
    ///    anon DM conversation row)
    /// 4. Anon group member (stored on the group's member row in any group)
    /// 5. The `User <id8>` placeholder every other screen shows
    pub fn get_display_name(&self, user_id: &UserId) -> String {
        self.get_display_names(std::slice::from_ref(user_id))
            .remove(user_id)
            .unwrap_or_else(|| friend_display_name_placeholder(user_id))
    }

    /// [`Self::get_display_name`] for many users at once: the profile name and
    /// the username of every id come from one reader query
    /// (`UserRepository::display_name_sources`); only an id that neither
    /// resolves walks the per-id anon fallbacks. Opening a conversation names
    /// all its senders through this.
    pub fn get_display_names(
        &self,
        user_ids: &[UserId],
    ) -> std::collections::HashMap<UserId, String> {
        let sources = self
            .user_repo
            .display_name_sources(user_ids)
            .unwrap_or_default();
        user_ids
            .iter()
            .map(|user_id| {
                let (profile_name, username) = sources.get(user_id).cloned().unwrap_or_default();
                (
                    *user_id,
                    self.resolve_display_name(user_id, profile_name, username),
                )
            })
            .collect()
    }

    fn resolve_display_name(
        &self,
        user_id: &UserId,
        profile_name: Option<String>,
        username: Option<String>,
    ) -> String {
        // A profile row written while we had no profile key holds the
        // placeholder itself; that is not a name, so keep resolving (a known
        // username beats `User 1c242673` in Contacts).
        let placeholder = friend_display_name_placeholder(user_id);
        if let Some(name) = profile_name {
            if !name.is_empty() && name != placeholder {
                return name;
            }
        }
        if let Some(username) = username {
            // `ensure_user_exists` stores the id itself as a placeholder
            // username; that is not a name, so keep resolving.
            if !username.is_empty() && username != user_id.to_string() {
                return username;
            }
        }
        if let Ok(Some(anon_dm)) = self.anon_dm_repo.get_live_conversation_by_peer(user_id) {
            if let Some(name) = anon_dm.peer_display_name {
                if !name.is_empty() {
                    return name;
                }
            }
        }
        // A blocked anon peer whose chat was deleted: the block row keeps
        // the name the conversation row carried.
        if let Ok(Some(name)) = self.anon_dm_repo.get_blocked_display_name(user_id) {
            if !name.is_empty() {
                return name;
            }
        }
        if let Ok(Some(name)) = self.anon_group_repo.find_display_name_for_user(user_id) {
            if !name.is_empty() {
                return name;
            }
        }
        placeholder
    }

    /// Get the current user's ID.
    pub fn current_user_id(&self) -> Result<UserId, UserError> {
        self.settings_repo
            .current_user_id()
            .map_err(UserError::from)
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
        // The lookup is an existence oracle. Only a complete handle may reach
        // the server — a caller that fires per keystroke must not turn it into
        // a prefix/enumeration probe.
        if !is_lookup_handle(username) {
            return Err(UserError::InvalidInput(
                "not a nickname.discriminator handle".into(),
            ));
        }
        let _ = self.settings_repo.current_user_id()?;
        let hash = crate::crypto::auth_crypto::hash_username_lookup(username)
            .map_err(|e| UserError::InvalidInput(format!("invalid username: {e}")))?;
        let mut user_client = self.user_client();
        match user_client.lookup_username(hash).await? {
            Some(id_str) => {
                let user_id = id_str.parse().ok();
                if let Some(id) = user_id.as_ref() {
                    if let Err(e) = self.user_repo.set_known_username(id, username) {
                        tracing::warn!(error = %e, "Could not remember looked-up peer handle");
                    }
                }
                Ok(user_id)
            }
            None => Ok(None),
        }
    }

    /// Read this account's "Who can find me by my username" setting from the
    /// server. `true` = Everyone, `false` = Nobody.
    pub async fn get_username_discoverability(&self) -> Result<bool, UserError> {
        let _ = self.settings_repo.current_user_id()?;
        let mut user_client = self.user_client();
        Ok(user_client.get_username_discoverability().await?)
    }

    /// Set this account's "Who can find me by my username" setting on the
    /// server. With `false` the server answers "no match" to everyone else's
    /// username lookup for this account; friend codes and username links
    /// still reach it. Returns the value the server persisted.
    pub async fn set_username_discoverability(
        &self,
        discoverable: bool,
    ) -> Result<bool, UserError> {
        let _ = self.settings_repo.current_user_id()?;
        let mut user_client = self.user_client();
        Ok(user_client
            .set_username_discoverability(discoverable)
            .await?)
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
    pub fn current_friend_code(&self) -> Result<String, UserError> {
        let current_user = self.settings_repo.current_user_id()?;
        Ok(format!("chatter://connect/{}", current_user))
    }

    pub fn generate_friend_code(&self) -> Result<String, UserError> {
        self.current_friend_code()
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
        let qr = QrCode::new(code_url.as_bytes())
            .map_err(|e| UserError::InvalidInput(format!("Failed to generate QR code: {}", e)))?;

        // Render to image with quiet zone
        let qr_image = qr.render::<Luma<u8>>().quiet_zone(true).build();

        // Scale to requested size
        let scaled =
            image::imageops::resize(&qr_image, size, size, image::imageops::FilterType::Nearest);

        // Encode as PNG
        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(scaled.as_raw(), size, size, image::ExtendedColorType::L8)
            .map_err(|e| UserError::InvalidInput(format!("Failed to encode PNG: {}", e)))?;

        Ok(png_bytes)
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
    pub fn current_private_friend_code(&self) -> Result<String, UserError> {
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

    pub fn generate_private_friend_code(&self) -> Result<String, UserError> {
        self.current_private_friend_code()
    }

    /// Generate a QR code for private friend code.
    ///
    /// Returns PNG image bytes encoding the private friend code URL.
    pub fn generate_private_friend_qr(&self, size: u32) -> Result<Vec<u8>, UserError> {
        use image::{ImageEncoder, Luma};
        use qrcode::QrCode;

        let code_url = self.generate_private_friend_code()?;

        // Generate QR code (private codes are longer, may need higher error correction)
        let qr = QrCode::with_error_correction_level(code_url.as_bytes(), qrcode::EcLevel::M)
            .map_err(|e| {
                UserError::InvalidInput(format!("Failed to generate private QR code: {}", e))
            })?;

        // Render to image with quiet zone
        let qr_image = qr.render::<Luma<u8>>().quiet_zone(true).build();

        // Scale to requested size
        let scaled =
            image::imageops::resize(&qr_image, size, size, image::imageops::FilterType::Nearest);

        // Encode as PNG
        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(scaled.as_raw(), size, size, image::ExtendedColorType::L8)
            .map_err(|e| UserError::InvalidInput(format!("Failed to encode PNG: {}", e)))?;

        Ok(png_bytes)
    }

    // ========================================================================
    // Username Link Operations (Signal-style, L6)
    // ========================================================================
    //
    // A username link is a shareable `chatter://u/{link_id}#{entropy}` URL/QR
    // that resolves to our username WITHOUT the server learning it: the blob
    // uploaded is libsignal `usernames::create_for_username` ciphertext, and
    // the decryption entropy only ever travels in the URL fragment. See
    // `crate::domain::username_link`.

    /// Create (or rotate) the current user's username link and return the
    /// shareable URL. Setting a new link replaces the old one server-side, so
    /// previously shared URLs stop resolving — that IS the reset button.
    pub async fn create_username_link(&self) -> Result<String, UserError> {
        let _ = self.settings_repo.current_user_id()?;
        let full_username = self
            .settings_repo
            .get_string(keys::FULL_USERNAME)?
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                // Shown to the person as-is (`user_message`), so it is copy.
                UserError::InvalidInput(
                    "your username isn't stored on this device, so a link can't be made here"
                        .to_string(),
                )
            })?;

        let (entropy, blob) = usernames::create_for_username(
            &mut rand::rngs::OsRng.unwrap_err(),
            full_username,
            None,
        )
            .map_err(|e| UserError::InvalidInput(format!("Failed to encrypt username: {e}")))?;

        let mut user_client = self.user_client();
        let link_id = user_client.set_username_link(blob).await?;

        let url = crate::domain::UsernameLink { link_id, entropy }.to_url();
        self.settings_repo
            .set_string(keys::USERNAME_LINK_URL, &url)?;
        Ok(url)
    }

    /// The locally stored username-link URL, if one was created on this
    /// device. `None` means "no active link" (never created or deleted).
    pub fn username_link_local(&self) -> Result<Option<String>, UserError> {
        Ok(self.settings_repo.get_string(keys::USERNAME_LINK_URL)?)
    }

    /// Render a username-link URL as a square PNG QR code.
    pub fn username_link_qr(url: &str, size: u32) -> Result<Vec<u8>, UserError> {
        use image::{ImageEncoder, Luma};
        use qrcode::QrCode;

        let qr = QrCode::new(url.as_bytes())
            .map_err(|e| UserError::InvalidInput(format!("Failed to generate QR code: {}", e)))?;
        let qr_image = qr.render::<Luma<u8>>().quiet_zone(true).build();
        let scaled =
            image::imageops::resize(&qr_image, size, size, image::imageops::FilterType::Nearest);
        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(scaled.as_raw(), size, size, image::ExtendedColorType::L8)
            .map_err(|e| UserError::InvalidInput(format!("Failed to encode PNG: {}", e)))?;
        Ok(png_bytes)
    }

    /// Resolve someone else's username link: parse the URL, fetch the
    /// ciphertext blob and the link owner's id by link id, and decrypt the
    /// blob locally with the fragment entropy. Returns the decrypted username
    /// and the owner's account id.
    ///
    /// The owner id comes from the link row, NOT from `LookupUsername`: a link
    /// is a capability like a friend code, so it must still reach an owner
    /// who set "Who can find me by my username" to Nobody. Decryption still
    /// runs first, so a URL with the wrong fragment fails as before.
    pub async fn resolve_username_link(&self, url: &str) -> Result<(String, UserId), UserError> {
        let link = crate::domain::UsernameLink::parse(url)
            .map_err(|e| UserError::InvalidInput(format!("Invalid username link: {e}")))?;

        let mut user_client = self.user_client();
        let (owner_id, blob) = user_client
            .get_username_link(&link.link_id)
            .await?
            .ok_or_else(|| {
                UserError::InvalidInput(
                    "This username link is no longer active (deleted or reset)".to_string(),
                )
            })?;

        let username = usernames::decrypt_username(&link.entropy, &blob).map_err(|_| {
            UserError::InvalidInput(
                "Could not decrypt this username link (corrupted or wrong key)".to_string(),
            )
        })?;

        let user_id = owner_id.parse::<UserId>().map_err(|_| {
            UserError::InvalidInput(format!("Invalid user ID in username link: {owner_id}"))
        })?;
        if let Err(e) = self.user_repo.set_known_username(&user_id, &username) {
            tracing::warn!(error = %e, "Could not remember linked peer handle");
        }
        Ok((username, user_id))
    }

    /// Delete the current user's username link (server row + local copy).
    pub async fn delete_username_link(&self) -> Result<(), UserError> {
        let _ = self.settings_repo.current_user_id()?;
        let mut user_client = self.user_client();
        user_client.delete_username_link().await?;
        self.settings_repo.delete(keys::USERNAME_LINK_URL)?;
        Ok(())
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
            return id_str
                .parse::<UserId>()
                .map_err(|_| UserError::InvalidInput("This code isn't valid.".into()));
        }

        // Try plain UUID
        code.parse::<UserId>()
            .map_err(|_| UserError::InvalidInput("This code isn't valid.".into()))
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
        // Username links (`chatter://u/…#…`) resolve to their owner via
        // fetch-link → local decrypt; plain friend codes carry the user id
        // directly. Neither goes through the discoverability-gated lookup.
        let friend_id = if crate::domain::UsernameLink::is_username_link(code) {
            self.resolve_username_link(code).await?.1
        } else {
            Self::parse_friend_code(code)?
        };

        // Check we're not adding ourselves
        let current_user = self.settings_repo.current_user_id()?;
        if friend_id == current_user {
            return Err(UserError::InvalidInput(
                "Cannot add yourself as a friend".to_string(),
            ));
        }

        // Check if already a friend. A local Pending is not checked: a
        // recipient's decline is never sent back, so it can be stale, and the
        // server rejects a request that really is still pending.
        if let Ok(Some(existing)) = self.user_repo.get_friendship(&current_user, &friend_id) {
            if existing.status == FriendshipStatus::Accepted {
                return Err(UserError::InvalidInput(
                    "Already friends with this user".to_string(),
                ));
            }
        }

        // Send friend request
        self.send_friend_request(&friend_id, message).await
    }
}
