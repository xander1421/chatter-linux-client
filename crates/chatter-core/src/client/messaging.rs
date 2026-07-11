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

    /// Whether the parent message a reply quotes is missing or deleted.
    ///
    /// Signal-Desktop parity: drives the "Original message not found" warning
    /// under a reply's quoted snapshot. See `MessageService::reply_parent_missing`.
    pub fn reply_parent_missing(&self, parent: &crate::domain::ids::MessageId) -> bool {
        self.messages().reply_parent_missing(parent)
    }

    // ── Resolved display fields (all business rules live here, not in the UI) ──
    //
    // The presentation layer copies these straight onto its display struct and
    // must not branch on conversation type or re-resolve names itself.

    /// Sender display name, scoped to the conversation so anon channels show
    /// the per-channel name instead of the global profile. `None` sender →
    /// "Anonymous".
    pub fn message_sender_display_name(&self, msg: &crate::domain::message::Message) -> String {
        match msg.sender_id {
            Some(ref id) => self.display_name_in(&msg.conversation_type, &msg.conversation_id, id),
            None => "Anonymous".to_string(),
        }
    }

    /// Reply-quote author display name, scoped like the sender. Empty when the
    /// message is not a reply or carries no quoted author.
    pub fn reply_author_display_name(&self, msg: &crate::domain::message::Message) -> String {
        match msg.reply_to_author_id {
            Some(ref id) => self.display_name_in(&msg.conversation_type, &msg.conversation_id, id),
            None => String::new(),
        }
    }

    /// Whether to show the "Original message not found" warning for a reply.
    ///
    /// Anon replies carry a self-contained quote snapshot and reference their
    /// parent via a synthetic timestamp-derived id that never matches the
    /// parent's stored id — so the by-id lookup is meaningless and we never
    /// warn. Non-anon replies use the real parent lookup.
    pub fn reply_parent_missing_for(&self, msg: &crate::domain::message::Message) -> bool {
        match msg.reply_to {
            Some(ref parent) if !msg.conversation_type.is_anon() => self.reply_parent_missing(parent),
            _ => false,
        }
    }

    /// Media descriptor of a reply's quoted parent: `(category, attachment_id,
    /// duration)`. Lets the quote bubble render the parent's media richly (an
    /// inline audio player for audio parents) instead of plain text.
    ///
    /// `category` is `"audio"`/`"image"`/`"video"`; `duration` is a formatted
    /// `"m:ss"` for audio, else empty. All fields are empty when the message
    /// is not a reply, the parent or its first attachment can't be resolved
    /// locally, or the parent carries no renderable media — the quote then
    /// falls back to its text/thumbnail snapshot with no inline player.
    pub fn reply_parent_media(
        &self,
        msg: &crate::domain::message::Message,
    ) -> (String, String, String) {
        use crate::domain::attachment::FileCategory;
        let empty = || (String::new(), String::new(), String::new());

        let Some(parent_id) = msg.reply_to.as_ref() else { return empty() };
        let Some(parent) = self.get_message_local(parent_id) else { return empty() };
        let Some(att_id) = parent.attachments.first() else { return empty() };
        let Some(att) = self.get_attachment(att_id) else { return empty() };

        let meta = &att.metadata;
        let category = match meta.category() {
            FileCategory::Audio => "audio",
            FileCategory::Image => "image",
            FileCategory::Video => "video",
            _ => return empty(),
        };

        let duration = meta
            .duration_seconds
            .map(|secs| {
                let total = secs as u64;
                format!("{}:{:02}", total / 60, total % 60)
            })
            .unwrap_or_default();

        (category.to_string(), att.id.to_string(), duration)
    }

    /// Sender avatar bytes for a message, resolved per conversation type
    /// (anon-DM own-vs-peer, anon-group member). `None` for server channels/DMs,
    /// whose avatars come from the profile path.
    pub fn message_sender_avatar(&self, msg: &crate::domain::message::Message) -> Option<Vec<u8>> {
        use crate::domain::message::ConversationType;
        match msg.conversation_type {
            ConversationType::AnonDm => {
                let dm_id = crate::domain::AnonDmId::from_uuid(*msg.conversation_id.as_uuid());
                let is_own = self
                    .session_current_user_id()
                    .map(|uid| msg.sender_id == Some(uid))
                    .unwrap_or(false);
                if is_own {
                    self.my_avatar()
                } else {
                    self.get_anon_dm_conversation(&dm_id).and_then(|d| d.peer_avatar)
                }
            }
            ConversationType::AnonGroup => {
                let group_id =
                    crate::domain::anon_group::AnonGroupId::from_uuid(*msg.conversation_id.as_uuid());
                let sid = msg.sender_id?;
                self.get_anon_group_member(&group_id, &sid).and_then(|m| m.avatar)
            }
            _ => None,
        }
    }

    /// Stable `User <id8>` placeholder for a user with no resolvable name.
    /// Single source of truth so the UI never hand-rolls the truncation.
    pub fn fallback_display_name(&self, user_id: &crate::domain::ids::UserId) -> String {
        let id = user_id.to_string();
        format!("User {}", &id[..8.min(id.len())])
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

    /// Build the complete DM sidebar list — regular server DMs plus anon DMs
    /// (active conversations first, then incoming requests) — as one ordered,
    /// fully-shaped vector the UI renders directly. Regular DMs are dropped when
    /// anon mode is on. This is the single source of truth for the sidebar; the
    /// UI must not merge two separate lists (that races and drops fresh DMs).
    /// Build the regular (non-anon) DM sidebar rows — one per accepted
    /// friend, keyed by the peer's `user_id` (channel-free, Signal-style).
    /// A friend with no messages yet still shows (empty preview), so a
    /// freshly-accepted friend appears immediately. Returns empty in anon
    /// mode (regular DMs carry a server identity link the sidebar hides).
    ///
    /// This is the single source of truth for regular DM rows — both
    /// `list_dm_sidebar` and the initial-load sidebar builder use it, so
    /// the UI never iterates channels to find DMs.
    pub fn list_regular_dm_sidebar(&self) -> Vec<crate::ui::sidebar_data::DMListItemSendData> {
        use crate::domain::message::{truncate_preview_text, ConversationType};
        use crate::domain::user::FriendshipStatus;
        use crate::ui::chat_data::initials_from_name;
        use crate::ui::sidebar_data::DMListItemSendData;

        if self.anon_mode_enabled() {
            return Vec::new();
        }
        let Some(current_user_id) = self.auth_state().get().current_user_id else {
            return Vec::new();
        };

        let mut out: Vec<DMListItemSendData> = Vec::new();
        let friends = self.users().get_friends_local().unwrap_or_default();
        for friendship in friends {
            if friendship.status != FriendshipStatus::Accepted {
                continue;
            }
            // The peer is whichever party isn't us.
            let peer_id = if friendship.requester_id == current_user_id {
                friendship.addressee_id
            } else {
                friendship.requester_id
            };
            // Local storage key for a channel-free DM = the peer's user_id
            // wrapped as a ChannelId (same UUID).
            let conversation_id: crate::domain::ids::ChannelId = peer_id.into();

            let display_name = self.display_name(&peer_id);
            let avatar_bytes = self.get_profile_local(&peer_id).and_then(|p| p.avatar_bytes);

            // Preview + timestamp from local DM history (empty if none).
            let (last_message, timestamp) = self
                .get_messages_local(&conversation_id, &ConversationType::ServerDm, None, 1)
                .ok()
                .and_then(|msgs| msgs.into_iter().next())
                .map(|msg| {
                    let is_own = msg.sender_id == Some(current_user_id);
                    let text = msg
                        .as_text()
                        .map(|t| {
                            let trimmed = truncate_preview_text(t, 50);
                            if is_own {
                                format!("You: {}", trimmed)
                            } else {
                                trimmed
                            }
                        })
                        .unwrap_or_default();
                    let ts = msg
                        .created_at
                        .with_timezone(&chrono::Local)
                        .format("%H:%M")
                        .to_string();
                    (text, ts)
                })
                .unwrap_or_default();

            let unread_count = self
                .messages()
                .message_repo
                .get_unread_count(&conversation_id, &ConversationType::ServerDm)
                .unwrap_or(0) as u32;

            out.push(DMListItemSendData {
                channel_id: peer_id.to_string(),
                user_id: peer_id.to_string(),
                initials: initials_from_name(&display_name),
                display_name,
                avatar_bytes,
                avatar_decoded: None,
                presence_status: "offline".to_string(),
                last_message,
                timestamp,
                unread_count,
                is_typing: false,
                is_anon: false,
            });
        }
        out
    }

    /// Build the complete DM sidebar list — regular DMs plus anon DMs
    /// (active conversations first, then incoming requests) — as one ordered,
    /// fully-shaped vector the UI renders directly. Regular DMs are dropped when
    /// anon mode is on. This is the single source of truth for the sidebar; the
    /// UI must not merge two separate lists (that races and drops fresh DMs).
    pub fn list_dm_sidebar(&self) -> Vec<crate::ui::sidebar_data::DMListItemSendData> {
        use crate::domain::anon_dm::AnonDmStatus;
        use crate::domain::message::truncate_preview_text;
        use crate::ui::chat_data::initials_from_name;
        use crate::ui::sidebar_data::DMListItemSendData;

        // Mode-exclusive: shield ON shows ONLY anon DMs; OFF shows ONLY regular
        // (friend-keyed) DMs. An anon conversation must not surface — or be
        // operable — in the identified UI, and identified friends must not show
        // in anon mode. This is the reactive filter behind the sidebar refresh on
        // toggle.
        if self.anon_mode_enabled() {
            let mut out: Vec<DMListItemSendData> = Vec::new();

        // Anon DMs — active conversations.
        for dm in self.list_anon_dm_conversations() {
            if dm.status == AnonDmStatus::Pending {
                continue;
            }
            let display_name = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
            let (last_message, timestamp) = match self.get_latest_anon_dm_message(&dm.id) {
                Some(msg) => {
                    let preview = msg
                        .as_text()
                        .map(|t| truncate_preview_text(t, 50))
                        .unwrap_or_else(|| "[Attachment]".to_string());
                    (
                        preview,
                        msg.created_at
                            .with_timezone(&chrono::Local)
                            .format("%H:%M")
                            .to_string(),
                    )
                }
                None => (String::new(), String::new()),
            };
            out.push(DMListItemSendData {
                channel_id: dm.id.to_string(),
                user_id: dm.peer_id.to_string(),
                initials: initials_from_name(&display_name),
                display_name,
                avatar_bytes: dm.peer_avatar.clone(),
                avatar_decoded: None,
                presence_status: if dm.status == AnonDmStatus::Active {
                    "online".to_string()
                } else {
                    "offline".to_string()
                },
                last_message,
                timestamp,
                unread_count: dm.unread_count,
                is_typing: false,
                is_anon: true,
            });
        }

        // Anon DMs — incoming message requests awaiting acceptance.
        for dm in self.list_anon_dm_message_requests() {
            let peer_name = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
            let last_message = match self.get_latest_anon_dm_message(&dm.id) {
                Some(msg) => msg.as_text().unwrap_or_default().to_string(),
                None => "Message request".to_string(),
            };
            out.push(DMListItemSendData {
                channel_id: dm.id.to_string(),
                user_id: dm.peer_id.to_string(),
                initials: initials_from_name(&peer_name),
                // Envelope marker distinguishes a pending request from an
                // active conversation in the sidebar.
                display_name: format!("\u{1F4E9} {}", peer_name),
                avatar_bytes: dm.peer_avatar.clone(),
                avatar_decoded: None,
                presence_status: "pending".to_string(),
                last_message,
                timestamp: dm
                    .created_at
                    .with_timezone(&chrono::Local)
                    .format("%H:%M")
                    .to_string(),
                unread_count: 1,
                is_typing: false,
                is_anon: true,
            });
        }

        // Anon DMs — outgoing requests we sent, awaiting acceptance.
        for dm in self.list_anon_dm_outgoing_requests() {
            let peer_name = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
            let (last_message, timestamp) = match self.get_latest_anon_dm_message(&dm.id) {
                Some(msg) => {
                    let preview = msg
                        .as_text()
                        .map(|t| format!("You: {}", truncate_preview_text(t, 50)))
                        .unwrap_or_else(|| "Message request sent".to_string());
                    (
                        preview,
                        msg.created_at.with_timezone(&chrono::Local).format("%H:%M").to_string(),
                    )
                }
                None => (
                    "Waiting for response...".to_string(),
                    dm.created_at.with_timezone(&chrono::Local).format("%H:%M").to_string(),
                ),
            };
            out.push(DMListItemSendData {
                channel_id: dm.id.to_string(),
                user_id: dm.peer_id.to_string(),
                initials: initials_from_name(&peer_name),
                // Postbox marker distinguishes an outgoing pending request.
                display_name: format!("\u{1F4EE} {} (pending)", peer_name),
                avatar_bytes: dm.peer_avatar.clone(),
                avatar_decoded: None,
                presence_status: "offline".to_string(),
                last_message,
                timestamp,
                unread_count: 0,
                is_typing: false,
                is_anon: true,
            });
        }

            out
        } else {
            // Non-anon mode: only identified, friend-keyed DMs.
            self.list_regular_dm_sidebar()
        }
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
                    has_avatar: false,
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

    /// List the current user's pending channel invitations (server scopes to
    /// PENDING + unexpired). Best-effort — empty on failure.
    pub async fn list_pending_channel_invites(
        &self,
    ) -> Vec<crate::proto::chat_v1::PendingChannelInvite> {
        let mut client = self.services.grpc_pool.channel_client();
        match client.list_pending_invites().await {
            Ok(resp) => resp.invites,
            Err(e) => {
                tracing::debug!(error = %e, "list_pending_channel_invites failed");
                Vec::new()
            }
        }
    }

    /// Accept or decline a channel invitation; drives the response stream to
    /// completion.
    pub async fn respond_to_channel_invite(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        accept: bool,
    ) -> Result<(), crate::services::channel::ChannelError> {
        use futures::StreamExt;
        let cid = channel_id.to_string();
        let mut client = self.services.grpc_pool.channel_client();
        let mut stream = client.respond_to_invite(&cid, accept).await?;
        while let Some(ev) = stream.next().await {
            ev?;
        }
        Ok(())
    }

    /// Build the list of people the user can invite to `channel_id`: their DM
    /// contacts (regular server DMs + anon DMs — the same set shown in the
    /// sidebar), deduped, with `already_member` flagged for current members.
    pub async fn list_channel_invite_candidates(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<Vec<InviteCandidate>, crate::services::channel::ChannelError> {
        use std::collections::HashSet;
        use crate::domain::ids::UserId;

        let members: HashSet<UserId> = self
            .get_channel_members(channel_id, 100, 0)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.user_id)
            .collect();

        let mut seen: HashSet<UserId> = HashSet::new();
        let mut out: Vec<InviteCandidate> = Vec::new();

        let push = |out: &mut Vec<InviteCandidate>,
                        seen: &mut HashSet<UserId>,
                        user_id: UserId,
                        display_name: String| {
            if !seen.insert(user_id) {
                return;
            }
            let username = self.get_user_local(&user_id).map(|u| u.username).unwrap_or_default();
            let avatar_bytes = self
                .get_profile_local(&user_id)
                .and_then(|p| p.avatar_bytes)
                .unwrap_or_default();
            out.push(InviteCandidate {
                already_member: members.contains(&user_id),
                user_id,
                display_name,
                username,
                avatar_bytes,
            });
        };

        // Regular DM peers = accepted friends (channel-free, keyed by
        // peer user_id). Skip in anon mode — they carry a server identity
        // link the sidebar also suppresses there.
        if !self.anon_mode_enabled() {
            if let Some(current_user_id) = self.auth_state().get().current_user_id {
                let friends = self.users().get_friends_local().unwrap_or_default();
                for friendship in friends {
                    if friendship.status != crate::domain::user::FriendshipStatus::Accepted {
                        continue;
                    }
                    let peer_id = if friendship.requester_id == current_user_id {
                        friendship.addressee_id
                    } else {
                        friendship.requester_id
                    };
                    let display_name = self.display_name(&peer_id);
                    push(&mut out, &mut seen, peer_id, display_name);
                }
            }
        }

        // Anon DM peers.
        for dm in self.list_anon_dm_conversations() {
            let display = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| dm.peer_id.to_string());
            push(&mut out, &mut seen, dm.peer_id, display);
        }

        Ok(out)
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

    /// Signal-style profile-key handshake, fired once on both sides right
    /// after friend-accept so each peer's real display name + avatar appear
    /// immediately, with zero visible messages. See
    /// `MessageService::send_profile_key_handshake`.
    pub async fn send_profile_key_handshake(
        &self,
        peer_id: &crate::domain::ids::UserId,
    ) -> Result<(), crate::services::message::MessageError> {
        self.messages().send_profile_key_handshake(peer_id).await
    }
}

/// A person the user can invite to a channel, shaped by
/// [`ChatterClient::list_channel_invite_candidates`]. UI maps this straight to
/// its row struct — no sourcing or dedupe logic in the UI layer.
#[derive(Debug, Clone)]
pub struct InviteCandidate {
    pub user_id: crate::domain::ids::UserId,
    pub display_name: String,
    pub username: String,
    /// Peer avatar bytes (decrypted profile image), empty when none cached.
    pub avatar_bytes: Vec<u8>,
    pub already_member: bool,
}

impl InviteCandidate {
    /// Whether this candidate matches a search query (case-insensitive
    /// substring over display name or username). Keeps the "what fields a
    /// query matches" rule out of the UI filter.
    pub fn matches(&self, query: &str) -> bool {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return true;
        }
        self.display_name.to_lowercase().contains(&needle)
            || self.username.to_lowercase().contains(&needle)
    }
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
