//! Message converters.

use slint::{ModelRc, VecModel};

use chatter_core::domain::attachment::Attachment;
use chatter_core::domain::message::{Message, MessageContent};

use crate::{MessageItemData, RichTextBlock};

use super::attachment::{
    AttachmentDisplayData, assign_album_positions, attachment_to_display_data,
    standalone_attachment_indices,
};
use super::markdown::{MarkdownBlock, parse_blocks, strip_markdown};
use super::utils::{format_count, reaction_counts_to_entries};
use chatter_core::domain::mention::MentionMember;

/// A message body as a row renders it: `rich_content` is the blocks the
/// bubble draws (inline runs as one `StyledText` each, fenced code blocks on
/// their own), `content` the stripped text used for empty checks, copy and
/// the plain path. A body with no markdown at all gets no blocks: the bubble
/// draws `content` in a plain `Text`.
///
/// Every path that builds or rewrites a row's body goes through here: history
/// and live rows (`to_slint_with_user`), edits (`apply_edit`), the sender's
/// own echo (`features/chat/helpers.rs` `build_own_message_item`) and local
/// Jarvis rows. A path that set `content` itself drew the raw markers.
pub fn body_fields(
    raw: &str,
    members: &[MentionMember],
    broadcast: bool,
) -> (slint::SharedString, ModelRc<RichTextBlock>) {
    let trimmed = raw.trim();
    let blocks = parse_blocks(trimmed, members, broadcast);
    let plain = match blocks.as_slice() {
        [] => true,
        [MarkdownBlock::Inline(paras)] => paras
            .iter()
            .all(|p| p.formatting.is_empty() && p.links.is_empty()),
        _ => false,
    };
    let rows: Vec<RichTextBlock> = if plain {
        Vec::new()
    } else {
        blocks
            .into_iter()
            .map(|b| match b {
                MarkdownBlock::Inline(paras) => RichTextBlock {
                    plain: paras
                        .iter()
                        .map(|p| p.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                        .into(),
                    text: slint::StyledText::from_paragraphs(paras),
                    ..Default::default()
                },
                MarkdownBlock::Code(code) => RichTextBlock {
                    plain: code.into(),
                    is_code_block: true,
                    ..Default::default()
                },
            })
            .collect()
    };
    (
        strip_markdown(trimmed).into(),
        ModelRc::new(VecModel::from(rows)),
    )
}

/// Replace a displayed row's body with an edit. The bubble draws
/// `rich_content` whenever it is non-empty, so an edit that patched only
/// `content` kept showing the old text under an "edited" badge until the
/// conversation was reopened.
pub fn apply_edit(
    row: &mut MessageItemData,
    new_text: &str,
    members: &[MentionMember],
    broadcast: bool,
) {
    (row.content, row.rich_content) = body_fields(new_text, members, broadcast);
    row.is_edited = true;
}

/// Data needed to create message UI (can be sent across threads).
#[derive(Clone, Debug)]
pub struct MessageDisplayData {
    /// Cache lifetime of prepared pixels; publication after pause/logout keeps
    /// metadata placeholders instead of restoring revoked images.
    pub prepared_generation: Option<u64>,
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
    pub attachments: Vec<AttachmentDisplayData>,
    pub has_reactions: bool,
    /// Typed reaction counts (source of truth; no string round-trips).
    pub reaction_counts: Vec<chatter_core::domain::message::ReactionCount>,
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
    /// Pre-decoded thumbnail of the quoted media, decoded from
    /// `reply_to_thumbnail_bytes` before hitting the UI thread.
    pub reply_to_thumbnail_decoded: Option<crate::bin_ui::decode_cache::Pixels>,
    pub delivery_status: String,
    pub failure_reason: String,
    /// Raw avatar bytes for anonymous group members.
    pub sender_avatar_bytes: Option<std::sync::Arc<[u8]>>,
    /// The sender's avatar at row size, shared by every row of that sender
    /// (`decode_cache::avatar`).
    pub sender_avatar_decoded: Option<crate::bin_ui::decode_cache::Pixels>,
    // Call message fields
    pub is_pinned: bool,
    pub is_call: bool,
    /// Channel phase events keep their phase label; DM records show direction/result.
    pub is_channel_call: bool,
    pub call_message_type: String, // "started", "ended", "missed", "declined" (raw from domain)
    pub call_initiator_id: String, // User ID of who initiated the call
    pub call_duration: String,     // "1 min 30 s" or empty
}

impl MessageDisplayData {
    /// Pre-decode all thumbnails (attachments, avatar, reply bubble) before invoke_from_event_loop.
    pub fn decode_attachment_thumbnails(&mut self, generation: u64) {
        super::decode_inline_thumbnails_guarded(&mut self.attachments, generation);

        if self.sender_avatar_decoded.is_none() {
            if let Some(bytes) = &self.sender_avatar_bytes {
                self.sender_avatar_decoded =
                    crate::bin_ui::decode_cache::avatar_guarded(bytes, generation);
            }
        }

        self.decode_reply_thumbnail();
    }

    fn decode_reply_thumbnail(&mut self) {
        // Quote thumbnails are small and determine the quote's first layout.
        if let Some(bytes) = &self.reply_to_thumbnail_bytes {
            if !bytes.is_empty() {
                self.reply_to_thumbnail_decoded =
                    chatter_core::media::ImageProcessor::inline_pixels_from_memory(bytes)
                        .ok()
                        .map(|rgba| {
                            let (w, h) = rgba.dimensions();
                            crate::bin_ui::decode_cache::Pixels::clone_from_slice(
                                rgba.as_raw(),
                                w,
                                h,
                            )
                        });
            }
        }
    }

    /// Convert to Slint MessageItemData with current user context.
    ///
    /// The `current_user_id` is used to determine call direction (outgoing vs incoming).
    pub fn to_slint_with_user(&self, current_user_id: &str) -> MessageItemData {
        self.to_slint_with_media(current_user_id, true)
    }

    /// First paint keeps image geometry while pixel decoding happens off-thread.
    pub fn to_slint_preview_with_user(&self, current_user_id: &str) -> MessageItemData {
        self.to_slint_with_media(current_user_id, false)
    }

    fn to_slint_with_media(&self, current_user_id: &str, media_ready: bool) -> MessageItemData {
        let pixels_current =
            self.prepared_generation == Some(crate::bin_ui::decode_cache::thumbnail_generation());
        let media_ready = media_ready && pixels_current;
        let mut slint_attachments: Vec<crate::MessageAttachmentData> = self
            .attachments
            .iter()
            .map(|a| a.to_slint_with_media(media_ready))
            .collect();
        assign_album_positions(&mut slint_attachments);
        let standalone_attachment_indices = standalone_attachment_indices(&slint_attachments);

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

        let (sender_avatar_source, has_avatar) = if let Some(pixels) = self
            .sender_avatar_decoded
            .as_ref()
            .filter(|_| pixels_current)
        {
            (slint::Image::from_rgba8(pixels.clone()), true)
        } else {
            (slint::Image::default(), false)
        };

        let (reply_to_thumbnail, reply_to_has_thumbnail, reply_to_thumb_w, reply_to_thumb_h) =
            if let Some(pixels) = self
                .reply_to_thumbnail_decoded
                .as_ref()
                .filter(|_| pixels_current)
            {
                let (w, h) = (pixels.width(), pixels.height());
                (
                    slint::Image::from_rgba8(pixels.clone()),
                    true,
                    w as i32,
                    h as i32,
                )
            } else {
                (slint::Image::default(), false, 0, 0)
            };

        // A notice we caused reads "You set disappearing messages…", as in
        // Signal, not our own name.
        let own_notice = (self.is_system && is_own)
            .then(|| self.content.strip_prefix(self.sender_name.as_str()))
            .flatten()
            .map(|rest| format!("You{rest}"));
        let (content, rich_content) = body_fields(
            own_notice.as_deref().unwrap_or(&self.content),
            &self.mention_members,
            self.mention_broadcast,
        );

        MessageItemData {
            // Multi-select marking is applied by features::chat::selection after
            // conversion; a freshly converted row is never pre-selected.
            message_id: self.message_id.clone().into(),
            sender_id: self.sender_id.clone().into(),
            sender_name: self.sender_name.clone().into(),
            sender_initials: self.sender_initials.clone().into(),
            sender_avatar_source,
            has_avatar,
            content,
            rich_content,
            timestamp: self.timestamp.clone().into(),
            timestamp_ns: self.timestamp_ns.clone().into(),
            is_own,
            is_edited: self.is_edited,
            disappearing: self.disappearing,
            is_deleted: self.is_deleted,
            is_system: self.is_system,
            is_ai: self.is_ai,
            has_attachments: !slint_attachments.is_empty() || self.has_attachments,
            attachment_count: (slint_attachments.len() as i32).max(self.attachment_count),
            attachments: ModelRc::new(VecModel::from(slint_attachments)),
            standalone_attachment_indices,
            has_reactions: self.has_reactions,
            reaction_summary: chatter_core::domain::message::ReactionCount::summary_string(
                &self.reaction_counts,
            )
            .into(),
            reactions: ModelRc::new(VecModel::from(reaction_counts_to_entries(
                &self.reaction_counts,
                &self.current_user_reactions,
            ))),
            reply_count: self.reply_count,
            reply_count_text: self.reply_count_text.clone().into(),
            is_thread_reply: self.is_thread_reply,
            is_reply: self.is_reply,
            reply_to_message_id: self.reply_to_message_id.clone().into(),
            reply_to_not_found: self.reply_to_not_found,
            reply_to_sender_name: self.reply_to_sender_name.clone().into(),
            reply_to_preview: self.reply_to_preview.clone().into(),
            reply_to_category: self.reply_to_category.clone().into(),
            reply_to_attachment_id: self.reply_to_attachment_id.clone().into(),
            reply_to_duration: self.reply_to_duration.clone().into(),
            reply_to_filename: self.reply_to_filename.clone().into(),
            reply_to_waveform: ModelRc::new(VecModel::from(
                self.reply_to_waveform
                    .iter()
                    .copied()
                    .map(super::attachment::waveform_bar)
                    .collect::<Vec<_>>(),
            )),
            reply_to_has_thumbnail,
            reply_to_thumbnail,
            reply_to_thumbnail_width: reply_to_thumb_w,
            reply_to_thumbnail_height: reply_to_thumb_h,
            show_sender: true,
            // For own messages with empty status, default to "sent"
            failure_reason: self.failure_reason.clone().into(),
            delivery_status: if is_own && self.delivery_status.is_empty() {
                "sent".into()
            } else {
                self.delivery_status.clone().into()
            },
            is_pinned: self.is_pinned,
            is_call: self.is_call,
            call_type: call_type.into(),
            call_duration: self.call_duration.clone().into(),
            date_separator_text: self.day_label.clone().into(),
            ..Default::default()
        }
    }
}

/// Convert a domain Message to thread-safe MessageDisplayData.
///
/// If `resolved_attachments` is provided, attachment details will be included.
/// If `reply_count` is provided, it will be used; otherwise defaults to 0.
/// Flag the rows core will make disappear, in one store query per page.
pub fn mark_disappearing(
    client: &chatter_core::client::ChatterClient,
    rows: &mut [MessageDisplayData],
) {
    let ids: Vec<String> = rows.iter().map(|r| r.message_id.clone()).collect();
    let disappearing = client.disappearing_message_ids(&ids);
    for row in rows {
        row.disappearing = disappearing.contains(&row.message_id);
    }
}

pub fn message_to_display_data(
    msg: &Message,
    sender_name: &str,
    resolved_attachments: &[Attachment],
    sender_avatar: Option<std::sync::Arc<[u8]>>,
) -> MessageDisplayData {
    message_to_display_data_with_replies(
        msg,
        sender_name,
        resolved_attachments,
        None,
        sender_avatar,
    )
}

/// Convert a domain Message to thread-safe MessageDisplayData with reply count.
pub fn message_to_display_data_with_replies(
    msg: &Message,
    sender_name: &str,
    resolved_attachments: &[Attachment],
    reply_count: Option<i32>,
    sender_avatar: Option<std::sync::Arc<[u8]>>,
) -> MessageDisplayData {
    let attachment_data: Vec<AttachmentDisplayData> = resolved_attachments
        .iter()
        .map(attachment_to_display_data)
        .collect();

    let reply_count = reply_count.unwrap_or(0);
    let reply_count_text = format_count(reply_count, "reply", "replies");

    // Extract call information if this is a call message
    let (is_call, call_message_type, call_initiator_id, call_duration) =
        if let MessageContent::Call(call_msg) = &msg.content {
            use chatter_core::domain::message::CallMessageType;
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
            let description = if msg.conversation_type
                == chatter_core::domain::message::ConversationType::AnonDm
            {
                match sys {
                    chatter_core::domain::message::SystemMessage::DisappearingTimerChanged {
                        seconds: 0,
                    } => "turned off disappearing messages".to_string(),
                    chatter_core::domain::message::SystemMessage::DisappearingTimerChanged {
                        seconds,
                    } => format!(
                        "set disappearing messages to {} after read",
                        chatter_core::services::disappearing::timer_label(*seconds)
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

    MessageDisplayData {
        prepared_generation: None,
        mention_members: Vec::new(),
        mention_broadcast: matches!(
            msg.conversation_type,
            chatter_core::domain::message::ConversationType::Channel
                | chatter_core::domain::message::ConversationType::AnonGroup
        ),
        message_id: msg.id.to_string(),
        // Actions and inbound updates address the stored event author; caller
        // identity is used separately for presentation and call direction.
        sender_id: msg.sender_id.map(|id| id.to_string()).unwrap_or_default(),
        sender_name: sender_name.to_string(),
        sender_initials: crate::bin_ui::converters::initials_from_name(sender_name),
        content,
        timestamp: chatter_core::util::time::format_message_time(msg.created_at.clone()),
        timestamp_ns: msg.sent_at.to_string(),
        day_label: crate::bin_ui::converters::day_label(msg.created_at),
        is_edited: msg.is_edited,
        disappearing: false, // Marked per page by `mark_disappearing`.
        is_deleted: msg.is_deleted,
        is_system,
        is_ai: msg.targets_ai_bot,
        has_attachments: msg.has_attachments(),
        attachment_count: msg.attachments.len() as i32,
        attachments: attachment_data,
        has_reactions: !msg.reaction_counts.is_empty(),
        reaction_counts: msg.reaction_counts.clone(),
        current_user_reactions: msg.current_user_reactions.clone(),
        sender_avatar_bytes: sender_avatar,
        sender_avatar_decoded: None,
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
        reply_to_thumbnail_decoded: None,
        delivery_status: chatter_core::services::message::display_delivery_status(
            &msg.delivery_status,
            msg.failure_reason.as_deref(),
        )
        .to_owned(),
        failure_reason: msg.failure_reason.clone().unwrap_or_default(),
        is_pinned: msg.is_pinned,
        is_call,
        is_channel_call: matches!(
            msg.conversation_type,
            chatter_core::domain::message::ConversationType::Channel
        ),
        call_message_type,
        call_initiator_id,
        call_duration,
    }
}

/// Convert a batch of domain Messages to display data, resolving sender names,
/// attachments, reply-to context, and pre-decoding thumbnails.
///
/// Eliminates duplicated message-to-display conversion loops across channel loading,
/// pagination, thread loading, and initial data loading.
///
/// All name/reply business rules (conversation-scoped names, the anon
/// "not found" rule) live in `chatter-core`; every resolver here is keyed on
/// the whole `Message` and callers just forward the matching `ChatterClient`
/// method. This converter stays pure presentation — it never branches on
/// conversation type or re-resolves identities.
///
/// # Arguments
/// - `messages`: slice of domain Messages to convert
/// - `sender_names`: `ChatterClient::message_sender_display_names` — the
///   whole page's names, in order
/// - `resolve_attachment`: resolves an `AttachmentId` to an `Option<Attachment>`
///   (callers should handle the `Result` → `Option` conversion themselves)
/// - `reply_author_name`: `ChatterClient::reply_author_display_name`
/// - `reply_not_found`: `ChatterClient::reply_parent_missing_for`
/// - `reply_parent_media`: `ChatterClient::reply_parent_media` — resolves the
///   quoted parent's [`ReplyParentMedia`] (category, attachment id, duration,
///   local thumbnail, filename, waveform) for rich quote rendering (inline
///   audio player, thumbnail); empty fields fall back to text. The local
///   thumbnail backfills `reply_to_thumbnail_bytes` when the wire quote
///   thumbnail was dropped.
/// Apply resolved parent-media to a reply's display data (category, attachment
/// id/duration/filename/waveform, and a local-thumbnail fallback when the wire
/// quote thumbnail was capped/dropped). Shared by the reload batch AND the live
/// receive path so both render the quote thumbnail/player without a reload.
/// Caller decodes thumbnails afterward.
pub fn apply_reply_parent_media(
    display_data: &mut MessageDisplayData,
    parent_media: chatter_core::client::ReplyParentMedia,
) {
    display_data.reply_to_category = parent_media.category;
    display_data.reply_to_attachment_id = parent_media.attachment_id;
    display_data.reply_to_duration = parent_media.duration;
    display_data.reply_to_filename = parent_media.filename;
    display_data.reply_to_waveform = parent_media.waveform;
    let wire_thumb_empty = display_data
        .reply_to_thumbnail_bytes
        .as_ref()
        .map_or(true, |b| b.is_empty());
    if wire_thumb_empty {
        if let Some(bytes) = parent_media.thumbnail {
            if !bytes.is_empty() {
                display_data.reply_to_thumbnail_bytes = Some(bytes);
            }
        }
    }
}

/// Stamp thread reply counts onto a converted batch so root bubbles render the
/// "N replies" indicator. Counts come from core
/// (`ChatterClient::thread_reply_counts`); messages with no replies keep 0.
pub fn apply_reply_counts(
    display_messages: &mut [MessageDisplayData],
    counts: &std::collections::HashMap<chatter_core::domain::ids::MessageId, u32>,
) {
    if counts.is_empty() {
        return;
    }
    for display in display_messages.iter_mut() {
        let Ok(id) = display
            .message_id
            .parse::<chatter_core::domain::ids::MessageId>()
        else {
            continue;
        };
        if let Some(count) = counts.get(&id) {
            display.reply_count = *count as i32;
            display.reply_count_text = format_count(display.reply_count, "reply", "replies");
        }
    }
}

/// Resolve call identity from the recorded initiator, including end events sent
/// by another participant. Borrow ordinary messages without changing their source.
pub fn call_display_sender(msg: &Message) -> std::borrow::Cow<'_, Message> {
    if let MessageContent::Call(call) = &msg.content {
        if msg.sender_id != Some(call.initiator_id) {
            let mut display = msg.clone();
            display.sender_id = Some(call.initiator_id);
            return std::borrow::Cow::Owned(display);
        }
    }
    std::borrow::Cow::Borrowed(msg)
}

pub async fn messages_to_display_batch(
    messages: &[Message],
    sender_names: impl FnOnce(&[Message]) -> Vec<String>,
    sender_avatar: impl Fn(&Message) -> Option<Vec<u8>>,
    resolve_attachment: impl Fn(&chatter_core::domain::AttachmentId) -> Option<Attachment>,
    reply_author_name: impl Fn(&Message) -> String,
    reply_not_found: impl Fn(&Message) -> bool,
    reply_parent_media: impl Fn(&Message) -> chatter_core::client::ReplyParentMedia,
    mention_members: impl Fn(&Message) -> Vec<MentionMember>,
) -> Vec<MessageDisplayData> {
    let generation = crate::bin_ui::decode_cache::thumbnail_generation();
    let display = messages_to_display_batch_unprepared(
        messages,
        sender_names,
        sender_avatar,
        resolve_attachment,
        reply_author_name,
        reply_not_found,
        reply_parent_media,
        mention_members,
    );
    prepare_display_media_guarded(display, true, generation).await
}

pub async fn messages_to_display_batch_deferred(
    messages: &[Message],
    sender_names: impl FnOnce(&[Message]) -> Vec<String>,
    sender_avatar: impl Fn(&Message) -> Option<Vec<u8>>,
    resolve_attachment: impl Fn(&chatter_core::domain::AttachmentId) -> Option<Attachment>,
    reply_author_name: impl Fn(&Message) -> String,
    reply_not_found: impl Fn(&Message) -> bool,
    reply_parent_media: impl Fn(&Message) -> chatter_core::client::ReplyParentMedia,
    mention_members: impl Fn(&Message) -> Vec<MentionMember>,
) -> Vec<MessageDisplayData> {
    let generation = crate::bin_ui::decode_cache::thumbnail_generation();
    let display = messages_to_display_batch_unprepared(
        messages,
        sender_names,
        sender_avatar,
        resolve_attachment,
        reply_author_name,
        reply_not_found,
        reply_parent_media,
        mention_members,
    );
    prepare_display_media_guarded(display, false, generation).await
}

pub fn messages_to_display_batch_unprepared(
    messages: &[Message],
    sender_names: impl FnOnce(&[Message]) -> Vec<String>,
    sender_avatar: impl Fn(&Message) -> Option<Vec<u8>>,
    resolve_attachment: impl Fn(&chatter_core::domain::AttachmentId) -> Option<Attachment>,
    reply_author_name: impl Fn(&Message) -> String,
    reply_not_found: impl Fn(&Message) -> bool,
    reply_parent_media: impl Fn(&Message) -> chatter_core::client::ReplyParentMedia,
    mention_members: impl Fn(&Message) -> Vec<MentionMember>,
) -> Vec<MessageDisplayData> {
    let mut display_messages = Vec::with_capacity(messages.len());
    let mut mention_cache: std::collections::HashMap<
        (
            chatter_core::domain::message::ConversationType,
            chatter_core::domain::ids::ChannelId,
        ),
        Vec<MentionMember>,
    > = std::collections::HashMap::new();
    // The whole page's names in one call (core batches the lookup).
    let display_senders: Vec<_> = messages.iter().map(call_display_sender).collect();
    let names = if display_senders
        .iter()
        .any(|m| matches!(m, std::borrow::Cow::Owned(_)))
    {
        let lookup: Vec<_> = display_senders.iter().map(|m| m.as_ref().clone()).collect();
        sender_names(&lookup)
    } else {
        sender_names(messages)
    };
    // One avatar per sender, decoded once at row size and shared by all of
    // that sender's rows (`decode_cache::avatar`). Decoding the full 512 px
    // avatar again for every row was most of a conversation open.
    let mut avatars: std::collections::HashMap<
        Option<chatter_core::domain::ids::UserId>,
        (
            Option<std::sync::Arc<[u8]>>,
            Option<crate::bin_ui::decode_cache::Pixels>,
        ),
    > = std::collections::HashMap::new();

    for (i, msg) in messages.iter().enumerate() {
        if chatter_core::services::disappearing::is_timer_notice(msg) {
            continue;
        }
        let identity = display_senders[i].as_ref();
        let sender = names.get(i).cloned().unwrap_or_default();
        let (avatar_bytes, avatar_pixels) = avatars
            .entry(identity.sender_id)
            .or_insert_with(|| {
                let bytes = sender_avatar(identity).map(std::sync::Arc::<[u8]>::from);
                let pixels = None;
                (bytes, pixels)
            })
            .clone();

        let resolved_attachments: Vec<_> = msg
            .attachments
            .iter()
            .filter_map(|att_id| resolve_attachment(att_id))
            .collect();

        let mut display_data =
            message_to_display_data(msg, &sender, &resolved_attachments, avatar_bytes);
        display_data.mention_members = mention_cache
            .entry((msg.conversation_type.clone(), msg.conversation_id))
            .or_insert_with(|| mention_members(msg))
            .clone();
        display_data.sender_avatar_decoded = avatar_pixels;

        if display_data.is_reply {
            display_data.reply_to_sender_name = reply_author_name(msg);
            display_data.reply_to_not_found = reply_not_found(msg);
            apply_reply_parent_media(&mut display_data, reply_parent_media(msg));
        }

        display_messages.push(display_data);
    }

    display_messages
}

/// Apply prepared pixels to existing rows without replaying stale transfer,
/// delivery, deletion or attachment membership snapshots.
pub fn hydrate_message_media(row: &mut MessageItemData, prepared: &MessageDisplayData) {
    use slint::Model;
    if prepared.prepared_generation != Some(crate::bin_ui::decode_cache::thumbnail_generation())
        || row.is_deleted
        || row.message_id.as_str() != prepared.message_id
    {
        return;
    }
    let attachments: Vec<_> = row
        .attachments
        .iter()
        .map(|mut attachment| {
            if let Some(pixels) = prepared
                .attachments
                .iter()
                .find(|a| a.id == attachment.id.as_str())
                .and_then(|a| a.thumbnail_pixels.as_ref())
            {
                attachment.thumbnail = slint::Image::from_rgba8(pixels.clone());
                attachment.has_thumbnail = true;
                if attachment.width <= 0 || attachment.height <= 0 {
                    attachment.width = pixels.width() as i32;
                    attachment.height = pixels.height() as i32;
                }
            }
            attachment
        })
        .collect();
    row.attachments = ModelRc::new(VecModel::from(attachments));
    if let Some(pixels) = &prepared.sender_avatar_decoded {
        row.sender_avatar_source = slint::Image::from_rgba8(pixels.clone());
        row.has_avatar = true;
    }
    if row.reply_to_message_id.as_str() == prepared.reply_to_message_id {
        if let Some(pixels) = &prepared.reply_to_thumbnail_decoded {
            row.reply_to_thumbnail = slint::Image::from_rgba8(pixels.clone());
            row.reply_to_has_thumbnail = true;
            row.reply_to_thumbnail_width = pixels.width() as i32;
            row.reply_to_thumbnail_height = pixels.height() as i32;
        }
    }
}

/// All row image work shares admission with attachment preparation. Keep
/// construction synchronous; only owned display data crosses the worker hop.
pub fn prepare_display_media(
    display: Vec<MessageDisplayData>,
    attachments: bool,
) -> impl std::future::Future<Output = Vec<MessageDisplayData>> {
    let generation = crate::bin_ui::decode_cache::thumbnail_generation();
    prepare_display_media_guarded(display, attachments, generation)
}

pub async fn prepare_display_media_guarded(
    mut display: Vec<MessageDisplayData>,
    attachments: bool,
    generation: u64,
) -> Vec<MessageDisplayData> {
    chatter_core::services::attachment::run_attachment_work(move || {
        if generation != crate::bin_ui::decode_cache::thumbnail_generation() {
            return display;
        }
        for row in &mut display {
            if generation != crate::bin_ui::decode_cache::thumbnail_generation() {
                break;
            }
            if attachments {
                row.decode_attachment_thumbnails(generation);
            } else {
                if let Some(bytes) = &row.sender_avatar_bytes {
                    row.sender_avatar_decoded =
                        crate::bin_ui::decode_cache::avatar_guarded(bytes, generation);
                }
                row.decode_reply_thumbnail();
            }
        }
        for row in &mut display {
            row.prepared_generation = Some(generation);
        }
        if generation != crate::bin_ui::decode_cache::thumbnail_generation() {
            for row in &mut display {
                for att in &mut row.attachments {
                    att.thumbnail_pixels = None;
                }
                row.sender_avatar_decoded = None;
                row.reply_to_thumbnail_decoded = None;
            }
        }
        display
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use slint::Model;

    use super::*;

    #[test]
    fn delayed_media_hydration_preserves_current_delivery_transfers_and_deleted_rows() {
        let _serial = crate::bin_ui::decode_cache::TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        use chatter_core::domain::{
            attachment::{Attachment, FileMetadata},
            ids::{AttachmentId, ChannelId},
            message::ConversationType,
        };
        let message = Message::new_text(
            ChannelId::new(),
            ConversationType::Channel,
            None,
            "body".into(),
            123,
        );
        let a = Attachment::new(
            AttachmentId::new(),
            FileMetadata::new("a.png".into(), "image/png".into(), 10),
        );
        let b = Attachment::new(
            AttachmentId::new(),
            FileMetadata::new("b.png".into(), "image/png".into(), 10),
        );
        let mut prepared = message_to_display_data(&message, "sender", &[a, b], None);
        prepared.prepared_generation = Some(crate::bin_ui::decode_cache::thumbnail_generation());
        for attachment in &mut prepared.attachments {
            attachment.thumbnail_pixels = Some(crate::bin_ui::decode_cache::Pixels::new(8, 4));
        }
        let mut row = prepared.to_slint_preview_with_user("");
        row.delivery_status = "delivered".into();
        let mut current_attachment = row.attachments.row_data(0).unwrap();
        current_attachment.progress = 0.75;
        current_attachment.is_downloading = true;
        row.attachments = ModelRc::new(VecModel::from(vec![current_attachment]));
        hydrate_message_media(&mut row, &prepared);
        assert_eq!(row.delivery_status, "delivered");
        assert_eq!(row.attachments.row_count(), 1);
        let attachment = row.attachments.row_data(0).unwrap();
        assert_eq!(attachment.progress, 0.75);
        assert!(attachment.is_downloading && attachment.has_thumbnail);
        row.is_deleted = true;
        row.attachments = ModelRc::new(VecModel::from(Vec::<crate::MessageAttachmentData>::new()));
        hydrate_message_media(&mut row, &prepared);
        assert_eq!(row.attachments.row_count(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completed_row_pixels_cannot_publish_after_pause_but_metadata_survives() {
        use chatter_core::domain::{
            attachment::{Attachment, FileMetadata},
            ids::{AttachmentId, ChannelId},
            message::ConversationType,
        };
        let _serial = crate::bin_ui::decode_cache::TEST_SERIAL
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let message = Message::new_text(
            ChannelId::new(),
            ConversationType::Channel,
            None,
            "body".into(),
            123,
        );
        let mut attachment = Attachment::new(
            AttachmentId::new(),
            FileMetadata::new("a.png".into(), "image/png".into(), 10),
        );
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 4)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        attachment.metadata.thumbnail = Some(encoded.into_inner());
        let mut data = message_to_display_data(&message, "sender", &[attachment], None);
        data.sender_avatar_bytes = data.attachments.first().map(|a| a.thumbnail_bytes.clone());
        data.reply_to_thumbnail_bytes = data.sender_avatar_bytes.as_deref().map(|b| b.to_vec());
        let data = prepare_display_media(vec![data], true).await.remove(0);
        let mut placeholder = data.to_slint_preview_with_user("");
        placeholder.has_avatar = false;
        placeholder.reply_to_has_thumbnail = false;
        let ready = data.to_slint_with_user("");
        assert!(ready.attachments.row_data(0).unwrap().has_thumbnail);
        assert!(ready.has_avatar && ready.reply_to_has_thumbnail);
        crate::bin_ui::decode_cache::on_lock();
        let revoked = data.to_slint_with_user("");
        assert_eq!(revoked.attachments.row_count(), 1);
        assert!(!revoked.attachments.row_data(0).unwrap().has_thumbnail);
        assert!(!revoked.has_avatar && !revoked.reply_to_has_thumbnail);
        hydrate_message_media(&mut placeholder, &data);
        assert!(!placeholder.attachments.row_data(0).unwrap().has_thumbnail);
        assert!(!placeholder.has_avatar && !placeholder.reply_to_has_thumbnail);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolving_old_rows_cannot_adopt_a_new_cache_lifetime() {
        use chatter_core::domain::{
            attachment::{Attachment, FileMetadata},
            ids::{AttachmentId, ChannelId},
            message::ConversationType,
        };
        let _serial = crate::bin_ui::decode_cache::TEST_SERIAL
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::bin_ui::decode_cache::wipe();
        let mut message = Message::new_text(
            ChannelId::new(),
            ConversationType::Channel,
            None,
            "body".into(),
            123,
        );
        let mut attachment = Attachment::new(
            AttachmentId::new(),
            FileMetadata::new("a.png".into(), "image/png".into(), 10),
        );
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 4)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        attachment.metadata.thumbnail = Some(encoded.into_inner());
        message.attachments.push(attachment.id);
        let rows = messages_to_display_batch(
            &[message],
            |_| vec!["sender".into()],
            |_| None,
            |_| {
                crate::bin_ui::decode_cache::on_lock();
                Some(attachment.clone())
            },
            |_| String::new(),
            |_| false,
            |_| Default::default(),
            |_| Vec::new(),
        )
        .await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].attachments.len(), 1);
        assert!(rows[0].attachments[0].thumbnail_pixels.is_none());
        assert!(crate::bin_ui::decode_cache::is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_row_preparation_yields_and_lock_preserves_rows_without_stale_pixels() {
        let _serial = crate::bin_ui::decode_cache::TEST_SERIAL
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let work = tokio::spawn(chatter_core::services::attachment::run_attachment_work(
            move || {
                entered_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
            },
        ));
        entered_rx.await.unwrap();
        let message = Message::new_text(
            chatter_core::domain::ids::ChannelId::new(),
            chatter_core::domain::message::ConversationType::Channel,
            None,
            "keep row".into(),
            123,
        );
        let mut row = message_to_display_data(&message, "sender", &[], None);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(640, 320)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        row.reply_to_thumbnail_bytes = Some(bytes.into_inner());
        let preparing = tokio::spawn(prepare_display_media(vec![row], true));
        tokio::task::yield_now().await;
        assert!(!preparing.is_finished());
        crate::bin_ui::decode_cache::on_lock();
        release_tx.send(()).unwrap();
        work.await.unwrap().unwrap();
        let rows = preparing.await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message_id, message.id.to_string());
        assert!(rows[0].reply_to_thumbnail_decoded.is_none());
        let rows = prepare_display_media(rows, true).await;
        let pixels = rows[0].reply_to_thumbnail_decoded.as_ref().unwrap();
        assert_eq!((pixels.width(), pixels.height()), (320, 160));
        crate::bin_ui::decode_cache::wipe();
    }

    #[test]
    fn channel_call_phases_preserve_direction_and_action_identity_without_changing_dm_labels() {
        use chatter_core::domain::ids::{ChannelId, UserId};
        use chatter_core::domain::message::{CallMessage, CallMessageType, ConversationType};
        let caller = UserId::new();
        let reporter = UserId::new();
        for kind in [
            ConversationType::Channel,
            ConversationType::ServerDm,
            ConversationType::AnonDm,
        ] {
            for phase in [
                CallMessageType::Started,
                CallMessageType::Ended,
                CallMessageType::Missed,
                CallMessageType::Declined,
            ] {
                let mut msg = Message::new_text(
                    ChannelId::new(),
                    kind.clone(),
                    Some(reporter),
                    String::new(),
                    123,
                );
                let mut call =
                    CallMessage::started(uuid::Uuid::new_v4().to_string(), caller, String::new());
                call.call_type = phase;
                call.duration = Some(chrono::Duration::seconds(95));
                msg.content = MessageContent::Call(call);
                msg.reply_to = Some(chatter_core::domain::ids::MessageId::new());
                let display = message_to_display_data(&msg, "Caller", &[], None);
                for viewer in [caller, reporter] {
                    let row = display.to_slint_with_user(&viewer.to_string());
                    let expected = match phase {
                        CallMessageType::Started if kind == ConversationType::Channel => "started",
                        CallMessageType::Ended if kind == ConversationType::Channel => "ended",
                        CallMessageType::Missed => "missed",
                        CallMessageType::Declined => "declined",
                        _ if viewer == caller => "outgoing",
                        _ => "incoming",
                    };
                    assert_eq!(row.call_type.as_str(), expected);
                    assert_eq!(row.is_own, viewer == caller);
                    assert_eq!(row.message_id.as_str(), msg.id.to_string());
                    assert_eq!(row.sender_id.as_str(), reporter.to_string());
                    assert_eq!(row.timestamp_ns.as_str(), "123");
                    assert_eq!(
                        row.reply_to_message_id.as_str(),
                        msg.reply_to.unwrap().to_string()
                    );
                    assert_eq!(row.call_duration.as_str(), display.call_duration);
                }
            }
        }
    }

    #[test]
    fn call_display_identity_tracks_initiator_instead_of_end_event_sender() {
        use chatter_core::domain::ids::{ChannelId, UserId};
        use chatter_core::domain::message::{CallMessage, ConversationType};
        let caller = UserId::new();
        let end_sender = UserId::new();
        let mut msg = Message::new_text(
            ChannelId::new(),
            ConversationType::Channel,
            Some(end_sender),
            String::new(),
            1,
        );
        assert!(matches!(
            call_display_sender(&msg),
            std::borrow::Cow::Borrowed(_)
        ));
        msg.content = MessageContent::Call(
            CallMessage::started("call-fixture".into(), caller, String::new()).ended(),
        );
        let identity = call_display_sender(&msg);
        assert_eq!(identity.sender_id, Some(caller));
        assert_eq!(
            msg.sender_id,
            Some(end_sender),
            "domain source stays intact"
        );
        let display = message_to_display_data(&msg, "Caller", &[], None);
        assert_eq!(display.sender_id, end_sender.to_string());
        assert_eq!(display.call_initiator_id, caller.to_string());
        assert_eq!(display.sender_initials, "C");
        assert!(!display.to_slint_with_user(&end_sender.to_string()).is_own);
    }

    #[test]
    fn apply_edit_replaces_the_blocks_the_bubble_draws() {
        let mut row = MessageItemData::default();
        (row.content, row.rich_content) = body_fields("hi again", &[], false);
        assert_eq!(
            row.rich_content.row_count(),
            0,
            "plain text draws through the plain Text"
        );

        apply_edit(&mut row, "hi **againX**", &[], false);

        let blocks: Vec<_> = row.rich_content.iter().collect();
        assert_eq!(blocks.len(), 1, "one inline run, one StyledText");
        assert!(!blocks[0].is_code_block);
        assert_ne!(blocks[0].text, slint::StyledText::default());
        assert_eq!(blocks[0].plain, "hi againX", "the run's accessible label");
        assert_eq!(row.content, "hi againX");
        assert!(row.is_edited);
    }

    #[test]
    fn unstyled_link_uses_the_rich_clickable_path_and_edit_removes_it() {
        let mut row = MessageItemData::default();
        (row.content, row.rich_content) = body_fields("[web](https://example.org)", &[], false);
        assert_eq!(row.content, "web");
        assert_eq!(row.rich_content.row_count(), 1);
        apply_edit(&mut row, "web", &[], false);
        assert_eq!(row.rich_content.row_count(), 0);
    }

    #[test]
    fn code_blocks_are_their_own_rows() {
        let (_, blocks) = body_fields("look\n```\nlet x = 1;\n```", &[], false);
        let blocks: Vec<_> = blocks.iter().collect();
        assert_eq!(blocks.len(), 2);
        assert!(!blocks[0].is_code_block);
        assert!(blocks[1].is_code_block);
        assert_eq!(blocks[1].plain, "let x = 1;");
    }
}
