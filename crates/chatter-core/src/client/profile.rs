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
use crate::services::jarvis::JarvisService;
use crate::services::ai::LocalAIService;
use crate::services::mailbox_manager::MailboxManager;
use crate::services::message::MessageService;
use crate::services::pin_service::PinService;
use crate::services::session_service::{SessionError, SessionService};
use crate::services::signal_service::SignalService;
use crate::services::section_service::SectionService;
use crate::services::typing_service::TypingService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, JarvisState, MessageState, OfflineState,
    OrgState, PinnedState, PresenceState, PrivacyState, StreamingState, UIState, UserState,
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
        avatar_url: Option<String>,
        status_message: Option<crate::domain::user::StatusMessage>,
    ) -> Result<crate::domain::user::Profile, crate::services::UserError> {
        self.users().update_user_profile(display_name, avatar_url, status_message).await
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
