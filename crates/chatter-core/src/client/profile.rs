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

    pub fn get_user_local(
        &self,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<crate::domain::user::User> {
        self.users().get_user_local(user_id).ok().flatten()
    }

    pub fn generate_friend_code(&self) -> Result<String, crate::services::UserError> {
        self.users().generate_friend_code()
    }

    pub fn generate_private_friend_code(&self) -> Result<String, crate::services::UserError> {
        self.users().generate_private_friend_code()
    }

    pub fn generate_friend_qr(&self, size: u32) -> Result<Vec<u8>, crate::services::UserError> {
        self.users().generate_friend_qr(size)
    }

    pub fn generate_private_friend_qr(&self, size: u32) -> Result<Vec<u8>, crate::services::UserError> {
        self.users().generate_private_friend_qr(size)
    }

    pub async fn connect_via_friend_code(
        self: &Arc<Self>,
        code: &str,
        message: Option<&str>,
    ) -> Result<crate::domain::user::Friendship, crate::services::UserError> {
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

    pub async fn update_user_profile(
        &self,
        display_name: Option<String>,
        status_message: Option<crate::domain::user::StatusMessage>,
    ) -> Result<crate::domain::user::Profile, crate::services::UserError> {
        self.users().update_user_profile(display_name, status_message).await
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

    pub fn get_anon_dm_blocked_users(&self) -> Vec<crate::domain::ids::UserId> {
        self.anon_dms().get_blocked_users().unwrap_or_default()
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

    /// Check if a profile identifier is blocked (anon or server-side).
    pub fn is_profile_user_blocked(&self, id: &crate::domain::ids::UserId) -> bool {
        // Anon: check by peer ID
        if self.get_anon_dm_blocked_users().contains(id) {
            return true;
        }
        // Anon: check by conversation ID
        let conv_id = crate::domain::anon_dm::AnonDmId::from_uuid(id.0);
        if self.anon_dms().is_conversation_blocked(&conv_id).unwrap_or(false) {
            return true;
        }
        // Server-side
        self.ui.user_state.is_blocked(id)
    }

    pub async fn send_anon_dm_presence_update(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        presence: crate::domain::user::PresenceStatus,
    ) -> Result<(), crate::services::anon_dm::AnonDmError> {
        self.anon_dms().send_presence_update(conversation_id, presence).await
    }

    pub async fn download_and_store_anon_dm_avatar(
        &self,
        conversation_id: &crate::domain::anon_dm::AnonDmId,
        url: &str,
    ) -> Result<Vec<u8>, crate::services::anon_dm::AnonDmError> {
        self.anon_dms().download_and_store_avatar(conversation_id, url).await
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

    pub async fn upload_channel_icon(
        &self,
        path: &std::path::Path,
    ) -> Result<String, crate::services::attachment::AttachmentError> {
        self.attachments().upload_channel_icon(path).await
    }

    pub fn get_channel_icon_pixels(
        &self,
        cdn_key: &str,
    ) -> Option<crate::services::attachment::DecodedImagePixels> {
        self.attachments().get_channel_icon_pixels(cdn_key)
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
