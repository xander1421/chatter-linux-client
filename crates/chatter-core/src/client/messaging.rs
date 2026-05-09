//! messaging façade — thin delegates to services.

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
use crate::services::typing_service::TypingService;
use crate::services::user::UserService;
use crate::services::conversation::ConversationHandle;
use crate::state::{
    ActiveConversation, AuthState, ChatState, MessageState, OfflineState,
    OrgState, PrivacyState, UserState,
};
use crate::state::chat_state::PendingAttachment;
use crate::storage::settings_keys;
use crate::system::Directories;
use super::{ChatterClient, ClientError};

impl ChatterClient {
    // ── Messages façade ──────────────────────────────────────────────────

    pub fn get_messages_local(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::message::MessageError> {
        self.messages()
            .get_messages_local(channel_id, conversation_type, before, limit)
    }

    pub fn get_messages_local_asc(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::message::MessageError> {
        self.messages()
            .get_messages_local_asc(channel_id, conversation_type, before, limit)
    }

    pub fn get_message_by_sender_timestamp(
        &self,
        sender_id: &crate::domain::ids::UserId,
        sent_at: u64,
    ) -> Option<crate::domain::message::Message> {
        self.messages()
            .get_by_sender_timestamp(sender_id, sent_at)
            .ok()
            .flatten()
    }

    pub fn mark_conversation_read(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Result<(), crate::services::message::MessageError> {
        self.messages().mark_conversation_read(conversation_id, conversation_type)
    }

    pub fn classify_active_conversation(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        peer_id: Option<crate::domain::ids::UserId>,
    ) -> crate::state::ActiveConversation {
        self.messages().classify_active_conversation(channel_id, conversation_type, peer_id)
    }

    pub fn user_has_reacted(
        &self,
        sender_id: &crate::domain::ids::UserId,
        sent_at_ns: u64,
        emoji: &str,
    ) -> bool {
        self.messages().user_has_reacted(sender_id, sent_at_ns, emoji).unwrap_or(false)
    }

    pub fn get_message_local(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Option<crate::domain::message::Message> {
        self.messages().get_message_local(message_id).ok().flatten()
    }

    pub fn get_channel_local(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Option<crate::domain::channel::Channel> {
        self.channels().get_channel_local(channel_id).ok().flatten()
    }

    pub async fn list_channels_paged(
        &self,
        limit: u32,
        offset: u32,
        archived: Option<bool>,
    ) -> Result<Vec<crate::domain::channel::Channel>, crate::services::channel::ChannelError> {
        self.channels().list_channels(limit, offset, archived).await
    }

    pub fn get_channels_mru(&self) -> Vec<crate::domain::channel::Channel> {
        self.channels().get_channels_mru().unwrap_or_default()
    }

    pub fn list_channel_members_local(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Vec<crate::domain::channel::Membership> {
        self.channels().list_members_local(channel_id).unwrap_or_default()
    }

    pub async fn get_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<crate::domain::channel::Channel, crate::services::channel::ChannelError> {
        self.channels().get_channel(channel_id).await
    }

    /// Build a sidebar entry for a DM channel. Wraps peer-id detection,
    /// last-message lookup, "You: " prefixing, preview truncation, and
    /// local-time timestamp formatting — all of which used to live in the
    /// UI crate.
    pub fn get_dm_sidebar_preview(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> DmSidebarPreview {
        let peer_user_id = self
            .get_dm_peer_local(channel_id)
            .map(|id| id.to_string())
            .unwrap_or_default();

        let current_user_id = self.auth_state().get().current_user_id;

        let mut out = DmSidebarPreview {
            peer_user_id,
            ..Default::default()
        };
        if let Ok(messages) = self.get_messages_local(channel_id, conversation_type, None, 1) {
            if let Some(msg) = messages.first() {
                let is_own = current_user_id
                    .map(|id| msg.sender_id == Some(id))
                    .unwrap_or(false);
                if let Some(text) = msg.as_text() {
                    let trimmed = crate::domain::message::truncate_preview_text(text, 50);
                    out.last_message = if is_own {
                        format!("You: {}", trimmed)
                    } else {
                        trimmed
                    };
                }
                out.timestamp_display = msg
                    .created_at
                    .with_timezone(&chrono::Local)
                    .format("%H:%M")
                    .to_string();
            }
        }
        out
    }

    /// Enriched mention suggestion — everything the composer autocomplete
    /// needs without having to N+1 fetch profiles from the UI layer.
    pub async fn search_mention_suggestions(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        query: &str,
    ) -> Vec<MentionSuggestionInfo> {
        let query_lower = query.to_lowercase();
        let mut out: Vec<MentionSuggestionInfo> = Vec::new();

        // Broadcast mentions always surface first while they fuzzy-match.
        if "here".starts_with(&query_lower) {
            out.push(MentionSuggestionInfo {
                id: "here".into(),
                display: "@here".into(),
                kind: MentionSuggestionKind::Broadcast,
                has_avatar: false,
            });
        }
        if "all".starts_with(&query_lower) || "everyone".starts_with(&query_lower) {
            out.push(MentionSuggestionInfo {
                id: "all".into(),
                display: "@all".into(),
                kind: MentionSuggestionKind::Broadcast,
                has_avatar: false,
            });
        }

        let members = match self.channels().get_channel_members(channel_id, 50, 0).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "search_mention_suggestions: members fetch failed");
                return out;
            }
        };

        // Cap the per-member profile fetch to the first 20 members and short-
        // circuit once we have enough matches. UI cap is 8 total.
        for member in members.into_iter().take(20) {
            if out.len() >= 8 {
                break;
            }
            let profile = match self.users().get_user_profile(&member.user_id).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(
                        user = %member.user_id,
                        error = %e,
                        "search_mention_suggestions: profile fetch failed, falling back to id"
                    );
                    continue;
                }
            };
            let display_name = if profile.display_name.is_empty() {
                member.user_id.to_string()
            } else {
                profile.display_name.clone()
            };
            let user_id_str = member.user_id.to_string();
            let name_lower = display_name.to_lowercase();
            if name_lower.contains(&query_lower) || user_id_str.to_lowercase().contains(&query_lower) {
                out.push(MentionSuggestionInfo {
                    id: user_id_str,
                    display: format!("@{}", display_name),
                    kind: MentionSuggestionKind::User,
                    has_avatar: profile.avatar_url.is_some(),
                });
            }
        }

        out.truncate(8);
        out
    }

    pub async fn get_channel_members(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<crate::domain::channel::Membership>, crate::services::channel::ChannelError> {
        self.channels().get_channel_members(channel_id, limit, offset).await
    }

    pub async fn update_channel_member_role(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        new_role: crate::domain::channel::MemberRole,
    ) -> Result<crate::domain::channel::Membership, crate::services::channel::ChannelError> {
        self.channels().update_member_role(channel_id, user_id, new_role).await
    }

    pub async fn delete_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().delete_channel(channel_id).await
    }

    pub async fn invite_user_to_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        invitee_id: &crate::domain::ids::UserId,
        message: Option<&str>,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().invite_user(channel_id, invitee_id, message).await
    }

    pub async fn get_channel_with_memberships(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<crate::domain::channel::Channel, crate::services::channel::ChannelError> {
        self.channels().get_channel_with_memberships(channel_id).await
    }

    pub async fn search_channels_filtered(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
        type_filter: Vec<crate::domain::channel::ChannelType>,
    ) -> Result<Vec<crate::domain::channel::Channel>, crate::services::channel::ChannelError> {
        self.channels().search_channels(query, limit, offset, type_filter).await
    }

    pub fn list_thread_replies(
        &self,
        thread_root_id: &crate::domain::ids::MessageId,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::message::MessageError> {
        self.messages().list_for_thread(thread_root_id, before, limit)
    }

    pub fn search_messages_local(
        &self,
        query: &str,
        channel_id: Option<&crate::domain::ids::ChannelId>,
        limit: u32,
    ) -> Result<Vec<crate::storage::repositories::search_index::SearchResult>, crate::services::message::MessageError> {
        self.messages().search_local(query, channel_id, limit)
    }

    /// Send a channel/DM message (convenience façade over `MessageService::send`).
    #[allow(clippy::too_many_arguments)]
    pub async fn send_channel_message(
        &self,
        group_fanout: Option<&crate::mailbox::group::GroupFanout>,
        channel_id: &crate::domain::ids::ChannelId,
        text: &str,
        attachment_ids: Vec<crate::domain::ids::AttachmentId>,
        attachment_pointers: Vec<crate::proto::content_v1::AttachmentPointer>,
        thread_id: Option<&crate::domain::ids::MessageId>,
        mentioned_integrations: Vec<crate::domain::ids::IntegrationId>,
        targets_ai_bot: bool,
    ) -> Result<crate::domain::message::Message, crate::services::message::MessageError> {
        self.messages()
            .send(
                group_fanout,
                channel_id,
                text,
                attachment_ids,
                attachment_pointers,
                thread_id,
                mentioned_integrations,
                targets_ai_bot,
            )
            .await
    }
}

/// Pre-shaped sidebar entry for a DM channel.
///
/// UI should read this instead of duplicating peer-id detection + preview
/// formatting logic. Produced by [`ChatterClient::get_dm_sidebar_preview`].
#[derive(Debug, Clone, Default)]
pub struct DmSidebarPreview {
    /// User ID of the other participant (not us). Empty if can't resolve.
    pub peer_user_id: String,
    /// Last-message preview text. Already includes the "You: " prefix when
    /// we authored the latest message, and is truncated to sidebar width.
    pub last_message: String,
    /// Timestamp in local-time `HH:MM` format, or empty if no messages.
    pub timestamp_display: String,
}

/// UI-facing send result returned by [`ChatterClient::send_message_ui`].
///
/// Consolidates the post-send shaping that used to live in the UI send
/// pipeline: effective message id (synthetic for bootstrap), delivery
/// status string, sidebar preview with "You: " prefix + truncation,
/// local-time `HH:MM` stamp, attachments resolved by id, and the quoted
/// message's thumbnail bytes (if any).
///
/// UI code renders this struct directly — no `get_attachment` loops, no
/// preview string building, no "pending-{ts}" synthesis.
#[derive(Debug, Clone)]
pub struct SendMessageUiResult {
    /// Effective message id to render in the UI. Real message id when the
    /// send completed synchronously; `"pending-{sent_at}"` synthetic id
    /// when the send triggered a DM bootstrap that must finish first.
    pub message_id_effective: String,
    /// Server-assigned sent-at timestamp in nanoseconds.
    pub sent_at: u64,
    /// Local-time `HH:MM` string for message header rendering.
    pub timestamp_display: String,
    /// Sidebar preview with "You: " prefix, truncated to sidebar width.
    pub sidebar_preview: String,
    /// Either `"sent"` (delivered) or `"sending"` (DM bootstrap in flight).
    pub delivery_status: &'static str,
    /// True when a DM bootstrap was kicked off — UI should show the
    /// "Setting up secure channel..." composer placeholder.
    pub bootstrap_initiated: bool,
    /// Resolved attachments for the outgoing message. Empty during DM
    /// bootstrap (media can't flow until the key exchange completes).
    pub attachments: Vec<crate::domain::attachment::Attachment>,
    /// Raw thumbnail bytes for the quoted message (JPEG/PNG), if any.
    pub reply_thumbnail_bytes: Option<Vec<u8>>,
}

/// Enriched mention suggestion surfaced by
/// [`ChatterClient::search_mention_suggestions`].
#[derive(Debug, Clone)]
pub struct MentionSuggestionInfo {
    pub id: String,
    pub display: String,
    pub kind: MentionSuggestionKind,
    pub has_avatar: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionSuggestionKind {
    /// Real user in the channel.
    User,
    /// Broadcast mention (`@here`, `@all`, `@everyone`).
    Broadcast,
}

impl MentionSuggestionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Broadcast => "broadcast",
        }
    }
}
