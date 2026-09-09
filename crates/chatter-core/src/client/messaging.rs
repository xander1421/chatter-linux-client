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
    ) -> Result<crate::domain::message::UnreadAnchor, crate::services::message::MessageError> {
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

    /// Media descriptor of a reply's quoted parent — see [`ReplyParentMedia`].
    /// Lets the quote bubble render the parent's media richly (an inline audio
    /// player for audio parents, a thumbnail for visual parents) instead of
    /// plain text. Returns a default (all-empty) descriptor when the message is
    /// not a reply, the parent or its first attachment can't be resolved
    /// locally, or the parent carries no renderable media.
    pub fn reply_parent_media(
        &self,
        msg: &crate::domain::message::Message,
    ) -> ReplyParentMedia {
        use crate::domain::attachment::FileCategory;

        let Some(parent_id) = msg.reply_to.as_ref() else { return ReplyParentMedia::default() };
        let Some(parent) = self.get_message_local(parent_id) else { return ReplyParentMedia::default() };
        let Some(att_id) = parent.attachments.first() else { return ReplyParentMedia::default() };
        let Some(att) = self.get_attachment(att_id) else { return ReplyParentMedia::default() };

        let meta = &att.metadata;
        // GIFs are an image MIME but read as animated — split them out before
        // the generic image arm so the quote can badge them distinctly.
        let category = if meta.mime_type == "image/gif" {
            "gif"
        } else {
            match meta.category() {
                FileCategory::Audio => "audio",
                FileCategory::Image => "image",
                FileCategory::Video => "video",
                _ => return ReplyParentMedia::default(),
            }
        };

        let duration = meta
            .duration_seconds
            .map(|secs| {
                let total = secs as u64;
                format!("{}:{:02}", total / 60, total % 60)
            })
            .unwrap_or_default();

        // Local thumbnail fallback for visual quotes (image/gif/video); audio
        // has no thumbnail and renders its inline player instead.
        let local_thumbnail = match category {
            "image" | "gif" | "video" => meta.thumbnail.clone(),
            _ => None,
        };

        // Audio waveform (32 values, 0-100) mapped to i32 for the inline quote
        // player; empty when the parent carries none.
        let waveform = meta
            .waveform_samples
            .as_ref()
            .map(|samples| samples.iter().map(|&s| s as i32).collect())
            .unwrap_or_default();

        ReplyParentMedia {
            category: category.to_string(),
            attachment_id: att.id.to_string(),
            duration,
            thumbnail: local_thumbnail,
            filename: meta.name.clone(),
            waveform,
        }
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

    /// The newest locally-stored message in a conversation, if it has one.
    pub fn latest_message(
        &self,
        conversation_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> Option<crate::domain::message::Message> {
        self.get_messages_local(conversation_id, conversation_type, None, 1)
            .ok()
            .and_then(|msgs| msgs.into_iter().next())
    }

    /// Typed label for a message's attachments — "🖼️ Photo", "🎬 Video +2",
    /// "📎 report.pdf". `None` when the message carries no attachment or its
    /// metadata has not landed locally yet; the caller then falls back to a
    /// plain count. Media categories get a kind word rather than a filename
    /// because "IMG_4821.HEIC" tells the reader nothing a 🖼️ hasn't.
    fn attachment_preview_label(&self, message: &crate::domain::message::Message) -> Option<String> {
        use crate::domain::attachment::FileCategory;

        let meta = self
            .get_attachment_metadata(message.attachments.first()?)?
            .metadata;
        let category = meta.category();
        let icon = crate::ui::file_data::category_icon(category.as_str());
        let label = match category {
            FileCategory::Image => "Photo",
            FileCategory::Video => "Video",
            FileCategory::Audio => "Audio",
            _ => meta.name.as_str(),
        };
        let extra = message.attachments.len() - 1;
        Some(if extra > 0 {
            format!("{} {} +{}", icon, label, extra)
        } else {
            format!("{} {}", icon, label)
        })
    }

    /// The one-line sidebar preview for a message. `Message::sidebar_preview`
    /// owns the wording; this adds the attachment lookup the domain type has no
    /// storage handle for.
    pub fn message_sidebar_preview(&self, message: &crate::domain::message::Message) -> String {
        let label = self.attachment_preview_label(message);
        message.sidebar_preview(
            self.auth_state().get().current_user_id.as_ref(),
            label.as_deref(),
        )
    }

    /// Preview + local-time stamp of a conversation's latest activity: the
    /// second line and the right-hand time of a sidebar row. Empty pair when
    /// nothing has happened yet, so a fresh row renders as name only.
    ///
    /// Every sidebar row kind — DM, channel, private group — goes through here,
    /// which is why the anon-group store can hand its own `Message` in rather
    /// than each caller re-deriving the wording.
    pub fn sidebar_activity(
        &self,
        latest: Option<&crate::domain::message::Message>,
    ) -> (String, String) {
        match latest {
            Some(msg) => (
                self.message_sidebar_preview(msg),
                msg.created_at
                    .with_timezone(&chrono::Local)
                    .format("%H:%M")
                    .to_string(),
            ),
            None => (String::new(), String::new()),
        }
    }

    /// The storage bucket a channel's messages and pins are filed under.
    ///
    /// Never hardcode `ConversationType::Channel` for a channel — see
    /// `channel_sidebar_activity` for what that costs.
    pub fn channel_conversation_type(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> crate::domain::message::ConversationType {
        crate::services::message::conversion::conversation_type_for_channel(
            &self.services.channel_repo,
            channel_id,
        )
    }

    /// `sidebar_activity` for a channel row.
    ///
    /// Takes the channel rather than an id + type because the type is the trap:
    /// a channel's messages are filed under `Channel::conversation_type()`,
    /// which is `AnonGroup` for an encrypted channel — and encryption is the
    /// DEFAULT (`ChannelConfig::default().require_encryption`, domain/channel.rs)
    /// — or `SavedMessages` for the vault. It is `ConversationType::Channel`
    /// only for a plaintext channel. Hardcoding that variant is what left every
    /// channel row in the sidebar with a name and nothing else, and it is the
    /// same trap `features/sidebar/channel.rs` already documents on the
    /// message-list side. Deriving it here means no caller can repeat it.
    pub fn channel_sidebar_activity(
        &self,
        channel: &crate::domain::channel::Channel,
    ) -> (String, String) {
        self.sidebar_activity(
            self.latest_message(&channel.id, &channel.conversation_type())
                .as_ref(),
        )
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
        use crate::domain::message::ConversationType;
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
            // Presence rides on the same stored profile as the avatar — one
            // read, both fields. It used to be hardcoded "offline", so every
            // friend's dot in the sidebar was grey until a live presence event
            // happened to arrive, and grey again after the next rebuild.
            let profile = self.get_profile_local(&peer_id);
            let presence_status = profile
                .as_ref()
                .map_or("offline", |p| p.presence.as_str())
                .to_string();
            let avatar_bytes = profile.and_then(|p| p.avatar_bytes);

            // Preview + timestamp from local DM history (empty if none). This
            // used to hand-roll the text-only case, so a DM whose last message
            // was a file rendered blank until the next live message arrived.
            let (last_message, timestamp) = self.sidebar_activity(
                self.latest_message(&conversation_id, &ConversationType::ServerDm)
                    .as_ref(),
            );

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
                presence_status,
                last_message,
                timestamp,
                unread_count,
                is_typing: false,
                is_anon: false,
                is_pinned: false,
            });
        }
        self.finish_dm_sidebar(out)
    }

    /// Final pass every DM sidebar list goes through: drop rows that live in a
    /// custom section (they render inside that folder — showing them here too
    /// rendered the same conversation twice) and float pinned rows to the top.
    /// Idempotent, so the anon and non-anon paths can both call it.
    fn finish_dm_sidebar(
        &self,
        mut rows: Vec<crate::ui::sidebar_data::DMListItemSendData>,
    ) -> Vec<crate::ui::sidebar_data::DMListItemSendData> {
        let sectioned = self
            .sections()
            .all_sectioned_conversation_ids()
            .unwrap_or_default();
        rows.retain(|r| !sectioned.contains(&r.channel_id));

        let pinned = self.conversation_pins().pinned_ids();
        for row in rows.iter_mut() {
            row.is_pinned = pinned.contains(&row.channel_id);
        }
        rows.sort_by_key(|r| !r.is_pinned);
        rows
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
                is_pinned: false,
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
                is_pinned: false,
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
                is_pinned: false,
            });
        }

            self.finish_dm_sidebar(out)
        } else {
            // Non-anon mode: only identified, friend-keyed DMs.
            self.list_regular_dm_sidebar()
        }
    }

    /// Resolve a DM sidebar id to the conversation it names.
    ///
    /// The id is either an `AnonDmId` (an anon conversation) or a peer `UserId`
    /// (a channel-free server DM — the row's id IS the peer). Both are UUIDs,
    /// so the anon table is the discriminator, not the string shape.
    ///
    /// The UI used to answer this by scanning the DM sidebar model for a row
    /// with a matching id. Rows filed into a custom folder are removed from
    /// that model on purpose — they render inside the folder — so the lookup
    /// came back empty and a foldered DM opened into nothing: no active
    /// conversation, no messages, and every send silently dropped.
    pub fn resolve_dm_conversation(&self, dm_id: &str) -> Option<crate::state::ActiveConversation> {
        use crate::state::ActiveConversation;

        if let Ok(anon_id) = dm_id.parse::<crate::domain::anon_dm::AnonDmId>() {
            if self.get_anon_dm_conversation(&anon_id).is_some() {
                return Some(ActiveConversation::AnonDm(anon_id));
            }
        }
        dm_id
            .parse::<crate::domain::ids::UserId>()
            .ok()
            .map(|peer_id| ActiveConversation::ServerDm { peer_id })
    }

    /// Every conversation a message can be forwarded to, filtered by a
    /// case-insensitive name substring and ordered by name.
    ///
    /// Sources are the same mode-exclusive sidebar builders the sidebar
    /// renders — anon mode never offers an identified destination and vice
    /// versa — PLUS the rows those builders drop because they live in a custom
    /// folder (filing a conversation is a layout choice, not a reason it cannot
    /// receive a forward), PLUS Saved Messages once its channel is known
    /// locally. Rows that cannot be resolved back to a conversation to send to
    /// (mesh, mailbox DMs, a stale id) are not destinations and are dropped.
    pub fn forward_destinations(&self, query: &str) -> Vec<ForwardDestination> {
        use crate::domain::message::ConversationType as CT;

        let mut out: Vec<ForwardDestination> = Vec::new();

        for dm in self.list_dm_sidebar() {
            out.push(ForwardDestination {
                kind: if dm.is_anon { CT::AnonDm } else { CT::ServerDm },
                id: dm.channel_id,
                name: dm.display_name,
            });
        }
        for channel in self.sidebar_channels() {
            out.push(ForwardDestination {
                kind: CT::Channel,
                id: channel.id.to_string(),
                name: channel.name,
            });
        }
        for group in self.sidebar_private_groups() {
            out.push(ForwardDestination {
                kind: CT::AnonGroup,
                id: group.id.to_string(),
                name: group.name,
            });
        }
        for section in self.list_sections_with_conversations().unwrap_or_default() {
            for conv in section.conversations {
                out.push(ForwardDestination {
                    kind: conv.conversation_type,
                    id: conv.conversation_id,
                    name: conv.display_name,
                });
            }
        }
        // Saved Messages ("note to self"). Only once the channel exists
        // locally — the send path recognises it by that row's channel_type,
        // and without it the forward would route as a plain channel send.
        if let Some(user_id) = self.auth_state().get().current_user_id {
            let channel_id: crate::domain::ids::ChannelId = user_id.into();
            if !self.anon_mode_enabled() && self.get_channel_local(&channel_id).is_some() {
                out.push(ForwardDestination {
                    kind: CT::SavedMessages,
                    id: channel_id.to_string(),
                    name: "Saved Messages".to_string(),
                });
            }
        }

        let needle = query.trim().to_lowercase();
        out.retain(|d| {
            !d.name.is_empty()
                && (needle.is_empty() || d.name.to_lowercase().contains(&needle))
                && ActiveConversation::from_routing_key(&d.kind, &d.id).is_some()
        });
        out.sort_by_key(|d| d.name.to_lowercase());
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

        // Full membership, paginated — an already-joined member past the
        // server's page size would otherwise show up as invitable.
        let members: HashSet<UserId> = self
            .channels()
            .get_all_channel_members(channel_id)
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

    /// Reply count per thread root in a conversation, for the "N replies"
    /// indicator on root message bubbles.
    pub fn thread_reply_counts(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
    ) -> std::collections::HashMap<crate::domain::ids::MessageId, u32> {
        self.messages()
            .thread_reply_counts(channel_id, conversation_type)
            .unwrap_or_default()
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
                None, // thread replies use parent_id, not a reply quote
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

/// Media descriptor of a reply's quoted parent, shaped by
/// [`ChatterClient::reply_parent_media`]. Lets the quote bubble render the
/// parent's media richly (an inline audio player for audio parents, a thumbnail
/// for visual parents) instead of plain text. All fields are empty/`None` when
/// the message is not a reply, the parent or its first attachment can't be
/// resolved locally, or the parent carries no renderable media.
#[derive(Debug, Clone, Default)]
pub struct ReplyParentMedia {
    /// `"audio"`/`"image"`/`"gif"`/`"video"` — `"gif"` splits out of image so
    /// the quote can badge it as animated. Empty when no renderable media.
    pub category: String,
    /// Parent attachment id (for play/seek callbacks).
    pub attachment_id: String,
    /// Formatted `"m:ss"` for audio, else empty.
    pub duration: String,
    /// Parent attachment's locally-stored thumbnail bytes for visual categories
    /// — a fallback the converter uses when the wire quote thumbnail (capped at
    /// ~12KB and often dropped) is absent, so image/gif/video quotes still show
    /// a preview instead of degrading to text.
    pub thumbnail: Option<Vec<u8>>,
    /// Parent attachment filename, shown in the inline audio quote player.
    pub filename: String,
    /// Audio waveform (32 values, 0-100) for the inline quote player; empty when
    /// the parent carries none.
    pub waveform: Vec<i32>,
}

/// One destination row in the forward picker, shaped by
/// [`ChatterClient::forward_destinations`].
///
/// `kind` is core's own conversation vocabulary, so the confirm step maps a
/// picked row back with [`ActiveConversation::from_routing_key`] instead of the
/// UI inventing a tag of its own.
#[derive(Debug, Clone)]
pub struct ForwardDestination {
    pub id: String,
    pub name: String,
    pub kind: ConversationType,
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
    /// Quoted parent's media descriptor (category/attachment/duration/filename/
    /// waveform) so the sender's own bubble renders the audio player / media
    /// preview immediately, without waiting for a reload.
    pub reply_parent_media: crate::client::ReplyParentMedia,
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
