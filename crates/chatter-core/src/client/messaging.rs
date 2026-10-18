//! messaging façade — thin delegates to services.

use std::sync::Arc;

use super::ChatterClient;
use crate::domain::channel::Channel;
use crate::domain::message::ConversationType;
use crate::state::ActiveConversation;

/// Channel invitation text resolved from the current user's local contacts.
pub struct ChannelInviteRow {
    pub channel_id: String,
    pub channel_name: String,
    pub invited_by: String,
    pub created_at: u64,
    pub note: String,
}

/// Make every chat-list preview a single plain-text line, including notices,
/// attachment labels and pending requests. The row elides to its own width.
pub(crate) fn plain_sidebar_preview(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut remove = vec![false; chars.len()];
    let mut i = 0;
    while i < chars.len() {
        let marker = chars[i];
        if marker == '>'
            && chars[..i]
                .iter()
                .rev()
                .take_while(|c| **c != '\n')
                .all(|c| c.is_whitespace())
        {
            remove[i] = true;
            i += 1;
            continue;
        }
        if matches!(marker, '*' | '_' | '`') {
            let start = i;
            while i < chars.len() && chars[i] == marker {
                i += 1;
            }
            let width = i - start;
            let standalone = (start == 0 || chars[start - 1].is_whitespace())
                && (i == chars.len() || chars[i].is_whitespace());
            if standalone && ((marker == '`' && width >= 3) || (marker != '`' && width >= 2)) {
                for index in start..i {
                    remove[index] = true;
                }
                continue;
            }
            let left_boundary = start == 0
                || (!chars[start - 1].is_alphanumeric()
                    && chars[start - 1] != '/'
                    && chars[start - 1] != '\\');
            let content_follows = i < chars.len() && !chars[i].is_whitespace();
            if left_boundary && content_follows {
                let mut end = i + 1;
                while end + width <= chars.len() {
                    if chars[end..end + width].iter().all(|c| *c == marker)
                        && (!chars[end - 1].is_whitespace() || (marker == '`' && width >= 3))
                        && (end + width == chars.len()
                            || (!chars[end + width].is_alphanumeric()
                                && !(marker == '_'
                                    && chars[end + width] == '.'
                                    && end + width + 1 < chars.len()
                                    && chars[end + width + 1].is_alphanumeric())))
                    {
                        for index in start..i {
                            remove[index] = true;
                        }
                        for index in end..end + width {
                            remove[index] = true;
                        }
                        break;
                    }
                    end += 1;
                }
            }
            continue;
        }
        i += 1;
    }
    let mut output = String::with_capacity(input.len());
    let mut pending_space = false;
    for (index, ch) in chars.into_iter().enumerate() {
        if ch.is_whitespace() {
            pending_space = !output.is_empty();
        } else if !remove[index] && !ch.is_control() {
            if pending_space {
                output.push(' ');
                pending_space = false;
            }
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod sidebar_preview_tests {
    use super::plain_sidebar_preview;

    #[test]
    fn markdown_fences_and_lines_become_one_plain_line() {
        assert_eq!(
            plain_sidebar_preview("  You: **hello**\n```\n> _world_  `again`\n```  "),
            "You: hello world again"
        );
    }

    #[test]
    fn emoji_clusters_survive_without_truncation() {
        let emoji = "👩🏽‍💻";
        let source = format!("{}{}{}\n**done**", emoji, emoji, emoji);
        assert_eq!(
            plain_sidebar_preview(&source),
            format!("{}{}{} done", emoji, emoji, emoji)
        );
    }
}

/// Actions available for the current stored selection.
pub struct SelectionActions {
    pub can_delete_everyone: bool,
    pub can_forward: bool,
}

impl ChatterClient {
    // ── Messages façade ──────────────────────────────────────────────────

    /// Execute a selected-message delete using the captured account/session.
    pub async fn delete_selected_messages(
        self: &Arc<Self>,
        conversation: ActiveConversation,
        ids: &[String],
        scope: crate::services::conversation::SelectedDeleteScope,
        account: crate::domain::ids::UserId,
        session_revision: u64,
        completed: tokio::sync::mpsc::UnboundedSender<Vec<(crate::domain::ids::UserId, u64)>>,
    ) -> crate::services::conversation::SelectedDeleteResult {
        self.conversation(conversation)
            .delete_selected(ids, scope, account, session_revision, completed)
            .await
    }

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

    /// See [`crate::services::message::MessageService::get_older_messages_local_asc`].
    pub fn get_older_messages_local_asc(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        before: &crate::domain::ids::MessageId,
        until: Option<&crate::domain::ids::MessageId>,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::message::MessageError> {
        self.messages().get_older_messages_local_asc(
            channel_id,
            conversation_type,
            before,
            until,
            crate::config::MESSAGE_PAGE_SIZE as u32,
        )
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
        // Off screen nothing is read; `set_app_visible(true)` catches up.
        if !self.chat_state().is_app_visible() {
            return Ok(Default::default());
        }
        self.messages()
            .mark_conversation_read(conversation_id, conversation_type)
    }

    pub fn classify_active_conversation(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &crate::domain::message::ConversationType,
        peer_id: Option<crate::domain::ids::UserId>,
    ) -> crate::state::ActiveConversation {
        self.messages()
            .classify_active_conversation(channel_id, conversation_type, peer_id)
    }

    pub fn user_has_reacted(
        &self,
        scope: &crate::domain::reaction::ReactionScope,
        sender_id: &crate::domain::ids::UserId,
        sent_at_ns: u64,
        emoji: &str,
    ) -> bool {
        self.messages()
            .user_has_reacted(scope, sender_id, sent_at_ns, emoji)
            .unwrap_or(false)
    }

    /// Resolve selected IDs against current storage and conversation, in the
    /// caller's display order. A missing or foreign row invalidates the set.
    pub fn selected_message_targets(
        &self,
        conversation: ActiveConversation,
        ids: &[String],
    ) -> Option<Vec<(crate::domain::ids::UserId, u64)>> {
        if ids.is_empty() || conversation.channel_id().is_none() { return None; }
        let mut seen = std::collections::HashSet::new();
        ids.iter().map(|id| {
            if !seen.insert(id) { return None; }
            let id = id.parse().ok()?;
            let message = self.get_message_local(&id)?;
            if Some(message.conversation_id) != conversation.channel_id() { return None; }
            Some((message.sender_id?, message.sent_at))
        }).collect()
    }

    pub fn selection_actions(
        self: &Arc<Self>,
        conversation: ActiveConversation,
        ids: &[String],
    ) -> SelectionActions {
        let handle = self.conversation(conversation);
        let targets = self.selected_message_targets(conversation, ids);
        SelectionActions {
            can_delete_everyone: targets.as_ref().is_some_and(|targets|
                targets.iter().all(|(author, ts)|
                    handle.can_delete_for_everyone(author, *ts).unwrap_or(false))),
            can_forward: handle.can_forward_from(),
        }
    }

    pub fn get_message_local(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Option<crate::domain::message::Message> {
        self.messages().get_message_local(message_id).ok().flatten()
    }

    pub fn get_history_target_local(
        &self,
        message_id: &crate::domain::ids::MessageId,
        channel_id: &crate::domain::ids::ChannelId,
        conversation_type: &ConversationType,
    ) -> Vec<crate::domain::message::Message> {
        self.messages()
            .get_history_target_local(message_id, channel_id, conversation_type)
            .unwrap_or_default()
    }

    /// Timestamp of an outgoing failed or parked message eligible for tap-to-retry.
    pub fn retryable_message_timestamp(
        &self,
        message_id: &crate::domain::ids::MessageId,
    ) -> Option<u64> {
        let message = self.get_message_local(message_id)?;
        let me = self.session_current_user_id()?;
        (message.sender_id == Some(me)
            && match message.conversation_type {
                ConversationType::Channel | ConversationType::ServerDm => {
                    matches!(message.delivery_status.as_str(), "failed" | "parked")
                }
                ConversationType::AnonDm => {
                    message.delivery_status == "failed"
                        && message.failure_reason.as_deref() == Some("Message not submitted")
                }
                ConversationType::AnonGroup
                | ConversationType::SavedMessages
                | ConversationType::Mesh
                | ConversationType::MeshDm => false,
            }
            && !message.is_deleted)
            .then_some(message.sent_at)
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
        self.message_sender_display_names(std::slice::from_ref(msg))
            .pop()
            .unwrap_or_default()
    }

    /// Re-resolve a stored join notice after its subject's profile arrives.
    /// The notice stores the user ID, so a name unavailable at insertion time
    /// does not become permanent text in the message history.
    pub fn refreshed_join_notice(
        &self,
        message_id: &crate::domain::ids::MessageId,
        user_id: &crate::domain::ids::UserId,
    ) -> Option<(String, String)> {
        use crate::domain::message::{MessageContent, SystemMessage};

        let message = self.get_message_local(message_id)?;
        let MessageContent::System(system @ SystemMessage::UserJoined(subject)) = &message.content
        else {
            return None;
        };
        if subject != user_id || message.sender_id != Some(*user_id) {
            return None;
        }
        let name = self.message_sender_display_name(&message);
        let content = format!(
            "{} {}",
            name,
            system.description_for(&message.conversation_type)
        );
        Some((name, content))
    }

    /// [`Self::message_sender_display_name`] for a whole page, in order. Each
    /// distinct sender is resolved once, and every sender's directory name in
    /// a single query (`UserService::get_display_names`) — opening a
    /// conversation used to run that lookup per row.
    pub fn message_sender_display_names(
        &self,
        messages: &[crate::domain::message::Message],
    ) -> Vec<String> {
        use crate::domain::message::ConversationType;
        let me = self.session_current_user_id();
        let mut seen = std::collections::HashSet::new();
        let senders: Vec<crate::domain::ids::UserId> = messages
            .iter()
            .filter_map(|m| m.sender_id)
            .filter(|id| seen.insert(*id))
            .collect();
        let names = self.users().get_display_names(&senders);
        // Anon-group names are per group, so they key on the conversation too.
        let mut group_names = std::collections::HashMap::new();
        messages
            .iter()
            .map(|msg| {
                if let crate::domain::message::MessageContent::System(
                    crate::domain::message::SystemMessage::AnonGroupUserLeft {
                        left_name: Some(name),
                    },
                ) = &msg.content
                {
                    return name.clone();
                }
                // System lines about us read "You left the channel", as Signal's do.
                if matches!(
                    msg.content,
                    crate::domain::message::MessageContent::System(_)
                ) && msg.sender_id.is_some()
                    && msg.sender_id == me
                {
                    return "You".to_string();
                }
                let Some(id) = msg.sender_id else {
                    return "Anonymous".to_string();
                };
                if msg.conversation_type == ConversationType::AnonGroup {
                    let group_name = group_names
                        .entry((msg.conversation_id, id))
                        .or_insert_with(|| {
                            let group_id = crate::domain::anon_group::AnonGroupId::from_uuid(
                                *msg.conversation_id.as_uuid(),
                            );
                            self.get_anon_group_member_display_name(&group_id, &id)
                                .filter(|n| !n.is_empty())
                        })
                        .clone();
                    if let Some(name) = group_name {
                        return name;
                    }
                }
                names
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| self.display_name(&id))
            })
            .collect()
    }

    /// Reply-quote author display name, scoped like the sender. Empty when the
    /// message is not a reply or carries no quoted author.
    pub fn reply_author_display_name(&self, msg: &crate::domain::message::Message) -> String {
        match msg.reply_to_author_id {
            Some(ref id) => self.quote_author_name(id, || {
                self.display_name_in(&msg.conversation_type, &msg.conversation_id, id)
            }),
            None => String::new(),
        }
    }

    /// Who a quote or the composer's reply bar names: "You" for our own
    /// message, as every messenger does — our public name there read as if
    /// someone else wrote it. Otherwise the name `resolve` gives.
    pub fn quote_author_name(
        &self,
        author: &crate::domain::ids::UserId,
        resolve: impl FnOnce() -> String,
    ) -> String {
        if self.session_current_user_id().as_ref() == Some(author) {
            "You".to_string()
        } else {
            resolve()
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
            Some(ref parent) if !msg.conversation_type.is_anon() => {
                self.reply_parent_missing(parent)
            }
            _ => false,
        }
    }

    /// Media descriptor of a reply's quoted parent — see [`ReplyParentMedia`].
    /// Lets the quote bubble render the parent's media richly (an inline audio
    /// player for audio parents, a thumbnail for visual parents) instead of
    /// plain text. Returns a default (all-empty) descriptor when the message is
    /// not a reply, the parent or its first attachment can't be resolved
    /// locally, or the parent carries no renderable media.
    pub fn reply_parent_media(&self, msg: &crate::domain::message::Message) -> ReplyParentMedia {
        use crate::domain::attachment::FileCategory;

        let Some(parent_id) = msg.reply_to.as_ref() else {
            return ReplyParentMedia::default();
        };
        let Some(parent) = self.get_message_local(parent_id) else {
            return ReplyParentMedia::default();
        };
        let Some(att_id) = parent.attachments.first() else {
            return ReplyParentMedia::default();
        };
        let Some(att) = self.get_attachment(att_id) else {
            return ReplyParentMedia::default();
        };

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

    /// Sender avatar bytes for a message row, resolved per conversation type
    /// (anon-group member; channels our own avatar or the sender's stored
    /// profile). `None` = initials.
    ///
    /// DMs (server and anon) get `None`: a DM row draws no sender avatar —
    /// there are two people and the header already shows the other one
    /// (`message_item.slint`, `!root.is-dm`). Resolving and decoding one per
    /// row anyway was 85% of the time it took to open a DM (400 ms of an
    /// 870 ms open on a Galaxy S9, `conversation_open_bench`).
    pub fn message_sender_avatar(&self, msg: &crate::domain::message::Message) -> Option<Vec<u8>> {
        use crate::domain::message::ConversationType;
        match msg.conversation_type {
            ConversationType::ServerDm | ConversationType::AnonDm => None,
            ConversationType::AnonGroup => {
                let group_id = crate::domain::anon_group::AnonGroupId::from_uuid(
                    *msg.conversation_id.as_uuid(),
                );
                let sid = msg.sender_id?;
                self.get_anon_group_member_avatar(&group_id, &sid)
            }
            _ => self.user_avatar(&msg.sender_id?),
        }
    }

    /// A user's avatar as this device knows it: ours from settings (our own
    /// profile row carries none), anyone else's from their stored, decrypted
    /// profile. `None` = initials.
    pub fn user_avatar(&self, user_id: &crate::domain::ids::UserId) -> Option<Vec<u8>> {
        if self.session_current_user_id().as_ref() == Some(user_id) {
            self.my_avatar()
        } else {
            self.get_profile_local(user_id).and_then(|p| p.avatar_bytes)
        }
    }

    /// Stable `User <id8>` placeholder for a user with no resolvable name.
    /// Single source of truth so the UI never hand-rolls the truncation.
    pub fn fallback_display_name(&self, user_id: &crate::domain::ids::UserId) -> String {
        crate::services::user::friend_display_name_placeholder(user_id)
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
        self.channels()
            .list_members_local(channel_id)
            .unwrap_or_default()
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
        // Bound the work even if a chat contains only timer-update notices.
        const PAGE: u32 = 32;
        const MAX_PAGES: usize = 8;
        let mut before: Option<crate::domain::ids::MessageId> = None;
        for _ in 0..MAX_PAGES {
            let messages = self
                .get_messages_local(conversation_id, conversation_type, before.as_ref(), PAGE)
                .ok()?;
            if let Some(message) = messages
                .iter()
                .find(|message| !crate::services::disappearing::is_timer_notice(message))
            {
                return Some(message.clone());
            }
            if messages.len() < PAGE as usize {
                return None;
            }
            let next = messages.last()?.id;
            if before == Some(next) {
                return None;
            }
            before = Some(next);
        }
        None
    }

    /// Typed label for a message's attachments — "🎤 Voice message",
    /// "🎬 Video +2", "📄 report.pdf" (see `FileMetadata::preview_label`).
    /// `None` when the message carries no attachment or its metadata has not
    /// landed locally yet; the caller then falls back to a plain count.
    fn attachment_preview_label(
        &self,
        message: &crate::domain::message::Message,
    ) -> Option<String> {
        let label = self
            .get_attachment_metadata(message.attachments.first()?)?
            .metadata
            .preview_label();
        let extra = message.attachments.len() - 1;
        Some(if extra > 0 {
            format!("{} +{}", label, extra)
        } else {
            label
        })
    }

    /// Flatten search snippets with the same rules as chat-row previews.
    pub fn plain_message_preview(&self, input: &str) -> String {
        plain_sidebar_preview(input)
    }

    /// The one-line sidebar preview for a message. `Message::sidebar_preview`
    /// owns the wording; this adds the attachment lookup the domain type has no
    /// storage handle for.
    pub fn message_sidebar_preview(&self, message: &crate::domain::message::Message) -> String {
        let label = self.attachment_preview_label(message);
        let preview = message.sidebar_preview(
            self.auth_state().get().current_user_id.as_ref(),
            label.as_deref(),
        );
        // A notice is worded to follow its actor ("left the channel"), as the
        // message list renders it; alone in a preview it named nobody.
        if matches!(
            message.content,
            crate::domain::message::MessageContent::System(_)
        ) {
            return plain_sidebar_preview(&format!(
                "{} {}",
                self.message_sender_display_name(message),
                preview
            ));
        }
        plain_sidebar_preview(&preview)
    }

    /// Preview + raw activity time of a conversation's latest message: the
    /// second line and the time source of a sidebar row. Empty pair when
    /// nothing has happened yet, so a fresh row renders as name only.
    ///
    /// Every sidebar row kind — DM, channel, private group — goes through here,
    /// which is why the anon-group store can hand its own `Message` in rather
    /// than each caller re-deriving the wording.
    pub fn sidebar_activity(
        &self,
        latest: Option<&crate::domain::message::Message>,
    ) -> (String, Option<chrono::DateTime<chrono::Utc>>) {
        match latest {
            Some(msg) => (self.message_sidebar_preview(msg), Some(msg.created_at)),
            None => (String::new(), None),
        }
    }

    /// The storage bucket a channel's messages and pins are filed under.
    ///
    /// Never hardcode `ConversationType::Channel` for a channel: its messages
    /// are filed under `Channel::conversation_type()` — `Channel`, or
    /// `SavedMessages` for the vault, or `ServerDm`.
    pub fn channel_conversation_type(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> crate::domain::message::ConversationType {
        crate::services::message::conversion::conversation_type_for_channel(
            &self.services.channel_repo,
            channel_id,
        )
    }

    /// One folder row, shaped like the loose row for the same conversation so
    /// a chat looks the same filed or not: the name (a channel-free DM with no
    /// unified-conversation row yet falls back to the peer profile), initials,
    /// badge, pin, Leave/Delete availability, and — for the kinds a folder
    /// renders with the main list's rows (friend DMs and channels) — preview,
    /// time, avatar, presence and the private-channel lock.
    pub fn section_row(
        &self,
        conv: &crate::domain::section::SectionConversation,
    ) -> crate::ui::sidebar_data::ChatRowSendData {
        use crate::domain::message::ConversationType as CT;
        use crate::ui::sidebar_data::ChatRowSendData;

        let mut display_name = conv.display_name.clone();
        if display_name.is_empty() {
            if let Ok(peer_id) = conv.conversation_id.parse() {
                display_name = self.display_name(&peer_id);
            }
        }
        let channel_id = conv
            .conversation_id
            .parse::<crate::domain::ids::ChannelId>()
            .ok();
        let is_channel = conv.conversation_type == CT::Channel;
        let mut row = ChatRowSendData {
            unread_count: conv.unread_count,
            is_pinned: conv.is_pinned,
            can_leave: match conv.conversation_type {
                // As `private_group_row`.
                CT::AnonGroup => conv
                    .conversation_id
                    .parse()
                    .is_ok_and(|id| self.anon_groups().can_leave(&id)),
                CT::Channel => channel_id.is_some_and(|id| self.can_leave_channel(&id)),
                _ => false,
            },
            can_delete: (is_channel && channel_id.is_some_and(|id| self.is_former_channel(&id)))
                || (conv.conversation_type == CT::AnonGroup
                    && conv
                        .conversation_id
                        .parse()
                        .ok()
                        .and_then(|id| self.get_anon_group(&id))
                        .is_some_and(|g| {
                            g.status == crate::domain::anon_group::AnonGroupStatus::Removed
                        })),
            ..ChatRowSendData::new(
                conv.conversation_type.clone(),
                conv.conversation_id.clone(),
                display_name,
            )
        };

        let Some(id) = channel_id else { return row };
        if !matches!(conv.conversation_type, CT::ServerDm | CT::Channel) {
            return row;
        }
        (row.last_message, row.timestamp_at) =
            self.sidebar_activity(self.latest_message(&id, &conv.conversation_type).as_ref());
        match conv.conversation_type {
            CT::ServerDm => {
                let peer = crate::domain::ids::UserId::from_uuid(*id.as_uuid());
                let profile = self.get_profile_local(&peer);
                row.presence_status = self.peer_presence(&peer).as_str().to_string();
                row.avatar_bytes = profile.and_then(|p| p.avatar_bytes);
            }
            _ => {
                let channel = self.channels().channel_repo.get(&id).ok().flatten();
                row.private_channel = channel.as_ref().is_some_and(|c| {
                    c.channel_type == crate::domain::channel::ChannelType::Private
                });
                // A channel's icon is raw bytes on its record, not a URL.
                row.avatar_bytes = channel.and_then(|c| c.icon).filter(|b| !b.is_empty());
            }
        }
        row
    }

    /// Build the regular (non-anon) DM sidebar rows — one per accepted
    /// friend, keyed by the peer's `user_id` (channel-free, Signal-style).
    /// A friend with no messages yet still shows (empty preview), so a
    /// freshly-accepted friend appears immediately. Shown in both modes: in
    /// Shield the same conversation travels over the mailbox.
    ///
    /// This is the single source of truth for regular DM rows — both
    /// `list_dm_sidebar` and the initial-load sidebar builder use it, so
    /// the UI never iterates channels to find DMs.
    pub fn list_regular_dm_sidebar(&self) -> Vec<crate::ui::sidebar_data::ChatRowSendData> {
        use crate::domain::message::ConversationType;
        use crate::ui::sidebar_data::ChatRowSendData;

        // Each row carries its last activity, so the list is ordered newest
        // first. The friend list's order used to leak through, so every
        // rebuild (opening a profile, a pin) undid the move-to-top a live
        // message had made.
        let mut out: Vec<ChatRowSendData> = Vec::new();
        for peer_id in self.dm_peers() {
            // Local storage key for a channel-free DM = the peer's user_id
            // wrapped as a ChannelId (same UUID).
            let conversation_id: crate::domain::ids::ChannelId = peer_id.into();

            let display_name = self.display_name(&peer_id);
            // Presence from the stored profile (hidden for a blocked peer).
            // It used to be hardcoded "offline", so every friend's dot in the
            // sidebar was grey until a live presence event happened to
            // arrive, and grey again after the next rebuild.
            let profile = self.get_profile_local(&peer_id);
            let presence_status = self.peer_presence(&peer_id).as_str().to_string();
            let avatar_bytes = profile.and_then(|p| p.avatar_bytes);

            // Preview + timestamp from local DM history (empty if none). This
            // used to hand-roll the text-only case, so a DM whose last message
            // was a file rendered blank until the next live message arrived.
            let latest = self.latest_message(&conversation_id, &ConversationType::ServerDm);
            let (last_message, timestamp_at) = self.sidebar_activity(latest.as_ref());

            let unread_count = self
                .messages()
                .message_repo
                .get_unread_count(&conversation_id, &ConversationType::ServerDm)
                .unwrap_or(0) as u32;

            out.push(ChatRowSendData {
                user_id: peer_id.to_string(),
                avatar_bytes,
                presence_status,
                last_message,
                timestamp_at,
                unread_count,
                last_activity: latest.map(|m| m.created_at),
                ..ChatRowSendData::new(
                    ConversationType::ServerDm,
                    peer_id.to_string(),
                    display_name,
                )
            });
        }
        self.finish_dm_sidebar(out)
    }

    /// Final pass every DM sidebar list goes through: drop rows that live in a
    /// custom section (they render inside that folder — showing them here too
    /// rendered the same conversation twice), mark pins, and order the rows
    /// as the chat list does. Idempotent, so the anon and non-anon paths can
    /// both call it.
    fn finish_dm_sidebar(
        &self,
        mut rows: Vec<crate::ui::sidebar_data::ChatRowSendData>,
    ) -> Vec<crate::ui::sidebar_data::ChatRowSendData> {
        let sectioned = self
            .sections()
            .all_sectioned_conversation_ids()
            .unwrap_or_default();
        rows.retain(|r| !sectioned.contains(&r.id));

        let pinned = self.conversation_pins().pinned_ids();
        for row in rows.iter_mut() {
            row.is_pinned = pinned.contains(&row.id);
        }
        crate::ui::sidebar_data::order_chat_rows(&mut rows);
        rows
    }

    /// Build the complete DM sidebar list — friend DMs plus secret chats
    /// (anon DMs: active conversations, incoming requests, outgoing requests)
    /// — as one ordered, fully-shaped vector the UI renders directly. This is
    /// the single source of truth for the sidebar; the UI must not merge two
    /// separate lists (that races and drops fresh DMs).
    pub fn list_dm_sidebar(&self) -> Vec<crate::ui::sidebar_data::ChatRowSendData> {
        use crate::domain::anon_dm::AnonDmStatus;
        use crate::domain::message::ConversationType;
        use crate::ui::sidebar_data::ChatRowSendData;

        // Friend DMs show in both modes — one conversation per friend, whose
        // transport follows the mode. Secret chats (anon DMs) also show in
        // both modes (Telegram-style), as their own `AnonDm` rows beside the
        // friend DM with the same person: the row kind is what the UI marks
        // as secret. Their traffic rides the onion channel in either mode.
        let mut out: Vec<ChatRowSendData> = self.list_regular_dm_sidebar();
        // Old requests can coexist with an accepted chat in local storage.
        // A superseding incoming request takes the peer's one visible row.
        let mut shown_secret_peers = std::collections::HashSet::new();
        let anon_conversations = self.list_anon_dm_conversations();
        let superseded_peers: std::collections::HashSet<_> = anon_conversations
            .iter()
            .filter(|dm| dm.status == AnonDmStatus::Superseded)
            .map(|dm| dm.peer_id)
            .collect();

        // Anon DMs — incoming message requests awaiting acceptance.
        for dm in self.list_anon_dm_message_requests() {
            if !shown_secret_peers.insert(dm.peer_id) {
                continue;
            }
            let peer_name = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| self.fallback_display_name(&dm.peer_id));
            let latest = self.get_latest_anon_dm_message(&dm.id);
            let last_message = if superseded_peers.contains(&dm.peer_id) {
                format!("{} started a new secret chat — Accept / Decline", peer_name)
            } else {
                match &latest {
                    Some(msg) => self.message_sidebar_preview(msg),
                    None => "Message request".to_string(),
                }
            };
            out.push(ChatRowSendData {
                user_id: dm.peer_id.to_string(),
                avatar_bytes: dm.peer_avatar.clone(),
                presence_status: "pending".to_string(),
                last_message,
                timestamp_at: Some(dm.created_at),
                unread_count: 1,
                last_activity: Some(latest.map_or(dm.created_at, |m| m.created_at)),
                label: crate::ui::sidebar_data::ChatRowLabel::IncomingSecretRequest,
                ..ChatRowSendData::new(ConversationType::AnonDm, dm.id.to_string(), peer_name)
            });
        }

        // Anon DMs — active conversations.
        for dm in anon_conversations {
            if dm.status != AnonDmStatus::Active || !shown_secret_peers.insert(dm.peer_id) {
                continue;
            }
            let display_name = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| self.fallback_display_name(&dm.peer_id));
            let latest = self.get_latest_anon_dm_message(&dm.id);
            // The preview every other row uses: "You: " on our own messages
            // (RC108 O5), typed attachment labels.
            let (last_message, timestamp_at) = match &latest {
                Some(msg) => (self.message_sidebar_preview(msg), Some(msg.created_at)),
                None => (String::new(), None),
            };
            out.push(ChatRowSendData {
                user_id: dm.peer_id.to_string(),
                avatar_bytes: dm.peer_avatar.clone(),
                presence_status: "online".to_string(),
                last_message,
                timestamp_at,
                unread_count: dm.unread_count,
                last_activity: latest.map(|m| m.created_at),
                ..ChatRowSendData::new(ConversationType::AnonDm, dm.id.to_string(), display_name)
            });
        }

        // Anon DMs — outgoing requests we sent, awaiting acceptance.
        for dm in self.list_anon_dm_outgoing_requests() {
            if !shown_secret_peers.insert(dm.peer_id) {
                continue;
            }
            let peer_name = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| self.fallback_display_name(&dm.peer_id));
            let latest = self.get_latest_anon_dm_message(&dm.id);
            let (last_message, timestamp_at) = match &latest {
                Some(msg) => {
                    let preview = msg
                        .as_text()
                        .filter(|t| !t.is_empty())
                        .map(|t| plain_sidebar_preview(&format!("You: {}", t)))
                        .unwrap_or_else(|| "Message request sent".to_string());
                    (preview, Some(msg.created_at))
                }
                None => ("Waiting for response...".to_string(), Some(dm.created_at)),
            };
            out.push(ChatRowSendData {
                user_id: dm.peer_id.to_string(),
                avatar_bytes: dm.peer_avatar.clone(),
                presence_status: "offline".to_string(),
                last_message,
                timestamp_at,
                last_activity: Some(latest.map_or(dm.created_at, |m| m.created_at)),
                label: crate::ui::sidebar_data::ChatRowLabel::OutgoingSecretRequest,
                ..ChatRowSendData::new(ConversationType::AnonDm, dm.id.to_string(), peer_name)
            });
        }

        self.finish_dm_sidebar(out)
    }

    /// The chat list: every loose conversation the current mode shows — DMs
    /// (friend DMs and secret chats with their requests), channels (current and former),
    /// private groups — as ONE list, pinned first, then newest activity first
    /// (Signal/Telegram). It used to be a DM block above a channel block, so a
    /// channel with newer activity never rose above a DM. Folders render their
    /// own rows (`list_sections_with_conversations`); every builder here drops
    /// what lives in one.
    pub fn sidebar_rows(&self) -> Vec<crate::ui::sidebar_data::ChatRowSendData> {
        let mut rows = self.list_dm_sidebar();
        rows.extend(self.sidebar_channels().iter().map(|c| self.channel_row(c)));
        rows.extend(
            self.sidebar_private_groups()
                .iter()
                .map(|g| self.private_group_row(g)),
        );
        crate::ui::sidebar_data::order_chat_rows(&mut rows);
        // The local AI channel leads the list while it is on; it cannot be
        // pinned, so no activity order may bury it.
        #[cfg(feature = "jarvis")]
        {
            let jarvis = super::jarvis_channel_id().to_string();
            if let Some(i) = rows.iter().position(|r| r.id == jarvis) {
                let row = rows.remove(i);
                rows.insert(0, row);
            }
        }
        rows
    }

    /// A channel's chat-list row.
    fn channel_row(
        &self,
        channel: &crate::domain::channel::Channel,
    ) -> crate::ui::sidebar_data::ChatRowSendData {
        use crate::ui::sidebar_data::ChatRowSendData;
        let id = channel.id.to_string();
        let latest = self.latest_message(&channel.id, &channel.conversation_type());
        let (last_message, _) = self.sidebar_activity(latest.as_ref());
        // A channel with no messages yet orders (and is dated) by when it was
        // created, so a just-created channel leads the list instead of sinking
        // below every dated row.
        let last_activity = latest
            .map(|m| m.created_at)
            .or(channel.last_message_at)
            .unwrap_or(channel.created_at);
        ChatRowSendData {
            avatar_bytes: channel.icon.clone().filter(|b| !b.is_empty()),
            last_message,
            timestamp_at: Some(last_activity),
            unread_count: channel.unread_count,
            is_pinned: self.is_conversation_pinned(&id),
            private_channel: channel.channel_type == crate::domain::channel::ChannelType::Private,
            can_leave: self.can_leave_channel(&channel.id),
            can_delete: self.is_former_channel(&channel.id),
            last_activity: Some(last_activity),
            ..ChatRowSendData::new(
                crate::domain::message::ConversationType::Channel,
                id,
                channel.name.clone(),
            )
        }
    }

    /// A private (anon) group's chat-list row.
    fn private_group_row(
        &self,
        group: &crate::domain::anon_group::AnonGroup,
    ) -> crate::ui::sidebar_data::ChatRowSendData {
        use crate::ui::sidebar_data::ChatRowSendData;
        let latest = self.get_latest_anon_group_message(&group.id);
        let (last_message, _) = self.sidebar_activity(latest.as_ref());
        // As for channels: no messages yet = ordered by creation.
        let last_activity = latest.map_or(group.created_at, |m| m.created_at);
        ChatRowSendData {
            last_message,
            timestamp_at: Some(last_activity),
            unread_count: self.anon_group_unread_count(&group.id),
            member_count: self.get_anon_group_members(&group.id).len() as u32,
            is_pinned: self.is_conversation_pinned(&group.id.to_string()),
            // Any member may leave; the owner only once someone else owns it
            // or nobody else is left (`AnonGroupService::can_leave`).
            can_leave: self.anon_groups().can_leave(&group.id),
            can_delete: group.status == crate::domain::anon_group::AnonGroupStatus::Removed,
            last_activity: Some(last_activity),
            ..ChatRowSendData::new(
                crate::domain::message::ConversationType::AnonGroup,
                group.id.to_string(),
                group.name.clone(),
            )
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
    /// The conversation a sidebar row's "Delete chat" or "Leave" acts on: a
    /// channel row is its channel, a group row its group, a DM row resolves
    /// as [`Self::resolve_dm_conversation`].
    pub fn resolve_row_conversation(
        &self,
        row_id: &str,
        kind: &crate::domain::message::ConversationType,
    ) -> Option<crate::state::ActiveConversation> {
        match kind {
            crate::domain::message::ConversationType::Channel => row_id
                .parse()
                .ok()
                .map(crate::state::ActiveConversation::Channel),
            crate::domain::message::ConversationType::AnonGroup => row_id
                .parse()
                .ok()
                .map(crate::state::ActiveConversation::AnonGroup),
            _ => self.resolve_dm_conversation(row_id),
        }
    }

    pub fn resolve_dm_conversation(&self, dm_id: &str) -> Option<crate::state::ActiveConversation> {
        use crate::state::ActiveConversation;

        if let Ok(anon_id) = dm_id.parse::<crate::domain::anon_dm::AnonDmId>() {
            if self.get_anon_dm_conversation(&anon_id).is_some() {
                return Some(ActiveConversation::AnonDm(anon_id));
            }
        }
        let peer_id = dm_id.parse::<crate::domain::ids::UserId>().ok()?;
        // A friend's DM is the same conversation in both modes; Shield only
        // changes its transport (`send_friend_dm_content`). Anyone else gets
        // no friend DM in Shield — bootstrapping a mailbox with a stranger
        // takes identified calls — and neither does a peer we know only from
        // an anon group (it would tie their anonymous membership, and ours,
        // to the real accounts). A peer we also know anonymously opens that
        // conversation instead; otherwise there is nothing to open.
        if self.is_friend(&peer_id) {
            return Some(ActiveConversation::ServerDm { peer_id });
        }
        if self.anon_mode_enabled() || self.known_only_from_anon_group(&peer_id) {
            return self
                .find_anon_dm_by_peer(&peer_id)
                .map(ActiveConversation::AnonDm);
        }
        Some(ActiveConversation::ServerDm { peer_id })
    }

    /// Whether `peer` shares an anon group with us and we have no identified
    /// relationship to it — not a friend, no server DM history. For such a
    /// peer the identified DM is a NEW link between the two identities; for a
    /// friend or an existing DM that link already exists. Unreadable anon
    /// membership is treated conservatively until storage is available again.
    fn known_only_from_anon_group(&self, peer: &crate::domain::ids::UserId) -> bool {
        if self.is_friend(peer) {
            return false;
        }
        let has_server_dm = self
            .get_messages_local_asc(
                &crate::domain::ids::ChannelId::from(*peer),
                &ConversationType::ServerDm,
                None,
                1,
            )
            .is_ok_and(|m| !m.is_empty());
        if has_server_dm {
            return false;
        }
        // The sidebar façade turns storage errors into empty lists. Privacy
        // routing must distinguish absence from an unreadable membership:
        // without an established identified relationship, uncertainty may
        // only open an existing anonymous conversation.
        let Ok(groups) = self.anon_groups().list_groups() else {
            return true;
        };
        for group in groups {
            let Ok(members) = self.anon_groups().get_members(&group.id) else {
                return true;
            };
            if members.iter().any(|member| &member.user_id == peer) {
                return true;
            }
        }
        false
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
    ///
    /// Not the picker's list on its own: the secret-chat rules depend on the
    /// messages being forwarded, so the picker asks
    /// [`crate::services::conversation::ConversationHandle::forward_destinations`].
    pub(crate) fn forward_destinations(&self, query: &str) -> Vec<ForwardDestination> {
        use crate::domain::message::ConversationType as CT;

        let mut out: Vec<ForwardDestination> = Vec::new();

        for dm in self.list_dm_sidebar() {
            out.push(ForwardDestination {
                kind: dm.kind,
                id: dm.id,
                name: dm.display_name,
            });
        }
        for channel in self
            .sidebar_channels()
            .into_iter()
            .filter(|c| !self.is_former_channel(&c.id))
        {
            out.push(ForwardDestination {
                kind: CT::Channel,
                id: channel.id.to_string(),
                name: channel.name,
            });
        }
        for group in self.sidebar_private_groups() {
            if !group.status.can_send() {
                continue;
            }
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

    /// Rows for the Friends tab, mode-exclusive like the DM sidebar: shield on
    /// lists only active anon-DM peers (and makes no identified server call);
    /// off lists accepted friends. Anon rows carry the anon DM id in
    /// `user_id` (see `FriendDataSendData`).
    ///
    /// Identified friends are local-first. [`Self::local_friend_tab_entries`]
    /// is what cold start paints; this call replaces those rows when
    /// `ListFriends` succeeds and returns them unchanged when it does not.
    /// An error used to publish an empty vec, so Contacts showed "No friends
    /// yet" while the chat list still rendered the same people from storage.
    pub async fn friends_tab_entries(&self) -> Vec<crate::ui::sidebar_data::FriendDataSendData> {
        if self.anon_mode_enabled() {
            return self.local_friend_tab_entries();
        }
        match self.list_friends_with_profiles().await {
            Ok(pairs) => {
                let mut out = Vec::with_capacity(pairs.len());
                for (friendship, profile) in pairs {
                    // The server already leaves out its own blocks; this also
                    // drops a friend blocked through their anon DM.
                    if self.is_profile_user_blocked(&friendship.addressee_id) {
                        continue;
                    }
                    out.push(crate::ui::sidebar_data::FriendDataSendData {
                        user_id: friendship.addressee_id.to_string(),
                        // The resolver, not the raw row: a row written
                        // without the profile key holds `User <id8>`, and a
                        // known username reads better (REVIEW-R4 P0-3).
                        display_name: self.display_name(&friendship.addressee_id),
                        username: String::new(),
                        avatar_bytes: profile.avatar_bytes.clone(),
                        presence_status: self
                            .peer_presence(&friendship.addressee_id)
                            .as_str()
                            .to_string(),
                        custom_status: String::new(),
                        is_anon: false,
                    });
                }
                out
            }
            Err(e) => {
                tracing::warn!(error = %e, "Friends list: server unreachable, showing the local store");
                self.local_friend_tab_entries()
            }
        }
    }

    /// Friends-tab rows from local storage. No network.
    ///
    /// Shield on: active anon-DM peers. Shield off: accepted friends who are
    /// not blocked (`listed_friends`, either direction of the row). Presence
    /// uses the same privacy-shaped local presence as the DM sidebar. Live
    /// presence events update both views after this snapshot is published.
    pub fn local_friend_tab_entries(&self) -> Vec<crate::ui::sidebar_data::FriendDataSendData> {
        use crate::domain::anon_dm::AnonDmStatus;
        use crate::ui::sidebar_data::FriendDataSendData;

        if self.anon_mode_enabled() {
            let mut out = Vec::new();
            for dm in self.list_anon_dm_conversations() {
                if dm.status != AnonDmStatus::Active {
                    continue;
                }
                out.push(FriendDataSendData {
                    user_id: dm.id.to_string(),
                    display_name: dm
                        .peer_display_name
                        .clone()
                        .unwrap_or_else(|| self.fallback_display_name(&dm.peer_id)),
                    username: String::new(),
                    avatar_bytes: dm.peer_avatar.clone(),
                    presence_status: "online".to_string(),
                    custom_status: String::new(),
                    is_anon: true,
                });
            }
            return out;
        }

        self.listed_friends()
            .into_iter()
            .map(|peer| {
                let avatar_bytes = self.get_profile_local(&peer).and_then(|p| p.avatar_bytes);
                FriendDataSendData {
                    user_id: peer.to_string(),
                    display_name: self.display_name(&peer),
                    username: String::new(),
                    avatar_bytes,
                    presence_status: self.peer_presence(&peer).as_str().to_string(),
                    custom_status: String::new(),
                    is_anon: false,
                }
            })
            .collect()
    }

    /// Local global search excludes device-bound secret chats even though
    /// their rows remain in the sidebar in either mode. Returns matching
    /// regular messages, best match first.
    pub fn search_messages_visible(
        &self,
        query: &str,
        limit: u32,
    ) -> Vec<crate::domain::message::Message> {
        let joined_channels: std::collections::HashSet<_> = self
            .list_channels()
            .unwrap_or_default()
            .into_iter()
            .map(|channel| channel.id)
            .collect();
        let hits = match self.messages().search_local(query, None, limit) {
            Ok(hits) => hits,
            Err(e) => {
                tracing::error!("Search failed: {}", e);
                return Vec::new();
            }
        };
        hits.iter()
            .filter_map(|hit| self.get_message_local(&hit.message_id))
            .filter(|msg| {
                !msg.conversation_type.is_secret_chat()
                    && self.visible_in_mode(&msg.conversation_type)
                    && (msg.conversation_type != crate::domain::message::ConversationType::Channel
                        || joined_channels.contains(&msg.conversation_id))
                    && !self.is_blocked_conversation(
                        &msg.conversation_type,
                        &msg.conversation_id.to_string(),
                    )
            })
            .collect()
    }

    /// Jump-to / quick-switcher rows for `query`, ranked by the core rule
    /// (`services::search::filter_conversations`). Built from the same
    /// mode-filtered sources the sidebar renders — loose rows AND folder rows.
    /// Secret chats stay in the sidebar, but their names must not appear in
    /// the regular global search palette.
    ///
    /// `conv_type` uses the palette's vocabulary: "channel", "dm", "anon_dm",
    /// "private_group", "saved_messages" (id is the sentinel
    /// "saved-messages"), "mesh".
    pub fn quick_switcher_entries(
        &self,
        query: &str,
    ) -> Vec<crate::services::quick_switcher::QuickSwitcherEntry> {
        use crate::domain::message::ConversationType as CT;
        use crate::services::quick_switcher::QuickSwitcherEntry;

        let stats = self
            .services
            .conversation_repo
            .conversation_stats()
            .unwrap_or_default();
        let unread = |id: &str, kind: CT| {
            stats
                .get(&(id.to_string(), kind))
                .map_or(0, |s| s.unread_count)
        };
        let entry =
            |id: String, name: String, conv_type: &str, presence: String, unread_count: u32| {
                QuickSwitcherEntry {
                    id,
                    name,
                    conv_type: conv_type.to_string(),
                    presence_status: presence,
                    unread_count: i32::try_from(unread_count).unwrap_or(i32::MAX),
                }
            };

        let mut entries = Vec::new();
        if !self.anon_mode_enabled() {
            entries.push(entry(
                "saved-messages".into(),
                "Saved Messages".into(),
                "saved_messages",
                String::new(),
                0,
            ));
        }
        for dm in self.list_dm_sidebar() {
            if dm.is_anon_dm() {
                continue;
            }
            entries.push(entry(
                dm.id,
                dm.display_name,
                "dm",
                dm.presence_status,
                dm.unread_count,
            ));
        }
        for channel in self.sidebar_channels() {
            let id = channel.id.to_string();
            let count = unread(&id, channel.conversation_type());
            entries.push(entry(id, channel.name, "channel", String::new(), count));
        }
        for section in self.list_sections_with_conversations().unwrap_or_default() {
            for conv in section.conversations {
                if conv.conversation_type.is_secret_chat() {
                    continue;
                }
                let kind = match conv.conversation_type {
                    CT::Channel => "channel",
                    CT::ServerDm => "dm",
                    CT::AnonDm => "anon_dm",
                    CT::AnonGroup => "private_group",
                    CT::Mesh | CT::MeshDm => "mesh",
                    // Saved Messages has its own row.
                    CT::SavedMessages => continue,
                };
                entries.push(entry(
                    conv.conversation_id,
                    conv.display_name,
                    kind,
                    String::new(),
                    conv.unread_count,
                ));
            }
        }
        #[cfg(feature = "mesh")]
        let sectioned = self
            .sections()
            .all_sectioned_conversation_ids()
            .unwrap_or_default();
        #[cfg(feature = "mesh")]
        for conv in self.mesh_conversations().unwrap_or_default() {
            // Foldered rows came from the section loop; unresolvable ones
            // would open nothing.
            if sectioned.contains(&conv.id) || self.mesh_conversation_for(&conv.id).is_none() {
                continue;
            }
            entries.push(entry(
                conv.id,
                conv.name,
                "mesh",
                String::new(),
                conv.unread_count,
            ));
        }

        crate::services::search::filter_conversations(query, entries)
    }

    /// Where a message search hit opens, as the `(conv_type, id)` pair a
    /// [`Self::quick_switcher_entries`] row for its conversation carries — so
    /// a message hit and a conversation hit open through one path. A friend
    /// DM's `conversation_id` is the peer (the DM row's id); Saved Messages
    /// opens by its sentinel.
    pub fn search_hit_target(
        &self,
        msg: &crate::domain::message::Message,
    ) -> (&'static str, String) {
        use crate::domain::message::ConversationType as CT;
        let kind = match msg.conversation_type {
            CT::Channel => "channel",
            CT::ServerDm => "dm",
            CT::AnonDm => "anon_dm",
            CT::AnonGroup => "private_group",
            CT::SavedMessages => return ("saved_messages", "saved-messages".to_string()),
            CT::Mesh | CT::MeshDm => "mesh",
        };
        (kind, msg.conversation_id.to_string())
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
            if name_lower.contains(&query_lower)
                || user_id_str.to_lowercase().contains(&query_lower)
            {
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
    ) -> Result<Vec<crate::domain::channel::Membership>, crate::services::channel::ChannelError>
    {
        self.channels()
            .get_channel_members(channel_id, limit, offset)
            .await
    }

    pub async fn update_channel_member_role(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        user_id: &crate::domain::ids::UserId,
        new_role: crate::domain::channel::MemberRole,
    ) -> Result<crate::domain::channel::Membership, crate::services::channel::ChannelError> {
        self.channels()
            .update_member_role(channel_id, user_id, new_role)
            .await
    }

    pub async fn delete_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels().delete_channel(channel_id).await
    }

    /// Keep only invite choices that are selected and currently eligible.
    pub fn eligible_channel_invite_targets<I>(&self, rows: I) -> Vec<crate::domain::ids::UserId>
    where
        I: IntoIterator<Item = (String, bool, bool, bool)>,
    {
        rows.into_iter()
            .filter_map(|(id, selected, already_member, banned)| {
                if selected && !already_member && !banned {
                    id.parse().ok()
                } else {
                    None
                }
            })
            .collect()
    }

    pub async fn invite_user_to_channel(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        invitee_id: &crate::domain::ids::UserId,
        message: Option<&str>,
    ) -> Result<(), crate::services::channel::ChannelError> {
        self.channels()
            .invite_user(channel_id, invitee_id, message)
            .await
    }

    /// List the current user's pending channel invitations (server scopes to
    /// PENDING + unexpired). Best-effort — empty on failure.
    pub async fn list_pending_channel_invites(
        &self,
    ) -> Vec<crate::proto::chat_v1::PendingChannelInvite> {
        let mut client = self.services.grpc_pool.channel_client();
        match client.list_pending_invites().await {
            Ok(mut resp) => {
                resp.invites.sort_by(|a, b| {
                    a.created_at
                        .cmp(&b.created_at)
                        .then_with(|| a.channel_id.cmp(&b.channel_id))
                });
                resp.invites
            }
            Err(e) => {
                tracing::debug!(error = %e, "list_pending_channel_invites failed");
                Vec::new()
            }
        }
    }

    /// Pending invitations with unknown inviter IDs hidden from the display.
    pub async fn channel_invite_rows(&self) -> Vec<ChannelInviteRow> {
        self.list_pending_channel_invites()
            .await
            .into_iter()
            .map(|invite| {
                let channel_name = if invite.channel_name.is_empty() {
                    "Channel invite".to_string()
                } else {
                    format!("#{}", invite.channel_name)
                };
                let invited_by = invite
                    .inviter_id
                    .parse::<crate::domain::ids::UserId>()
                    .ok()
                    .map(|user_id| self.display_name(&user_id))
                    .filter(|name| *name != invite.inviter_id)
                    .map(|name| format!("Invited by {name}"))
                    .unwrap_or_default();
                ChannelInviteRow {
                    channel_id: invite.channel_id,
                    channel_name,
                    invited_by,
                    created_at: invite.created_at,
                    note: invite.message,
                }
            })
            .collect()
    }

    /// Accept or decline a channel invitation. Returns the channel on accept,
    /// `None` on decline.
    ///
    /// Accepting is a join, so this finishes the join the same way
    /// [`ChatterClient::join_channel`] — the other way into a channel — does.
    /// The service writes the channel and membership rows; on top of that this
    /// nudges the live message stream and pulls the existing members' sender
    /// keys. Without those three the user was a member on the server with
    /// nothing locally: no channel in the sidebar and no live message in it
    /// until the next sync (OPEN-ITEMS §69).
    pub async fn respond_to_channel_invite(
        self: &Arc<Self>,
        channel_id: &crate::domain::ids::ChannelId,
        accept: bool,
    ) -> Result<Option<Channel>, crate::services::channel::ChannelError> {
        let Some(channel) = self
            .channels()
            .respond_to_invite(channel_id, accept)
            .await?
        else {
            return Ok(None);
        };

        // Nudge the stream FIRST, ahead of the slower calls below. A brand-new
        // invitee was in no channel when their client started, so the server
        // refused their global stream and the stream task is sitting in its
        // reconnect backoff with no subscription at all — the membership
        // change the server publishes on accept (§68) reaches nobody. Starting
        // the reconnect here lets its catch-up run while the sender-key work
        // below is still going, rather than inside the window the user is
        // waiting for their first message in.
        self.resubscribe_channel_messages();

        // The SKDM request below asks every member the LOCAL membership list
        // names, so the member list has to be local first.
        if let Err(e) = self.channels().sync_channel_members(channel_id).await {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to sync channel members after accepting an invite"
            );
        }

        // Pull SKDMs from the existing members so their next message decrypts
        // without the no-sender-key defer/retry round-trip.
        if let Err(e) = self
            .messages()
            .request_skdms_from_channel_members(channel_id)
            .await
        {
            tracing::warn!(
                channel_id = %channel_id,
                error = %e,
                "Failed to request SKDMs after accepting an invite"
            );
        }

        Ok(Some(channel))
    }

    /// The invite picker's search: [`Self::list_channel_invite_candidates`]
    /// narrowed to `query` ([`InviteCandidate::matches`]), plus — when the
    /// query is a complete handle — the account that exact username resolves
    /// to (Signal-style: display names are profile-key-encrypted on the
    /// server, so only an exact username can reach a stranger).
    ///
    /// The lookup waits for typing to pause, so `alice.47` → `alice.471` →
    /// `alice.4711` does not probe every discriminator on the way: after the
    /// pause, `superseded()` is asked and a newer keystroke's search wins
    /// (`None`).
    pub async fn search_channel_invite_candidates(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
        query: &str,
        superseded: impl Fn() -> bool,
    ) -> Option<Vec<InviteCandidate>> {
        let mut cands = self
            .list_channel_invite_candidates(channel_id)
            .await
            .unwrap_or_default();
        let trimmed = query.trim();
        cands.retain(|c| c.matches(trimmed));
        if !crate::services::user::is_lookup_handle(trimmed) {
            return Some(cands);
        }

        tokio::time::sleep(Self::INVITE_LOOKUP_PAUSE).await;
        if superseded() {
            return None;
        }
        match self.lookup_by_username(trimmed).await {
            Ok(Some(user_id)) if !cands.iter().any(|c| c.user_id == user_id) => {
                match self.get_user_profile(&user_id).await {
                    Ok(profile) => cands.push(InviteCandidate {
                        user_id,
                        display_name: profile.display_name,
                        username: self
                            .known_username(&user_id)
                            .unwrap_or_else(|| trimmed.to_string()),
                        avatar_bytes: profile.avatar_bytes.unwrap_or_default(),
                        // A non-contact found by exact username; if already a
                        // member or banned, the invite RPC rejects it server-side.
                        already_member: false,
                        banned: false,
                    }),
                    Err(e) => {
                        tracing::warn!(error = %e, %user_id, "lookup hit but profile fetch failed")
                    }
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, query = %trimmed, "username lookup failed"),
        }
        Some(cands)
    }

    /// How long typing must pause on a complete handle before it is looked up.
    const INVITE_LOOKUP_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

    /// Build the list of people the user can invite to `channel_id`: their DM
    /// contacts (regular server DMs + anon DMs — the same set shown in the
    /// sidebar), deduped, with `already_member` flagged for current members.
    pub async fn list_channel_invite_candidates(
        &self,
        channel_id: &crate::domain::ids::ChannelId,
    ) -> Result<Vec<InviteCandidate>, crate::services::channel::ChannelError> {
        use crate::domain::ids::UserId;
        use std::collections::HashSet;

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
        // Active bans, visible to owner/admin only (`None` otherwise; the
        // server then refuses a banned invitee with its own reason).
        let banned: HashSet<UserId> = self
            .channels()
            .list_banned_users(channel_id)
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
            .into_iter()
            .map(|b| b.user_id)
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
            let username = self.known_username(&user_id).unwrap_or_default();
            let avatar_bytes = self
                .get_profile_local(&user_id)
                .and_then(|p| p.avatar_bytes)
                .unwrap_or_default();
            out.push(InviteCandidate {
                already_member: members.contains(&user_id),
                banned: banned.contains(&user_id),
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
            for peer_id in self.listed_friends() {
                let display_name = self.display_name(&peer_id);
                push(&mut out, &mut seen, peer_id, display_name);
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

    pub async fn search_channels_filtered(
        &self,
        query: &str,
        limit: u32,
        offset: u32,
        type_filter: Vec<crate::domain::channel::ChannelType>,
    ) -> Result<Vec<crate::domain::channel::Channel>, crate::services::channel::ChannelError> {
        self.channels()
            .search_channels(query, limit, offset, type_filter)
            .await
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

    /// The newest `limit` replies before `before` (or overall), oldest →
    /// newest. Page backwards by passing the first returned reply's id.
    pub fn list_thread_replies(
        &self,
        thread_root_id: &crate::domain::ids::MessageId,
        before: Option<&crate::domain::ids::MessageId>,
        limit: u32,
    ) -> Result<Vec<crate::domain::message::Message>, crate::services::message::MessageError> {
        self.messages()
            .list_for_thread(thread_root_id, before, limit)
    }

    /// Total replies in a thread, so the panel knows whether earlier ones
    /// remain to be loaded.
    pub fn thread_reply_count(&self, thread_root_id: &crate::domain::ids::MessageId) -> u32 {
        self.messages()
            .count_for_thread(thread_root_id)
            .unwrap_or(0)
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
    /// Actively banned: not invitable until unbanned.
    pub banned: bool,
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
    /// Effective message id to render in the UI: the stored message's id,
    /// also for a send queued behind a DM bootstrap; `"pending-{sent_at}"`
    /// only when nothing was stored.
    pub message_id_effective: String,
    /// Server-assigned sent-at timestamp in nanoseconds.
    pub sent_at: u64,
    /// Local-time `HH:MM` string for message header rendering.
    pub timestamp_display: String,
    /// Sidebar preview with "You: " prefix, truncated to sidebar width.
    pub sidebar_preview: String,
    /// `"sent"` (delivered), `"sending"` (DM bootstrap in flight),
    /// `"unconfirmed"` (submitted, outcome unknown, no retry), or
    /// `"failed"` (stored, not delivered — see `failure_notice`).
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
    /// Why the message was not delivered, for the user, when
    /// `delivery_status` is `"failed"`.
    pub failure_notice: Option<String>,
    pub failure_reason: Option<String>,
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

#[cfg(test)]
mod shield_mode_tests {
    //! Shield (anon) mode is enforced in core: identity-revealing calls refuse
    //! or take the anon path themselves, whatever the UI offers.
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use crate::client::ChatterClient;
    use crate::domain::ids::UserId;
    use crate::domain::message::ConversationType;
    use crate::services::user::UserError;
    use crate::state::ActiveConversation;

    fn build_test_client() -> (tempfile::TempDir, Arc<ChatterClient>) {
        // A real client stores its DB passphrase in the OS keyring; point
        // keyring at the in-memory mock (see `transport_switch_tests`).
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        (dir, client)
    }

    #[tokio::test]
    async fn shield_mode_turns_the_gif_picker_off() {
        let (_dir, client) = build_test_client();
        assert!(client.gif_picker_available_for(&ConversationType::ServerDm));
        assert!(client.gif_picker_available());
        client.anon_mode.store(true, Ordering::Release);
        // GifService takes account auth: Shield must not reach it, from any
        // conversation, nor search or fetch through it.
        assert!(!client.gif_picker_available_for(&ConversationType::ServerDm));
        assert!(!client.gif_picker_available_for(&ConversationType::Channel));
        assert!(!client.gif_picker_available());
        assert!(client.search_gifs("cat", 1).await.is_err());
        assert!(
            client
                .fetch_gif_media("https://localhost:1/x.gif")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn shield_mode_refuses_identified_calls() {
        let (_dir, client) = build_test_client();
        client.anon_mode.store(true, Ordering::Release);
        let peer = UserId::new();

        assert!(matches!(
            client.send_friend_request(&peer, None).await,
            Err(UserError::ForbiddenInAnonMode)
        ));
        assert!(matches!(
            client
                .connect_via_friend_code("chatter://connect/x", None)
                .await,
            Err(UserError::ForbiddenInAnonMode)
        ));
        assert!(matches!(
            client.lookup_by_username("alice.4711").await,
            Err(UserError::HandleLookupForbiddenInAnonMode)
        ));
        // Not a friend and no anon DM with this peer: shield mode has nothing
        // to open, and never falls back to the identified DM.
        assert_eq!(client.resolve_dm_conversation(&peer.to_string()), None);
        // A friend's DM opens in shield mode too — the same conversation,
        // whose transport is the mailbox there.
        let (me, friend) = (UserId::new(), UserId::new());
        let users = &client.messages().user_repo;
        client
            .messages()
            .settings_repo
            .set_string(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &me.to_string(),
            )
            .unwrap();
        users.ensure_user_exists(&me).unwrap();
        users.ensure_user_exists(&friend).unwrap();
        let mut friendship = crate::domain::user::Friendship::new_request(me, friend);
        friendship.accept();
        users.insert_friendship(&friendship).unwrap();
        assert_eq!(
            client.resolve_dm_conversation(&friend.to_string()),
            Some(ActiveConversation::ServerDm { peer_id: friend })
        );
        // The Friends tab makes no identified call and lists no friend.
        assert!(client.friends_tab_entries().await.is_empty());
        // Search and jump-to hide channels; a friend DM shows in both modes.
        assert!(client.visible_in_mode(&ConversationType::ServerDm));
        assert!(!client.visible_in_mode(&ConversationType::SavedMessages));
        assert!(client.visible_in_mode(&ConversationType::AnonDm));
        assert!(
            client
                .quick_switcher_entries("")
                .iter()
                .all(|e| e.conv_type != "saved_messages")
        );

        client.anon_mode.store(false, Ordering::Release);
        assert_eq!(
            client.resolve_dm_conversation(&peer.to_string()),
            Some(ActiveConversation::ServerDm { peer_id: peer })
        );
        // Secret chats (anon DMs) show outside Shield too.
        assert!(client.visible_in_mode(&ConversationType::AnonDm));
    }

    /// With Shield off, a peer known only from an anon group never gets an
    /// identified DM: opening one would link their anonymous membership to
    /// their account. Anyone else still does.
    #[tokio::test]
    async fn anon_group_comember_never_gets_an_identified_dm() {
        use crate::domain::anon_group::{AnonGroup, AnonGroupMember, AnonGroupRole};

        let (_dir, client) = build_test_client();
        let (me, member, stranger) = (UserId::new(), UserId::new(), UserId::new());
        let group = AnonGroup::new_as_owner("g".into(), me);
        let repo = &client.anon_groups().anon_group_repo;
        repo.insert_group(&group).unwrap();
        repo.add_member(&AnonGroupMember::new(
            group.id,
            member,
            AnonGroupRole::Member,
        ))
        .unwrap();

        assert!(!client.anon_mode_enabled());
        // No anon DM with the member either: nothing to open.
        assert_eq!(client.resolve_dm_conversation(&member.to_string()), None);
        assert_eq!(
            client.resolve_dm_conversation(&stranger.to_string()),
            Some(ActiveConversation::ServerDm { peer_id: stranger })
        );
    }

    /// Holding the existing readers forces the pool to open another connection
    /// by path, which requires the fixture's directory to remain alive.
    #[tokio::test]
    async fn fixture_keeps_database_available_to_new_readers() {
        let (dir, client) = build_test_client();
        let db = client.messages().database();
        let readers: Vec<_> = (0..3).map(|_| db.read_connection().unwrap()).collect();
        assert!(dir.path().exists());
        let reader = db
            .read_connection()
            .expect("new reader can open the live DB path");
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM anon_groups", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        drop(reader);
        drop(readers);
    }

    /// Existing pooled descriptors may survive unlinking; a fresh path-based
    /// reader must fail. Membership SQL-error coverage below checks routing.
    #[tokio::test]
    async fn deleted_fixture_directory_prevents_fresh_reader() {
        let (dir, client) = build_test_client();
        let db = client.messages().database();
        assert!(db.open_reader_connection().is_ok());
        dir.close().expect("remove fixture directory");
        assert!(!db.path().exists());
        assert!(db.open_reader_connection().is_err());
    }

    /// Break actual SQL reads at each repository boundary, rather than mocking
    /// a membership boolean: an unreadable table cannot authorize a new link.
    #[tokio::test]
    async fn anon_membership_read_errors_do_not_open_identified_dms() {
        use crate::domain::anon_group::AnonGroup;
        use crate::domain::message::Message;
        use crate::domain::user::Friendship;
        use crate::storage::repositories::settings_repo::keys;

        for break_groups in [true, false] {
            let (_dir, client) = build_test_client();
            let (me, stranger, known, friend) =
                (UserId::new(), UserId::new(), UserId::new(), UserId::new());
            let group = AnonGroup::new_as_owner("g".into(), me);
            client
                .anon_groups()
                .anon_group_repo
                .insert_group(&group)
                .unwrap();
            client
                .messages()
                .settings_repo
                .set_string(keys::CURRENT_USER_ID, &me.to_string())
                .unwrap();
            for user in [me, known, friend] {
                client
                    .messages()
                    .user_repo
                    .ensure_user_exists(&user)
                    .unwrap();
            }
            let mut friendship = Friendship::new_request(me, friend);
            friendship.accept();
            client
                .messages()
                .user_repo
                .insert_friendship(&friendship)
                .unwrap();
            let message = Message::new_text(
                known.into(),
                ConversationType::ServerDm,
                Some(known),
                "hello".into(),
                1,
            );
            client.messages().message_repo.insert(&message).unwrap();
            let db = client.messages().database();
            db.connection()
                .unwrap()
                .execute_batch(if break_groups {
                    "ALTER TABLE anon_groups RENAME COLUMN owner_id TO unreadable_owner_id"
                } else {
                    "ALTER TABLE anon_group_members RENAME COLUMN user_id TO unreadable_user_id"
                })
                .unwrap();
            if break_groups {
                assert!(client.anon_groups().list_groups().is_err());
            } else {
                assert!(client.anon_groups().list_groups().is_ok());
                assert!(client.anon_groups().get_members(&group.id).is_err());
            }
            assert_eq!(client.resolve_dm_conversation(&stranger.to_string()), None);
            for peer_id in [known, friend] {
                assert_eq!(
                    client.resolve_dm_conversation(&peer_id.to_string()),
                    Some(ActiveConversation::ServerDm { peer_id })
                );
            }
            client.anon_mode.store(true, Ordering::Release);
            assert_eq!(client.resolve_dm_conversation(&known.to_string()), None);
            assert_eq!(
                client.resolve_dm_conversation(&friend.to_string()),
                Some(ActiveConversation::ServerDm { peer_id: friend })
            );
        }
    }
}

#[cfg(test)]
mod local_friends_tests {
    //! Contacts › Friends must render from the friendships table when the
    //! server cannot be reached. The builder points at a closed port.
    use std::sync::Arc;

    use crate::client::ChatterClient;
    use crate::domain::ids::UserId;
    use crate::domain::user::{FriendshipStatus, PresenceStatus, Profile};

    fn build_test_client() -> Arc<ChatterClient> {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        ChatterClient::builder()
            .server_url("https://127.0.0.1:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client")
    }

    #[tokio::test]
    async fn local_friends_list_without_network() {
        let client = build_test_client();
        let (me, friend, pending, blocked) =
            (UserId::new(), UserId::new(), UserId::new(), UserId::new());
        client.auth_state().set_logged_in(me, 1, "Me".into());
        client
            .messages()
            .settings_repo
            .set_string(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &me.to_string(),
            )
            .unwrap();
        let users = &client.messages().user_repo;
        for id in [me, friend, pending, blocked] {
            users.ensure_user_exists(&id).unwrap();
        }
        let mut accepted = crate::domain::user::Friendship::new_request(me, friend);
        accepted.accept();
        users.insert_friendship(&accepted).unwrap();
        users
            .insert_friendship(&crate::domain::user::Friendship::new_request(me, pending))
            .unwrap();
        let mut blocked_row = crate::domain::user::Friendship::new_request(me, blocked);
        blocked_row.status = FriendshipStatus::Blocked;
        users.insert_friendship(&blocked_row).unwrap();

        // Stored presence is Online from the last session. Offline render
        // must not repeat that.
        let mut profile = Profile::new(friend, "Ada Lovelace".into());
        profile.avatar_bytes = Some(vec![1, 2, 3, 4]);
        profile.presence = PresenceStatus::Online;
        users.upsert_profile(&profile).unwrap();

        let local = client.local_friend_tab_entries();
        assert_eq!(local.len(), 1, "pending and blocked rows are not friends");
        assert_eq!(local[0].user_id, friend.to_string());
        assert_eq!(local[0].display_name, "Ada Lovelace");
        assert_eq!(local[0].avatar_bytes.as_deref(), Some(&[1, 2, 3, 4][..]));
        assert_eq!(local[0].presence_status, "offline");
        assert!(!local[0].is_anon);

        // ListFriends against a closed port fails. The tab still returns
        // the stored friend instead of an empty vec.
        let via_tab = client.friends_tab_entries().await;
        assert_eq!(via_tab.len(), 1);
        assert_eq!(via_tab[0].user_id, friend.to_string());
        assert_eq!(via_tab[0].display_name, "Ada Lovelace");
        assert_eq!(via_tab[0].avatar_bytes.as_deref(), Some(&[1, 2, 3, 4][..]));
        assert_eq!(via_tab[0].presence_status, "offline");
    }
}

#[cfg(test)]
mod search_hit_tests {
    use crate::client::ChatterClient;
    use crate::domain::channel::{
        Channel, ChannelType, MemberRole, Membership, NotificationPreference,
    };
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::{ConversationType, Message};

    /// A message hit opens the conversation its row opens: a friend DM by its
    /// peer (the DM row's id), Saved Messages by its sentinel. A DM hit once
    /// left the previous chat on screen and a Saved hit opened nothing.
    #[tokio::test]
    async fn dm_and_saved_hits_open_their_own_conversation() {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let peer = UserId::new();
        let store = |id: ChannelId, kind: ConversationType, text: &str, n: u64| {
            let m = Message::new_text(id, kind, Some(peer), text.to_string(), n);
            client
                .messages()
                .message_repo
                .insert(&m)
                .expect("store message");
        };
        store(peer.into(), ConversationType::ServerDm, "badge two", 1);
        store(
            ChannelId::new(),
            ConversationType::SavedMessages,
            "badge note",
            2,
        );

        let mut targets: Vec<_> = client
            .search_messages_visible("badge", 10)
            .iter()
            .map(|m| client.search_hit_target(m))
            .collect();
        targets.sort();
        assert_eq!(
            targets,
            vec![
                ("dm", peer.to_string()),
                ("saved_messages", "saved-messages".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn global_search_does_not_surface_messages_from_left_channels() {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let me = UserId::new();
        client
            .messages()
            .settings_repo
            .set_string(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &me.to_string(),
            )
            .unwrap();
        client.messages().user_repo.ensure_user_exists(&me).unwrap();
        let joined = ChannelId::new();
        let left = ChannelId::new();
        for (id, name) in [(joined, "joined"), (left, "left")] {
            client
                .services
                .channel_repo
                .insert(&Channel::new(id, name.into(), ChannelType::Public, me))
                .unwrap();
            client
                .messages()
                .message_repo
                .insert(&Message::new_text(
                    id,
                    ConversationType::Channel,
                    Some(me),
                    "search privacy".into(),
                    1,
                ))
                .unwrap();
        }
        client
            .services
            .channel_repo
            .insert_membership(&Membership {
                channel_id: joined,
                user_id: me,
                role: MemberRole::Owner,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
                notification_preference: NotificationPreference::All,
            })
            .unwrap();
        let hits = client.search_messages_visible("search privacy", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].conversation_id, joined);
    }
}

#[cfg(test)]
mod new_channel_row_tests {
    use crate::client::ChatterClient;
    use crate::domain::channel::{
        Channel, ChannelType, MemberRole, Membership, NotificationPreference,
    };
    use crate::domain::ids::{ChannelId, UserId};

    /// A just-created channel has no messages. It used to sort below every
    /// dated row with no time; it orders and is dated by its creation.
    #[tokio::test]
    async fn a_new_channel_with_no_messages_leads_by_creation_time() {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let me = UserId::new();
        client
            .messages()
            .settings_repo
            .set_string(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &me.to_string(),
            )
            .unwrap();
        client.messages().user_repo.ensure_user_exists(&me).unwrap();
        let repo = &client.services.channel_repo;
        let join = |name: &str, last: Option<chrono::DateTime<chrono::Utc>>| {
            let mut c = Channel::new(ChannelId::new(), name.into(), ChannelType::Public, me);
            c.last_message_at = last;
            repo.insert(&c).unwrap();
            repo.insert_membership(&Membership {
                channel_id: c.id,
                user_id: me,
                role: MemberRole::Owner,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
                notification_preference: NotificationPreference::All,
            })
            .unwrap();
            c.id.to_string()
        };
        let old = join("old", Some(chrono::Utc::now() - chrono::Duration::hours(1)));
        let new = join("new", None);

        let rows = client.sidebar_rows();
        let ids: Vec<_> = rows.iter().map(|r| r.id.clone()).collect();
        assert_eq!(ids, [new, old], "the new channel leads the list");
        assert!(
            rows[0].timestamp_at.is_some(),
            "the new channel has its creation time"
        );
    }
}

#[cfg(test)]
mod older_messages_tests {
    use crate::client::ChatterClient;
    use crate::domain::ids::{ChannelId, MessageId, UserId};
    use crate::domain::message::{ConversationType, Message};

    /// 120 messages, `sent_at` 1..=120, and their ids in that order.
    fn conversation() -> (std::sync::Arc<ChatterClient>, ChannelId, Vec<MessageId>) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let (conv, peer) = (ChannelId::new(), UserId::new());
        let ids = (1..=120u64)
            .map(|ts| {
                let msg = Message::new_text(
                    conv,
                    ConversationType::ServerDm,
                    Some(peer),
                    "hi".into(),
                    ts,
                );
                client.messages().store_message_local(&msg).expect("store");
                msg.id
            })
            .collect();
        (client, conv, ids)
    }

    /// Device sweep RC25: after a cold start nothing older than the newest
    /// page ever loaded. Paging from the oldest loaded row walks the whole
    /// history, a page at a time, and ends on an empty page.
    #[tokio::test]
    async fn pages_back_to_the_beginning() {
        let (client, conv, ids) = conversation();
        let kind = ConversationType::ServerDm;
        let page = |before: &MessageId| {
            let rows = client
                .get_older_messages_local_asc(&conv, &kind, before, None)
                .unwrap();
            rows.iter().map(|m| m.sent_at).collect::<Vec<_>>()
        };
        // The open loaded 71..=120.
        assert_eq!(page(&ids[70]), (21..=70).collect::<Vec<u64>>());
        assert_eq!(page(&ids[20]), (1..=20).collect::<Vec<u64>>());
        assert!(page(&ids[0]).is_empty());
    }

    /// A jump to a message older than what is loaded brings in every page
    /// back to it — and nothing for a target this list cannot hold.
    #[tokio::test]
    async fn pages_until_the_jump_target() {
        let (client, conv, ids) = conversation();
        let kind = ConversationType::ServerDm;
        let until = |target: &MessageId| {
            let rows = client
                .get_older_messages_local_asc(&conv, &kind, &ids[100], Some(target))
                .unwrap();
            rows.iter().map(|m| m.sent_at).collect::<Vec<_>>()
        };
        // Row 101 is the oldest loaded; the target is two pages up. Whole
        // pages, contiguous with what is loaded.
        assert_eq!(until(&ids[29]), (1..=100).collect::<Vec<u64>>());
        assert_eq!(until(&ids[80]), (51..=100).collect::<Vec<u64>>());

        let elsewhere = Message::new_text(
            ChannelId::new(),
            ConversationType::ServerDm,
            Some(UserId::new()),
            "other conversation".into(),
            5,
        );
        client
            .messages()
            .store_message_local(&elsewhere)
            .expect("store");
        assert!(
            until(&elsewhere.id).is_empty(),
            "a target in another conversation"
        );
        assert!(
            until(&MessageId::new()).is_empty(),
            "a target that is not stored"
        );
    }
}

#[cfg(test)]
mod section_row_tests {
    //! Folder rows are shaped by core (`section_row`), like loose rows.
    use crate::client::ChatterClient;
    use crate::domain::ids::{ChannelId, UserId};
    use crate::domain::message::{ConversationType, Message};
    use crate::domain::section::SectionConversation;

    fn client() -> (std::sync::Arc<ChatterClient>, tempfile::TempDir) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_store(crate::system::keyring::test_credential_store());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        (client, dir)
    }

    fn conv(id: String, kind: ConversationType, name: &str) -> SectionConversation {
        SectionConversation {
            conversation_id: id,
            conversation_type: kind,
            display_name: name.into(),
            unread_count: 3,
            last_message_at: None,
            is_pinned: true,
        }
    }

    #[tokio::test]
    async fn group_row_offers_leave_and_carries_badge_and_name() {
        let (client, _dir) = client();
        // A group we belong to (not its owner): Leave is offered.
        client
            .messages()
            .settings_repo
            .set(
                crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID,
                &UserId::new().to_string(),
            )
            .unwrap();
        let id = crate::domain::anon_group::AnonGroupId::new();
        client
            .anon_groups()
            .anon_group_repo
            .insert_group(&crate::domain::anon_group::AnonGroup::from_invite(
                id,
                "Book Club".into(),
                UserId::new(),
            ))
            .unwrap();
        let row = client.section_row(&conv(
            id.to_string(),
            ConversationType::AnonGroup,
            "Book Club",
        ));
        assert_eq!(row.display_name, "Book Club");
        assert!(row.can_leave && !row.can_delete);
        assert!(row.is_pinned);
        assert_eq!(row.unread_count, 3);
        // Groups take no preview in a folder (as before the move).
        assert!(row.last_message.is_empty());
    }

    #[tokio::test]
    async fn unknown_channel_offers_neither_leave_nor_delete() {
        let (client, _dir) = client();
        let row = client.section_row(&conv(
            ChannelId::new().to_string(),
            ConversationType::Channel,
            "general",
        ));
        assert!(!row.can_leave && !row.can_delete);
        assert!(!row.private_channel);
        assert!(row.avatar_bytes.is_none());
    }

    #[tokio::test]
    async fn dm_row_falls_back_to_the_peer_name_and_shows_its_preview() {
        let (client, _dir) = client();
        let peer = UserId::new();
        let id: ChannelId = peer.into();
        let msg = Message::new_text(
            id,
            ConversationType::ServerDm,
            Some(peer),
            "hello there".into(),
            5,
        );
        client.messages().store_message_local(&msg).unwrap();

        let row = client.section_row(&conv(peer.to_string(), ConversationType::ServerDm, ""));
        assert_eq!(row.display_name, client.display_name(&peer));
        assert!(!row.display_name.is_empty());
        assert!(
            row.last_message.contains("hello there"),
            "{}",
            row.last_message
        );
        assert_eq!(row.presence_status, "offline");
        assert!(!row.can_leave);
    }

    /// The chat-list preview follows an edit and a delete-for-everyone of the
    /// conversation's last message, on both ends: each is in storage before
    /// its `MessageEdited` / `MessageDeleted` goes out, so the row rebuild the
    /// UI runs on those events shows it. The received delete used to leave
    /// the row for a separate event subscriber to remove, racing the rebuild.
    #[tokio::test]
    async fn dm_preview_follows_edits_and_a_deleted_last_message() {
        use crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID;
        let (client, _dir) = client();
        let me = UserId::new();
        client
            .messages()
            .settings_repo
            .set(CURRENT_USER_ID, &me.to_string())
            .unwrap();
        client.auth_state().set_logged_in(me, 1, "me".into());
        let peer = UserId::new();
        let id: ChannelId = peer.into();
        let sent = 1_700_000_000_000_000_000u64;
        let store = |from: UserId, text: &str, at: u64| {
            let msg =
                Message::new_text(id, ConversationType::ServerDm, Some(from), text.into(), at);
            client.messages().store_message_local(&msg).unwrap();
            msg
        };
        let preview = || {
            client
                .section_row(&conv(peer.to_string(), ConversationType::ServerDm, ""))
                .last_message
        };

        // Receiver: the peer edits, then deletes, its last message.
        store(peer, "earlier", sent);
        store(peer, "routing check harmless", sent + 1_000);
        assert_eq!(preview(), "routing check harmless");
        let m = client.messages();
        m.apply_dm_edit(
            &peer,
            sent + 1_000,
            "edited from S9",
            sent + 2_000,
            sent + 2_000,
        )
        .unwrap()
        .expect("applied");
        assert_eq!(preview(), "edited from S9");
        m.apply_targeted_dm_delete(&peer, Some(&peer.to_string()), sent + 1_000)
            .unwrap()
            .expect("matched");
        assert_eq!(preview(), "earlier");

        // Sender: our own edit is persisted the way `send_edit` does it.
        let own = store(me, "routing check harmless", sent + 3_000);
        assert_eq!(preview(), "You: routing check harmless");
        let edited = crate::domain::message::MessageContent::Text("edited from S9".into());
        m.update_message_body(&own.id, &edited).unwrap();
        assert_eq!(preview(), "You: edited from S9");
    }

    /// The invite search: a newer keystroke during the pre-lookup pause
    /// retires a complete-handle search before it reaches the server; a
    /// partial handle never pauses or looks up.
    #[tokio::test]
    async fn invite_search_is_superseded_before_any_lookup() {
        let (client, _dir) = client();
        let channel = ChannelId::new();
        let started = std::time::Instant::now();
        assert!(
            client
                .search_channel_invite_candidates(&channel, "alice.4711", || true)
                .await
                .is_none()
        );
        assert!(started.elapsed() >= ChatterClient::INVITE_LOOKUP_PAUSE);

        // Not a complete handle: no pause, so nothing to supersede.
        let cands = client
            .search_channel_invite_candidates(&channel, "alice.", || true)
            .await;
        assert_eq!(cands.map(|c| c.len()), Some(0));
    }
}
