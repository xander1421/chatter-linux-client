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

        // Decrypt with their profile key; no key → placeholder. The server
        // value is always ciphertext, so we never fall back to it.
        profile.display_name = decrypt_friend_display_name(
            &self.user_repo,
            &current_user,
            user_id,
            &proto_profile.display_name,
        )
        .unwrap_or_else(|| friend_display_name_placeholder(user_id));

        // Decrypt status the same way; proto now carries ciphertext.
        profile.status_message = decrypt_friend_status(
            &self.user_repo,
            &current_user,
            user_id,
            &proto_profile.status_message,
        )
        .map(StatusMessage::new);

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

        let profile_key = self.user_repo
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
            let encrypted = crate::crypto::profile_crypto::encrypt_display_name(&profile_key, name)?;
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
        let (encrypted_avatar, own_avatar_bytes): (Vec<u8>, Option<Vec<u8>>) =
            match (profile_key, self.settings_repo.get_bytes(keys::AVATAR)?) {
                (Some(pk), Some(avatar)) if !avatar.is_empty() => {
                    const AVATAR_TARGET_BYTES: usize = 64 * 1024;
                    // Match the anon-DM avatar ceiling; the profile RPC rides a
                    // normal gRPC stream (MB-scale limit), so the old 96 KB was
                    // needlessly tight — a 512² WebP-lossless photo (~120-200 KB)
                    // was being dropped. WebP-lossless of a 512² image never
                    // approaches 1 MB, so this is a safety net, not a real gate.
                    const ENCRYPTED_AVATAR_CAP: usize =
                        crate::domain::anon_dm_init::MAX_AVATAR_SIZE;
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
            // Encrypted when we're updating status; otherwise a throwaway value
            // (status_message is only in the update mask when actually changed).
            status_message: encrypted_status
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

        let _ = self.user_repo.insert_friendship(&friendship);

        Ok(friendship)
    }

    /// Build + ECIES-encrypt the friend-request envelope to `recipient`'s
    /// long-term identity key. Reuses the anon-DM init ECIES primitive; server
    /// never sees plaintext.
    ///
    /// Plaintext layout: `profile_key (32 bytes) || note (UTF-8, may be empty)`.
    /// The profile key lets the recipient decrypt our display name + avatar
    /// before accepting (Signal-style first-contact profile reveal).
    async fn encrypt_friend_request_envelope(
        &self,
        recipient: &UserId,
        note: &str,
    ) -> Result<Vec<u8>, UserError> {
        use crate::crypto::ecies;
        use crate::proto::chat_v1::get_pre_key_bundle_response::Result as BundleResult;
        use crate::storage::repositories::settings_repo::keys;

        let profile_key = self
            .settings_repo
            .get_bytes(keys::PROFILE_KEY)?
            .filter(|k| k.len() == 32)
            .ok_or_else(|| UserError::InvalidInput("missing profile key".to_string()))?;

        let resp = self
            .grpc_pool
            .auth_client()
            .get_pre_key_bundle(&recipient.to_string(), "1")
            .await
            .map_err(|e| UserError::InvalidInput(format!("prekey fetch failed: {e}")))?;

        let identity_key = match resp.result {
            Some(BundleResult::Bundle(b)) => b.identity_key,
            Some(BundleResult::Devices(d)) => d.identity_key,
            None => {
                return Err(UserError::InvalidInput(
                    "empty prekey bundle response".to_string(),
                ))
            }
        };

        let x25519_pub = ecies::signal_identity_to_x25519(&identity_key)
            .map_err(|e| UserError::InvalidInput(format!("identity key convert failed: {e}")))?;

        let mut plaintext = Vec::with_capacity(32 + note.len());
        plaintext.extend_from_slice(&profile_key);
        plaintext.extend_from_slice(note.as_bytes());

        let msg = ecies::encrypt(&x25519_pub, &plaintext)
            .map_err(|e| UserError::InvalidInput(format!("envelope encryption failed: {e}")))?;

        Ok(msg.to_bytes())
    }

    /// Decrypt an incoming friend-request envelope and resolve the sender's
    /// profile. Protocol logic (ECIES decrypt with our identity key, profile-key
    /// extraction, key persistence, profile decrypt) lives here in the service;
    /// the facade only fetches `identity_secret` and delegates.
    ///
    /// Envelope plaintext = `profile_key (32 bytes) || note (UTF-8)`. Everything
    /// is best-effort: undecryptable/empty → all-`None` (never surface
    /// ciphertext). The caller owns zeroizing `identity_secret`.
    pub async fn ingest_received_friend_request(
        &self,
        sender_id: &UserId,
        ciphertext: &[u8],
        identity_secret: &[u8; 32],
    ) -> ReceivedFriendRequestProfile {
        use crate::crypto::ecies;
        use zeroize::Zeroize;

        let mut out = ReceivedFriendRequestProfile::default();
        if ciphertext.is_empty() {
            return out;
        }

        let Some(plaintext) = ecies::EciesMessage::from_bytes(ciphertext)
            .ok()
            .and_then(|m| ecies::decrypt(identity_secret, &m).ok())
        else {
            return out;
        };
        if plaintext.len() < 32 {
            return out; // malformed — must at least carry the profile key
        }

        let mut profile_key = [0u8; 32];
        profile_key.copy_from_slice(&plaintext[..32]);
        out.note = String::from_utf8(plaintext[32..].to_vec())
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // Persist the sender's key, then decrypt their real name + avatar.
        if self.store_friend_request_sender_key(sender_id, &profile_key).is_ok() {
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
    /// Creates a `Pending` local friendship row keyed `(me, sender)` (the
    /// friend_profile_key column lives on that row); accepting later updates
    /// the row's status and preserves the key.
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
        // status it has, we only need the row present for the key UPDATE.
        let _ = self.user_repo.insert_friendship(&pending);

        self.user_repo
            .set_friend_profile_key(&current_user, sender, profile_key)
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
            // Persist locally via the shared helper, which ensures the peer's
            // user row exists first (the friendships FK). A plain insert here
            // FK-failed silently when the accepter had no local user row for
            // the requester (added via friend code), leaving the DM out of the
            // sidebar.
            self.record_accepted_friend_local(friend_id)?;
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
            profile.display_name = decrypt_friend_display_name(
                &self.user_repo,
                &current_user,
                &friend_id,
                &friend.display_name,
            )
            .unwrap_or_else(|| friend_display_name_placeholder(&friend_id));
            profile.status_message = decrypt_friend_status(
                &self.user_repo,
                &current_user,
                &friend_id,
                &friend.status_message,
            )
            .map(StatusMessage::new);
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
                self.user_repo.get(&other).ok().flatten().map(|u| u.username)
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
                UserError::InvalidInput(
                    "No username registered on this device — cannot create a username link"
                        .to_string(),
                )
            })?;

        let (entropy, blob) =
            usernames::create_for_username(&mut rand::rng(), full_username, None).map_err(
                |e| UserError::InvalidInput(format!("Failed to encrypt username: {e}")),
            )?;

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

        let qr = QrCode::new(url.as_bytes()).map_err(|e| {
            UserError::InvalidInput(format!("Failed to generate QR code: {}", e))
        })?;
        let qr_image = qr.render::<Luma<u8>>().quiet_zone(true).build();
        let scaled = image::imageops::resize(
            &qr_image,
            size,
            size,
            image::imageops::FilterType::Nearest,
        );
        let mut png_bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_bytes);
        encoder
            .write_image(scaled.as_raw(), size, size, image::ExtendedColorType::L8)
            .map_err(|e| UserError::InvalidInput(format!("Failed to encode PNG: {}", e)))?;
        Ok(png_bytes)
    }

    /// Resolve someone else's username link: parse the URL, fetch the
    /// ciphertext blob by link id, decrypt it locally with the fragment
    /// entropy, then discover the account through the existing hashed
    /// `LookupUsername` path. Returns the decrypted username and the account
    /// id (`None` when the username no longer maps to an account).
    pub async fn resolve_username_link(
        &self,
        url: &str,
    ) -> Result<(String, Option<UserId>), UserError> {
        let link = crate::domain::UsernameLink::parse(url)
            .map_err(|e| UserError::InvalidInput(format!("Invalid username link: {e}")))?;

        let mut user_client = self.user_client();
        let blob = user_client
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

        let user_id = self.lookup_by_username(&username).await?;
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
        // Username links (`chatter://u/…#…`) resolve to an account via
        // fetch-blob → local decrypt → hashed lookup; plain friend codes
        // carry the user id directly.
        let friend_id = if crate::domain::UsernameLink::is_username_link(code) {
            let (username, id) = self.resolve_username_link(code).await?;
            id.ok_or_else(|| {
                UserError::InvalidInput(format!("No account found for @{username}"))
            })?
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
