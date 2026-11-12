//! Message rows as the chat list shows them, before any toolkit type.
//!
//! [`MessageRow`] is everything a bubble needs that core can decide: names,
//! body text, reply context, call labels, counts. The UI turns it into its
//! own row struct and decodes images; nothing here touches pixels.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use crate::client::ChatterClient;
use crate::ui::display::ReplyParentMedia;
use crate::domain::attachment::Attachment;
use crate::domain::ids::{AttachmentId, ChannelId, MessageId, UserId};
use crate::domain::mention::MentionMember;
use crate::domain::message::{
    CallMessageType, ConversationType, Message, MessageContent, SystemMessage,
};
use crate::domain::disappearing::{is_timer_notice, timer_label};

/// One message row (can be sent across threads).
#[derive(Clone, Debug)]
pub struct MessageRow {
    pub mention_members: Vec<MentionMember>,
    pub mention_broadcast: bool,
    pub message_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub sender_initials: String,
    pub content: String,
    pub timestamp: String,
    pub timestamp_ns: String, // Nanosecond timestamp as string for event matching
    /// Local day chip for this message.
    pub day_label: String,
    pub is_edited: bool,
    /// A disappearing message (core's ephemeral store), for the footer glyph.
    pub disappearing: bool,
    pub is_deleted: bool,
    pub is_system: bool,
    pub is_ai: bool,
    pub has_attachments: bool,
    pub attachment_count: i32,
    /// The message's attachments as stored locally (unresolvable ones are
    /// left out). The UI shapes them into its attachment rows.
    pub attachments: Vec<Attachment>,
    pub has_reactions: bool,
    /// Typed reaction counts (source of truth; no string round-trips).
    pub reaction_counts: Vec<crate::domain::message::ReactionCount>,
    pub current_user_reactions: Vec<String>,
    pub reply_count: i32,
    pub reply_count_text: String,
    pub is_thread_reply: bool,
    // Quote / reply-to context
    pub is_reply: bool,
    /// Stringified parent `MessageId` (`Message.reply_to`); empty when this
    /// message is not a reply. Drives tap-to-jump to the quoted original.
    pub reply_to_message_id: String,
    /// Signal-Desktop parity: the quoted parent message is missing or deleted.
    /// When true the snapshot still renders but with an "Original message not
    /// found" warning, and tap-to-scroll-to-original is suppressed.
    pub reply_to_not_found: bool,
    pub reply_to_sender_name: String,
    pub reply_to_preview: String,
    /// Media category of the quoted parent ("audio"/"image"/"video"), empty
    /// when the parent has no renderable media or can't be resolved locally.
    /// Drives the rich quote rendering (e.g. inline audio player) vs plain text.
    pub reply_to_category: String,
    /// Attachment id of the quoted parent's first media attachment; keys the
    /// inline quote audio player into the shared playback machinery.
    pub reply_to_attachment_id: String,
    /// Formatted duration of the quoted audio parent (e.g. "0:37"); empty when
    /// unknown or the parent is not audio.
    pub reply_to_duration: String,
    /// Filename of the quoted audio parent, shown in the inline quote player.
    /// Empty when unknown or the parent is not audio.
    pub reply_to_filename: String,
    /// Waveform samples (32 values, 0-100) of the quoted audio parent, drawn by
    /// the inline quote player. Empty when unknown or the parent is not audio.
    pub reply_to_waveform: Vec<i32>,
    /// Raw thumbnail bytes of the quoted media (image/jpeg, image/png).
    /// Populated from `Message.reply_to_thumbnail` on receive/send.
    pub reply_to_thumbnail_bytes: Option<Vec<u8>>,
    pub delivery_status: String,
    pub failure_reason: String,
    /// Raw avatar bytes for anonymous group members.
    pub sender_avatar_bytes: Option<Arc<[u8]>>,
    // Call message fields
    pub is_pinned: bool,
    pub is_call: bool,
    /// Channel phase events keep their phase label; DM records show direction/result.
    pub is_channel_call: bool,
    pub call_message_type: String, // "started", "ended", "missed", "declined" (raw from domain)
    pub call_initiator_id: String, // User ID of who initiated the call
    pub call_duration: String,     // "1 min 30 s" or empty
}

/// The parts of a row that depend on who is looking at it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewerFields {
    pub is_own: bool,
    /// Call row kind: channel `started`/`ended` phases, `missed`, `declined`,
    /// else `outgoing`/`incoming` by initiator. Empty for non-call rows.
    pub call_type: String,
    /// Raw (markdown) body to render; a system notice we caused reads "You …".
    pub body: String,
    pub delivery_status: String,
}

impl MessageRow {
    /// Resolve the viewer-dependent fields for `current_user_id`.
    #[must_use]
    pub fn for_viewer(&self, current_user_id: &str) -> ViewerFields {
        // Preserve channel event phases; DM records display direction/result.
        let call_type = if self.is_call {
            match self.call_message_type.as_str() {
                "started" | "ended" if self.is_channel_call => self.call_message_type.clone(),
                "missed" => "missed".to_string(),
                "declined" => "declined".to_string(),
                _ => {
                    // If we initiated, it's outgoing; otherwise incoming
                    if self.call_initiator_id == current_user_id {
                        "outgoing".to_string()
                    } else {
                        "incoming".to_string()
                    }
                }
            }
        } else {
            String::new()
        };

        // For call messages, is_own is based on who initiated
        let is_own = if self.is_call {
            self.call_initiator_id == current_user_id
        } else {
            let matches = self.sender_id == current_user_id;
            if !matches && !current_user_id.is_empty() && !self.sender_id.is_empty() {
                tracing::debug!(
                    sender_id = %self.sender_id,
                    current_user_id = %current_user_id,
                    message_id = %self.message_id,
                    "[IS_OWN] sender_id != current_user_id"
                );
            }
            matches
        };

        // A notice we caused reads "You set disappearing messages…", as in
        // Signal, not our own name.
        let body = (self.is_system && is_own)
            .then(|| self.content.strip_prefix(self.sender_name.as_str()))
            .flatten()
            .map(|rest| format!("You{rest}"))
            .unwrap_or_else(|| self.content.clone());

        // For own messages with empty status, default to "sent"
        let delivery_status = if is_own && self.delivery_status.is_empty() {
            "sent".to_string()
        } else {
            self.delivery_status.clone()
        };

        ViewerFields {
            is_own,
            call_type,
            body,
            delivery_status,
        }
    }
}

/// Up to two initials, upper-cased: "ada lovelace" → "AL".
pub fn initials_from_name(name: &str) -> String {
    name.split_whitespace()
        .take(2)
        .filter_map(|word| word.chars().next())
        .collect::<String>()
        .to_uppercase()
}

/// Format a count with singular/plural form.
pub fn format_count(count: i32, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {}", singular)
    } else {
        format!("{} {}", count, plural)
    }
}

/// Flag the rows core will make disappear, in one store query per page.
pub fn mark_disappearing(client: &ChatterClient, rows: &mut [MessageRow]) {
    let ids: Vec<String> = rows.iter().map(|r| r.message_id.clone()).collect();
    let disappearing = client.disappearing_message_ids(&ids);
    for row in rows {
        row.disappearing = disappearing.contains(&row.message_id);
    }
}

/// Convert a domain Message to a row.
///
/// `resolved_attachments` are the message's attachments as stored; the reply
/// count starts at 0 (see [`apply_reply_counts`]).
pub fn message_to_row(
    msg: &Message,
    sender_name: &str,
    resolved_attachments: &[Attachment],
    sender_avatar: Option<Arc<[u8]>>,
) -> MessageRow {
    let reply_count = 0;
    let reply_count_text = format_count(reply_count, "reply", "replies");

    // Extract call information if this is a call message
    let (is_call, call_message_type, call_initiator_id, call_duration) =
        if let MessageContent::Call(call_msg) = &msg.content {
            let msg_type = match call_msg.call_type {
                CallMessageType::Started => "started",
                CallMessageType::Ended => "ended",
                CallMessageType::Missed => "missed",
                CallMessageType::Declined => "declined",
            };
            let duration = call_msg.duration_display().unwrap_or_default();
            (
                true,
                msg_type.to_string(),
                call_msg.initiator_id.to_string(),
                duration,
            )
        } else {
            (false, String::new(), String::new(), String::new())
        };

    // Extract content and system flag
    let (content, is_system) = match &msg.content {
        MessageContent::System(sys) => {
            let description = if msg.conversation_type == ConversationType::AnonDm {
                match sys {
                    SystemMessage::DisappearingTimerChanged { seconds: 0 } => {
                        "turned off disappearing messages".to_string()
                    }
                    SystemMessage::DisappearingTimerChanged { seconds } => format!(
                        "set disappearing messages to {} after read",
                        timer_label(*seconds)
                    ),
                    _ => sys.description_for(&msg.conversation_type),
                }
            } else {
                sys.description_for(&msg.conversation_type)
            };
            (format!("{} {}", sender_name, description), true)
        }
        _ => (msg.as_text().unwrap_or("").to_string(), false),
    };

    MessageRow {
        mention_members: Vec::new(),
        mention_broadcast: matches!(
            msg.conversation_type,
            ConversationType::Channel | ConversationType::AnonGroup
        ),
        message_id: msg.id.to_string(),
        // Actions and inbound updates address the stored event author; caller
        // identity is used separately for presentation and call direction.
        sender_id: msg.sender_id.map(|id| id.to_string()).unwrap_or_default(),
        sender_name: sender_name.to_string(),
        sender_initials: initials_from_name(sender_name),
        content,
        timestamp: crate::util::time::format_message_time(msg.created_at.clone()),
        timestamp_ns: msg.sent_at.to_string(),
        day_label: crate::util::time::day_label(msg.created_at),
        is_edited: msg.is_edited,
        disappearing: false, // Marked per page by `mark_disappearing`.
        is_deleted: msg.is_deleted,
        is_system,
        is_ai: msg.targets_ai_bot,
        has_attachments: msg.has_attachments(),
        attachment_count: msg.attachments.len() as i32,
        attachments: resolved_attachments.to_vec(),
        has_reactions: !msg.reaction_counts.is_empty(),
        reaction_counts: msg.reaction_counts.clone(),
        current_user_reactions: msg.current_user_reactions.clone(),
        sender_avatar_bytes: sender_avatar,
        reply_count,
        reply_count_text,
        is_thread_reply: msg.is_thread_reply(),
        is_reply: msg.reply_to.is_some(),
        reply_to_message_id: msg.reply_to.map(|id| id.to_string()).unwrap_or_default(),
        reply_to_not_found: false, // Resolved by build sites via core (needs repo access)
        reply_to_sender_name: String::new(), // Resolved by UI handler after conversion
        reply_to_preview: msg.reply_to_text.clone().unwrap_or_default(),
        reply_to_category: String::new(), // Resolved by batch via parent lookup
        reply_to_attachment_id: String::new(), // Resolved by batch via parent lookup
        reply_to_duration: String::new(), // Resolved by batch via parent lookup
        reply_to_filename: String::new(), // Resolved by batch via parent lookup
        reply_to_waveform: Vec::new(),    // Resolved by batch via parent lookup
        reply_to_thumbnail_bytes: msg.reply_to_thumbnail.clone(),
        delivery_status: crate::services::message::display_delivery_status(
            &msg.delivery_status,
            msg.failure_reason.as_deref(),
        )
        .to_owned(),
        failure_reason: msg.failure_reason.clone().unwrap_or_default(),
        is_pinned: msg.is_pinned,
        is_call,
        is_channel_call: matches!(msg.conversation_type, ConversationType::Channel),
        call_message_type,
        call_initiator_id,
        call_duration,
    }
}

/// Apply resolved parent-media to a reply's row (category, attachment
/// id/duration/filename/waveform, and a local-thumbnail fallback when the wire
/// quote thumbnail was capped/dropped). Shared by the history pages AND the
/// live receive path so both render the quote thumbnail/player without a
/// reload.
pub fn apply_reply_parent_media(row: &mut MessageRow, parent_media: ReplyParentMedia) {
    row.reply_to_category = parent_media.category;
    row.reply_to_attachment_id = parent_media.attachment_id;
    row.reply_to_duration = parent_media.duration;
    row.reply_to_filename = parent_media.filename;
    row.reply_to_waveform = parent_media.waveform;
    let wire_thumb_empty = row
        .reply_to_thumbnail_bytes
        .as_ref()
        .map_or(true, |b| b.is_empty());
    if wire_thumb_empty {
        if let Some(bytes) = parent_media.thumbnail {
            if !bytes.is_empty() {
                row.reply_to_thumbnail_bytes = Some(bytes);
            }
        }
    }
}

/// Stamp thread reply counts onto a page so root bubbles render the
/// "N replies" indicator. Counts come from
/// `ChatterClient::thread_reply_counts`; messages with no replies keep 0.
pub fn apply_reply_counts(rows: &mut [MessageRow], counts: &HashMap<MessageId, u32>) {
    if counts.is_empty() {
        return;
    }
    for row in rows.iter_mut() {
        let Ok(id) = row.message_id.parse::<MessageId>() else {
            continue;
        };
        if let Some(count) = counts.get(&id) {
            row.reply_count = *count as i32;
            row.reply_count_text = format_count(row.reply_count, "reply", "replies");
        }
    }
}

/// Resolve call identity from the recorded initiator, including end events sent
/// by another participant. Borrow ordinary messages without changing their source.
pub fn call_display_sender(msg: &Message) -> Cow<'_, Message> {
    if let MessageContent::Call(call) = &msg.content {
        if msg.sender_id != Some(call.initiator_id) {
            let mut display = msg.clone();
            display.sender_id = Some(call.initiator_id);
            return Cow::Owned(display);
        }
    }
    Cow::Borrowed(msg)
}

/// Convert a page of domain Messages to rows, resolving sender names,
/// attachments and reply-to context. Timer-change notices are dropped
/// (`disappearing::is_timer_notice`).
///
/// All name/reply business rules (conversation-scoped names, the anon
/// "not found" rule) live behind the resolvers, each keyed on the whole
/// `Message`; [`ChatterClient::message_rows`] wires the standard ones. This
/// converter never branches on conversation type or re-resolves identities.
///
/// - `sender_names`: the whole page's names, in order
/// - `resolve_attachment`: an `AttachmentId` to its stored `Attachment`
/// - `reply_parent_media`: the quoted parent's [`ReplyParentMedia`]; its local
///   thumbnail backfills `reply_to_thumbnail_bytes` when the wire quote
///   thumbnail was dropped.
#[allow(clippy::too_many_arguments)]
pub fn messages_to_rows(
    messages: &[Message],
    sender_names: impl FnOnce(&[Message]) -> Vec<String>,
    sender_avatar: impl Fn(&Message) -> Option<Vec<u8>>,
    resolve_attachment: impl Fn(&AttachmentId) -> Option<Attachment>,
    reply_author_name: impl Fn(&Message) -> String,
    reply_not_found: impl Fn(&Message) -> bool,
    reply_parent_media: impl Fn(&Message) -> ReplyParentMedia,
    mention_members: impl Fn(&Message) -> Vec<MentionMember>,
) -> Vec<MessageRow> {
    let mut rows = Vec::with_capacity(messages.len());
    let mut mention_cache: HashMap<(ConversationType, ChannelId), Vec<MentionMember>> =
        HashMap::new();
    // The whole page's names in one call (core batches the lookup).
    let display_senders: Vec<_> = messages.iter().map(call_display_sender).collect();
    let names = if display_senders.iter().any(|m| matches!(m, Cow::Owned(_))) {
        let lookup: Vec<_> = display_senders.iter().map(|m| m.as_ref().clone()).collect();
        sender_names(&lookup)
    } else {
        sender_names(messages)
    };
    // One avatar lookup per sender, shared by all of that sender's rows.
    let mut avatars: HashMap<Option<UserId>, Option<Arc<[u8]>>> = HashMap::new();

    for (i, msg) in messages.iter().enumerate() {
        if is_timer_notice(msg) {
            continue;
        }
        let identity = display_senders[i].as_ref();
        let sender = names.get(i).cloned().unwrap_or_default();
        let avatar_bytes = avatars
            .entry(identity.sender_id)
            .or_insert_with(|| sender_avatar(identity).map(Arc::<[u8]>::from))
            .clone();

        let resolved_attachments: Vec<_> = msg
            .attachments
            .iter()
            .filter_map(|att_id| resolve_attachment(att_id))
            .collect();

        let mut row = message_to_row(msg, &sender, &resolved_attachments, avatar_bytes);
        row.mention_members = mention_cache
            .entry((msg.conversation_type.clone(), msg.conversation_id))
            .or_insert_with(|| mention_members(msg))
            .clone();

        if row.is_reply {
            row.reply_to_sender_name = reply_author_name(msg);
            row.reply_to_not_found = reply_not_found(msg);
            apply_reply_parent_media(&mut row, reply_parent_media(msg));
        }

        rows.push(row);
    }

    rows
}

impl ChatterClient {
    /// [`messages_to_rows`] with this client's names, avatars, attachments,
    /// reply context and mention members.
    pub fn message_rows(&self, messages: &[Message]) -> Vec<MessageRow> {
        messages_to_rows(
            messages,
            |ms| self.message_sender_display_names(ms),
            |m| self.message_sender_avatar(m),
            self.attachment_display_resolver(),
            |m| self.reply_author_display_name(m),
            |m| self.reply_parent_missing_for(m),
            |m| self.reply_parent_media(m),
            |m| self.mention_members(&m.conversation_type, &m.conversation_id),
        )
    }

    /// Off-thread history resolver: old downloaded videos also receive previews.
    /// Four attempts per page bound decoder work independently of history size.
    pub fn attachment_display_resolver(&self) -> impl Fn(&AttachmentId) -> Option<Attachment> + '_ {
        #[cfg(target_os = "android")]
        let remaining = std::cell::Cell::new(4usize);
        let revision = self.session_revision();
        move |id| {
            if self.session_revision() != revision {
                return None;
            }
            let attachment = self.get_attachment_metadata(id)?;
            #[cfg(target_os = "android")]
            let mut attachment = attachment;
            #[cfg(target_os = "android")]
            if attachment.metadata.is_video()
                && attachment
                    .metadata
                    .thumbnail
                    .as_ref()
                    .is_none_or(|b| b.is_empty())
                && !attachment.needs_download()
                && remaining.get() > 0
            {
                remaining.set(remaining.get() - 1);
                match self.decrypt_and_extract_video_thumbnail(id) {
                    Ok(result) => attachment.metadata.thumbnail = Some(result.image_bytes),
                    Err(e) => tracing::warn!(error = %e, "History video preview extraction failed"),
                }
            }
            (self.session_revision() == revision).then_some(attachment)
        }
    }
}
