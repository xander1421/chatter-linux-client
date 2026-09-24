//! profile façade — thin delegates to services.

#![allow(unused_imports)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::app::context::{AppContext, ContextError};
use crate::app::core_services::CoreServices;
use crate::domain::attachment::Attachment;
use crate::domain::channel::{Channel, ChannelType};
use crate::domain::message::{ConversationType, Message};
use crate::error::NetworkError;
use crate::events::{DomainEvent, EventBus};
use crate::services::auth_service::{AuthError, LoginResult};
use crate::inbox::DeviceInboxProvider;
use crate::mailbox::group::GroupFanout;
use crate::proto::content_v1::AttachmentPointer;
use crate::services::ai_service::AIService;
use crate::services::anon_dm::AnonDmService;
use crate::services::anon_group::AnonGroupService;
use crate::services::attachment::AttachmentService;
use crate::services::auth_service::AuthService;
#[cfg(feature = "media")]
use crate::services::call_worker::CallWorkerHandle;
use crate::services::channel::ChannelService;
use crate::services::dm_bootstrap_service::DmBootstrapService;
use crate::services::gateway::outgoing::{
    OwnedOutgoingServices, OutgoingIntent, QuoteIntent, SendError, SendResult,
    route_outgoing_owned,
};
use crate::services::ai::LocalAIService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::pin_service::PinService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal_service::SignalService;
use crate::services::section_service::SectionService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, MessageState, OfflineState,
    PrivacyState, UserState,
};
use crate::state::chat_state::PendingAttachment;
use crate::storage::settings_keys;
use crate::system::Directories;
use super::{ChatterClient, ClientError};

impl ChatterClient {
    // ── Display / profile façade ─────────────────────────────────────────

    /// Resolve a user's display name (checks profile → username → anon DM
    /// peer → anon group member → UUID fallback).
    pub fn display_name(&self, user_id: &crate::domain::ids::UserId) -> String {
        self.users().get_display_name(user_id)
    }

    /// Resolve a display name *within a conversation*, so anonymous channels
    /// never leak the global profile name. In an anon group the per-group
    /// member name wins; everywhere else this is just `display_name`.
    ///
    /// `display_name` alone prefers the global profile, which defeats
    /// anonymity when the sender is also a known contact — this is what the
    /// message sender and reply-quote author must use in anon conversations.
    pub fn display_name_in(
        &self,
        conv_type: &crate::domain::message::ConversationType,
        conv_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
    ) -> String {
        if *conv_type == crate::domain::message::ConversationType::AnonGroup {
            let group_id =
                crate::domain::anon_group::AnonGroupId::from_uuid(*conv_id.as_uuid());
            if let Some(name) = self
                .get_anon_group_member_display_name(&group_id, user_id)
                .filter(|n| !n.is_empty())
            {
                return name;
            }
        }
        self.display_name(user_id)
    }

    /// Our own avatar bytes (raw image data stored in settings).
    pub fn my_avatar(&self) -> Option<Vec<u8>> {
        self.anon_dms().get_my_avatar()
    }

    pub async fn list_friends(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<crate::domain::user::Friendship>, crate::services::UserError> {
        self.users().list_friends(limit, offset).await
    }

    pub fn get_profile_local(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::user::Profile> {
        self.users().get_profile_local(user_id).ok().flatten()
    }

    /// A peer's handle, if this device actually knows it. The local user
    /// directory fills `username` with the user id when the handle is unknown
    /// (the server stores only a lookup hash), and that id must not be shown
    /// as "@username".
    pub fn known_username(&self, user_id: &crate::domain::ids::UserId) -> Option<String> {
        self.get_user_local(user_id)
            .map(|u| u.username)
            .filter(|u| !u.is_empty() && u.parse::<uuid::Uuid>().is_err())
    }

    pub fn get_user_local(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::user::User> {
        self.users().get_user_local(user_id).ok().flatten()
    }

    /// Resolve a call participant's view data (display name, initials, avatar
    /// bytes) from local state, by LiveKit identity (a user_id string). The UI
    /// only decodes `avatar_bytes` to an image and renders — identity/profile
    /// resolution (decrypted name, the server-DM-profile-vs-anon-DM-row avatar
    /// fallback, initials) stays in core, not the UI. A non-user_id identity
    /// (unlikely) falls back to showing the raw identity.
    pub fn resolve_call_participant(
        &self,
        identity: &str,
    ) -> crate::ui::chat_data::CallParticipantInfo {
        use crate::ui::chat_data::{initials_from_name, CallParticipantInfo};
        match identity.parse::<crate::domain::ids::UserId>() {
            Ok(uid) => {
                let display_name = self.display_name(&uid);
                let avatar_bytes = self
                    .get_profile_local(&uid)
                    .and_then(|p| p.avatar_bytes)
                    .or_else(|| self.anon_dm_peer_avatar(&uid));
                CallParticipantInfo {
                    initials: initials_from_name(&display_name),
                    display_name,
                    avatar_bytes,
                }
            }
            Err(_) => CallParticipantInfo {
                initials: initials_from_name(identity),
                display_name: identity.to_string(),
                avatar_bytes: None,
            },
        }
    }

    pub fn generate_friend_code(&self) -> Result<String, crate::services::UserError> {
        self.users().generate_friend_code()
    }

    pub fn generate_private_friend_code(&self) -> Result<String, crate::services::UserError> {
        // Anon contact setup is only safe in shield mode — otherwise the code
        // generation/redemption and the resulting conversation run over the
        // identified/direct path, leaking what anon DM exists to hide.
        if !self.anon_mode_enabled() {
            return Err(crate::services::UserError::RequiresAnonMode);
        }
        self.users().generate_private_friend_code()
    }

    pub fn generate_friend_qr(&self, size: u32) -> Result<Vec<u8>, crate::services::UserError> {
        self.users().generate_friend_qr(size)
    }

    pub fn generate_private_friend_qr(&self, size: u32) -> Result<Vec<u8>, crate::services::UserError> {
        if !self.anon_mode_enabled() {
            return Err(crate::services::UserError::RequiresAnonMode);
        }
        self.users().generate_private_friend_qr(size)
    }

    pub async fn connect_via_friend_code(
        self: &Arc<Self>,
        code: &str,
        message: Option<&str>,
    ) -> Result<crate::domain::user::Friendship, crate::services::UserError> {
        // A regular friend code ends in an identified friend request; shield
        // mode redeems private codes (`start_anon_dm_conversation_private`).
        if self.anon_mode_enabled() {
            return Err(crate::services::UserError::ForbiddenInAnonMode);
        }
        let result = self.users().connect_via_friend_code(code, message).await?;
        let client = Arc::clone(self);
        tokio::spawn(async move {
            client.refresh_and_emit_friend_requests().await;
        });
        Ok(result)
    }

    pub async fn list_friend_requests(
        &self,
        filter: crate::services::user::FriendRequestFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<crate::domain::user::Friendship>, crate::services::UserError> {
        self.users().list_friend_requests(filter, limit, offset).await
    }

    /// Convenience: every friend request the server knows about.
    /// Equivalent to `list_friend_requests(FriendRequestFilter::All, 100, 0)`.
    /// Use the parameterised variant when you actually need pagination
    /// or a specific direction (Sent / Received / Pending / Accepted).
    pub async fn list_all_friend_requests(
        &self,
    ) -> Result<Vec<crate::domain::user::Friendship>, crate::services::UserError> {
        self.users().list_friend_requests(
            crate::services::user::FriendRequestFilter::All,
            100,
            0,
        ).await
    }

    /// Convenience: incoming-only friend requests.
    pub async fn list_pending_friend_requests(
        &self,
    ) -> Result<Vec<crate::domain::user::Friendship>, crate::services::UserError> {
        self.users().list_friend_requests(
            crate::services::user::FriendRequestFilter::Received,
            100,
            0,
        ).await
    }

    /// Update the current user's profile and push it to every peer who can't
    /// fetch it from the server: each ready anon DM, and the anon groups this
    /// user owns. The push runs in the background — a slow or failing peer
    /// never holds up (or fails) the save itself.
    pub async fn update_user_profile(
        self: &std::sync::Arc<Self>,
        display_name: Option<String>,
        status_message: Option<crate::domain::user::StatusMessage>,
    ) -> Result<crate::domain::user::Profile, crate::services::UserError> {
        let profile = self.users().update_user_profile(display_name, status_message).await?;

        let display_name = Some(profile.display_name.clone());
        let bio = profile.status_message.as_ref().map(|m| m.text.clone());
        for conv in self.list_anon_dm_conversations().into_iter().filter(|c| c.is_ready()) {
            let client = self.clone();
            let (display_name, bio) = (display_name.clone(), bio.clone());
            tokio::spawn(async move {
                // The avatar rides inside the update as encrypted bytes; there
                // is no plaintext avatar URL to send.
                if let Err(e) = client.send_anon_dm_profile_update(&conv.id, display_name, None, bio).await {
                    tracing::warn!(conversation = %conv.id, error = %e, "profile update to anon DM failed");
                }
            });
        }
        let client = self.clone();
        tokio::spawn(async move {
            if let Err(e) = client.broadcast_profile_to_owned_groups().await {
                tracing::warn!(error = %e, "profile update to owned anon groups failed");
            }
        });

        Ok(profile)
    }

    pub fn list_anon_dm_conversations(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().list_conversations().unwrap_or_default()
    }

    pub async fn start_anon_dm_conversation_private(
        &self,
        friend_code: &crate::domain::PrivateFriendCode,
        intro_message: Option<&str>,
        sender_display_name: Option<&str>,
    ) -> Result<crate::domain::anon_dm::AnonDm, crate::services::anon_dm::AnonDmError> {
        // Redeeming a private code + firing the ECIES init must ride the anon
        // transport (isolated Tor circuit, blind-token delivery). Outside shield
        // mode it would go over the identified/direct path — refuse.
        if !self.anon_mode_enabled() {
            return Err(crate::services::anon_dm::AnonDmError::RequiresAnonMode);
        }
        self.anon_dms().start_conversation_private(friend_code, intro_message, sender_display_name).await
    }

    pub fn list_anon_dm_ready_conversations(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().list_ready_conversations().unwrap_or_default()
    }

    pub fn list_anon_dm_message_requests(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().list_message_requests().unwrap_or_default()
    }

    pub fn list_anon_dm_outgoing_requests(&self) -> Vec<crate::domain::anon_dm::AnonDm> {
        self.anon_dms().list_outgoing_requests().unwrap_or_default()
    }

    pub fn get_latest_anon_dm_message(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Option<crate::domain::message::Message> {
        self.anon_dms().get_latest_message(conversation_id).ok().flatten()
    }

    /// Everyone this user blocked: anon-DM peers and server-side blocks, in
    /// one de-duplicated list. The Friends → Blocked tab renders exactly this.
    pub fn blocked_users(&self) -> Vec<crate::domain::ids::UserId> {
        let mut ids = self.anon_dms().get_blocked_users().unwrap_or_default();
        for id in self.users().blocked_local() {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids
    }

    pub fn block_anon_dm_conversation(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().block_conversation(conversation_id)
    }

    pub fn block_anon_dm_user(
        &self,
        user_id: &crate::domain::ids::UserId,
        reason: Option<&str>,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().block_user(user_id, reason)
    }

    /// Check if a user ID belongs to an anonymous DM peer. Returns the
    /// conversation ID if found.
    pub fn find_anon_dm_by_peer(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::anon_dm::AnonDmId> {
        self.anon_dms()
            .get_conversation_by_peer(user_id)
            .ok()
            .flatten()
            .map(|dm| dm.id)
    }

    /// Decrypted avatar bytes for an anonymous DM peer (the user directory has
    /// no profile for anon peers — their avatar lives on the anon DM row, same
    /// source the sidebar uses).
    pub fn anon_dm_peer_avatar(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<Vec<u8>> {
        self.anon_dms()
            .get_conversation_by_peer(user_id)
            .ok()
            .flatten()
            .and_then(|dm| dm.peer_avatar)
    }

    pub fn unblock_anon_dm_conversation(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().unblock_conversation(conversation_id)
    }

    pub fn unblock_anon_dm_user(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().unblock_user(user_id)
    }

    /// Try to interpret a UserId as an AnonDmId (conversation ID).
    /// Returns Some if a conversation with that ID exists.
    pub fn try_as_anon_dm_conversation_id(
        &self,
        maybe_conv_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::anon_dm::AnonDmId> {
        let conv_id = crate::domain::anon_dm::AnonDmId::from_uuid(maybe_conv_id.0);
        self.anon_dms()
            .get_conversation(&conv_id)
            .ok()
            .flatten()
            .map(|_| conv_id)
    }

    /// Get the peer user ID for an anon DM conversation.
    pub fn get_anon_dm_peer_id(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Option<crate::domain::ids::UserId> {
        self.anon_dms()
            .get_conversation(conversation_id)
            .ok()
            .flatten()
            .map(|dm| dm.peer_id)
    }

    /// Check if an anon DM conversation is blocked.
    pub fn is_anon_dm_conversation_blocked(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Option<bool> {
        self.anon_dms()
            .is_conversation_blocked(conversation_id)
            .ok()
    }

    // ── Unified block/unblock (routes anon vs server) ────────────────

    /// Resolve a profile identifier (may be a UserId or an AnonDmId
    /// stored as a UUID) into the correct blocking path and execute it.
    pub async fn block_profile_user(
        &self,
        id: &crate::domain::ids::UserId,
    ) -> Result<(), super::ClientError> {
        // 1. Check if id is a peer in an anon DM
        if let Some(conv_id) = self.find_anon_dm_by_peer(id) {
            self.anon_dms().block_conversation(&conv_id)
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            self.anon_dms().block_user(id, Some("Blocked from profile"))
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            return Ok(());
        }
        // 2. Check if id is actually a conversation ID (Friends tab uses dm.id)
        let conv_id = crate::domain::anon_dm::AnonDmId::from_uuid(id.0);
        if let Ok(Some(dm)) = self.anon_dms().get_conversation(&conv_id) {
            self.anon_dms().block_conversation(&conv_id)
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            self.anon_dms().block_user(&dm.peer_id, Some("Blocked from profile"))
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            return Ok(());
        }
        // 3. Server-side friendship block
        self.users().block_user(id).await
            .map_err(|e| super::ClientError::Inbox(e.to_string()))
    }

    /// Reverse of [`block_profile_user`]. Re-creates the ephemeral
    /// mailbox so polling resumes for the unblocked conversation.
    pub async fn unblock_profile_user(
        &self,
        id: &crate::domain::ids::UserId,
    ) -> Result<(), super::ClientError> {
        let conv_id = if let Some(cid) = self.find_anon_dm_by_peer(id) {
            self.anon_dms().unblock_user(id)
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            Some(cid)
        } else {
            let cid = crate::domain::anon_dm::AnonDmId::from_uuid(id.0);
            if let Ok(Some(dm)) = self.anon_dms().get_conversation(&cid) {
                self.anon_dms().unblock_user(&dm.peer_id)
                    .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
                Some(cid)
            } else {
                None
            }
        };

        if let Some(cid) = conv_id {
            self.anon_dms().unblock_conversation(&cid)
                .map_err(|e| super::ClientError::Inbox(e.to_string()))?;
            // The local mailbox keys were preserved during block (only
            // polling was suspended). Setting status back to Active is
            // enough — the poller picks up the existing keys. If the
            // server-side mailbox expired during the block period, the
            // poll will fail and the next renewal cycle handles it.
            return Ok(());
        }

        self.users().unblock_user(id).await
            .map_err(|e| super::ClientError::Inbox(e.to_string()))
    }

    /// Whether this user is an accepted friend (local friendships table).
    pub fn is_friend(&self, id: &crate::domain::ids::UserId) -> bool {
        self.users().is_friend_local(id)
    }

    /// Check if a profile identifier is blocked (anon or server-side).
    pub fn is_profile_user_blocked(&self, id: &crate::domain::ids::UserId) -> bool {
        // Anon: check by peer ID
        if self.anon_dms().is_user_blocked(id).unwrap_or(false) {
            return true;
        }
        // Anon: check by conversation ID
        let conv_id = crate::domain::anon_dm::AnonDmId::from_uuid(id.0);
        if self.anon_dms().is_conversation_blocked(&conv_id).unwrap_or(false) {
            return true;
        }
        // Server-side: the durable table. `user_state`'s in-memory set is never
        // populated, so it answered "not blocked" for every server block.
        self.users().is_blocked_local(id)
    }

    pub async fn send_anon_dm_presence_update(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        presence: crate::domain::user::PresenceStatus,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().send_presence_update(conversation_id, presence).await
    }

    pub async fn send_anon_dm_profile_update(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        display_name: Option<String>,
        avatar_url: Option<String>,
        bio: Option<String>,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms()
            .send_profile_update(conversation_id, display_name, avatar_url, bio)
            .await
    }

    pub fn get_anon_dm_messages_asc(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        before_sent_at: Option<u64>,
        limit: i32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().get_messages_asc(conversation_id, before_sent_at, limit)
    }

    pub fn list_anon_dm_pinned(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
    ) -> Vec<crate::domain::message::Message> {
        self.anon_dms().list_pinned(conversation_id).unwrap_or_default()
    }

    /// Look up a single member of an anonymous group.
    pub fn get_anon_group_member(
        &self,
        group_id: &crate::domain::anon_group::AnonGroupId,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::anon_group::AnonGroupMember> {
        self.anon_groups().get_member(group_id, user_id).ok().flatten()
    }

    /// Get an attachment record by id (local storage).
    pub fn get_attachment(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<crate::domain::attachment::Attachment> {
        self.attachments().get(attachment_id).ok().flatten()
    }

    pub fn get_attachment_metadata(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<crate::domain::attachment::Attachment> {
        self.attachments().get_metadata(attachment_id).ok().flatten()
    }

    pub fn decrypt_attachment_auto(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<crate::crypto::secure_buffer::SecureBuffer, crate::services::attachment::AttachmentError> {
        self.attachments().decrypt_auto(attachment_id)
    }

    /// Open a random-access decryptor for streaming playback (video/audio).
    ///
    /// Authenticates the whole file, then serves decrypted ranges on demand so
    /// the player starts without a full decrypt-to-memory. See
    /// `AttachmentService::open_range_decryptor`.
    pub fn open_attachment_range(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<crate::services::attachment::CbcRangeDecryptor, crate::services::attachment::AttachmentError> {
        self.attachments().open_range_decryptor(attachment_id)
    }

    pub async fn download_attachment_to_filesystem(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<(), crate::services::attachment::AttachmentError> {
        self.attachments().download_to_filesystem(attachment_id).await
    }

    pub fn decrypt_and_decode_gif_first_frame(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<Option<crate::services::attachment::GifDecodeResult>, crate::services::attachment::AttachmentError> {
        self.attachments().decrypt_and_decode_gif_first_frame(attachment_id)
    }

    pub fn decrypt_and_get_image_info(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<crate::services::attachment::ImageDecodeResult, crate::services::attachment::AttachmentError> {
        self.attachments().decrypt_and_get_image_info(attachment_id)
    }

    /// Decrypt a video attachment and extract a thumbnail frame.
    ///
    /// Blocking (GStreamer) — call from a background thread. Used to derive a
    /// thumbnail for server-DM videos, which arrive without inline thumbnails.
    pub fn decrypt_and_extract_video_thumbnail(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Result<crate::services::attachment::ImageDecodeResult, crate::services::attachment::AttachmentError> {
        self.attachments().decrypt_and_extract_video_thumbnail(attachment_id)
    }

    /// Decode every frame of a GIF from already-decrypted bytes.
    ///
    /// Blocking — call from a background thread (e.g. `spawn_blocking`).
    /// Thin façade over [`AttachmentService::decode_gif_all_frames`] so the UI
    /// never has to reach into the service directly.
    pub fn decode_gif_all_frames(
        &self,
        gif_bytes: &[u8],
    ) -> Result<crate::services::attachment::GifFullDecodeResult, crate::services::attachment::AttachmentError> {
        crate::services::attachment::AttachmentService::decode_gif_all_frames(gif_bytes)
    }

    pub fn update_attachment(
        &self,
        attachment: &crate::domain::attachment::Attachment,
    ) -> Result<(), crate::services::attachment::AttachmentError> {
        self.attachments().update(attachment)
    }

    pub fn get_attachment_preview_image(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<(Vec<u8>, u32, u32)> {
        self.attachments().get_preview_image(attachment_id)
    }

    /// Fetch + decrypt the thumbnail sidecar for a received attachment, caching
    /// the bytes locally. Gives a preview without downloading the full blob.
    pub async fn ensure_attachment_thumbnail(
        &self,
        attachment_id: &crate::domain::ids::AttachmentId,
    ) -> Option<Vec<u8>> {
        self.attachments().ensure_thumbnail(attachment_id).await
    }

    pub async fn upload_attachment_to_cdn_encrypted(
        &self,
        path: &std::path::Path,
    ) -> Result<crate::services::attachment::CdnUploadResult, crate::services::attachment::AttachmentError> {
        self.attachments().upload_to_cdn_encrypted(path).await
    }

    pub async fn download_url_bytes(
        &self,
        url: &str,
    ) -> Result<Vec<u8>, crate::services::attachment::AttachmentError> {
        self.attachments().download_url_bytes(url).await
    }

    pub async fn download_and_upload_gif(
        &self,
        url: &str,
        gif_id: &str,
    ) -> Result<crate::services::attachment::CdnUploadResult, crate::services::attachment::AttachmentError> {
        self.attachments().download_and_upload_gif(url, gif_id).await
    }
}
