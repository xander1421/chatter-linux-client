//! Message converters.

use slint::{ModelRc, VecModel};

use chatter_core::domain::attachment::Attachment;
use chatter_core::domain::message::{Message, MessageContent};

use crate::{MessageItemData, RichTextBlock};

use super::attachment::{attachment_to_display_data, AttachmentDisplayData};
use super::markdown::{parse_blocks, strip_markdown, MarkdownBlock};
use super::utils::{format_count, reaction_counts_to_entries};

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
pub fn body_fields(raw: &str) -> (slint::SharedString, ModelRc<RichTextBlock>) {
    let trimmed = raw.trim();
    let blocks = parse_blocks(trimmed);
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
pub fn apply_edit(row: &mut MessageItemData, new_text: &str) {
    (row.content, row.rich_content) = body_fields(new_text);
    row.is_edited = true;
}

/// Data needed to create message UI (can be sent across threads).
#[derive(Clone, Debug)]
pub struct MessageDisplayData {
    pub message_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub sender_initials: String,
    pub content: String,
    pub timestamp: String,
    pub timestamp_ns: String, // Nanosecond timestamp as string for event matching
    /// Core's day chip for this message (`ui::chat_data::day_label`).
    pub day_label: String,
    pub is_edited: bool,
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
    pub sender_avatar_bytes: Option<Vec<u8>>,
    /// The sender's avatar at row size, shared by every row of that sender
    /// (`decode_cache::avatar`).
    pub sender_avatar_decoded: Option<crate::bin_ui::decode_cache::Pixels>,
    // Call message fields
    pub is_pinned: bool,
    pub is_call: bool,
    pub call_message_type: String, // "started", "ended", "missed", "declined" (raw from domain)
    pub call_initiator_id: String, // User ID of who initiated the call
    pub call_duration: String,     // "1 min 30 s" or empty
}

impl MessageDisplayData {
    /// Pre-decode all thumbnails (attachments, avatar, reply bubble) before invoke_from_event_loop.
    pub fn decode_attachment_thumbnails(&mut self) {
        for att in &mut self.attachments {
            att.decode_thumbnail();
        }

        if self.sender_avatar_decoded.is_none() {
            if let Some(bytes) = &self.sender_avatar_bytes {
                self.sender_avatar_decoded = crate::bin_ui::decode_cache::avatar(bytes);
            }
        }

        // Decode reply-bubble thumbnail
        if let Some(bytes) = &self.reply_to_thumbnail_bytes {
            if !bytes.is_empty() {
                self.reply_to_thumbnail_decoded = image::load_from_memory(bytes).ok().map(|img| {
                    let rgba = img.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    crate::bin_ui::decode_cache::Pixels::clone_from_slice(rgba.as_raw(), w, h)
                });
            }
        }
    }

    /// Convert to Slint MessageItemData with current user context.
    ///
    /// The `current_user_id` is used to determine call direction (outgoing vs incoming).
    pub fn to_slint_with_user(&self, current_user_id: &str) -> MessageItemData {
        let slint_attachments: Vec<crate::MessageAttachmentData> =
            self.attachments.iter().map(|a| a.to_slint()).collect();

        // Determine call type based on who initiated
        let call_type = if self.is_call {
            match self.call_message_type.as_str() {
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

        let (sender_avatar_source, has_avatar) = if let Some(pixels) = &self.sender_avatar_decoded {
            (slint::Image::from_rgba8(pixels.clone()), true)
        } else if let Some(bytes) = &self.sender_avatar_bytes {
            match crate::bin_ui::decode_cache::avatar(bytes) {
                Some(pixels) => (slint::Image::from_rgba8(pixels), true),
                None => (slint::Image::default(), false),
            }
        } else {
            (slint::Image::default(), false)
        };

        let (reply_to_thumbnail, reply_to_has_thumbnail, reply_to_thumb_w, reply_to_thumb_h) =
            if let Some(pixels) = &self.reply_to_thumbnail_decoded {
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
        let (content, rich_content) = body_fields(own_notice.as_deref().unwrap_or(&self.content));

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
            is_deleted: self.is_deleted,
            is_system: self.is_system,
            is_ai: self.is_ai,
            has_attachments: !slint_attachments.is_empty() || self.has_attachments,
            attachment_count: (slint_attachments.len() as i32).max(self.attachment_count),
            attachments: ModelRc::new(VecModel::from(slint_attachments)),
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
pub fn message_to_display_data(
    msg: &Message,
    sender_name: &str,
    resolved_attachments: &[Attachment],
    sender_avatar: Option<Vec<u8>>,
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
    sender_avatar: Option<Vec<u8>>,
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
        MessageContent::System(sys) => (format!("{} {}", sender_name, sys.description()), true),
        _ => (msg.as_text().unwrap_or("").to_string(), false),
    };

    MessageDisplayData {
        message_id: msg.id.to_string(),
        sender_id: if is_call {
            call_initiator_id.clone()
        } else {
            msg.sender_id.map(|id| id.to_string()).unwrap_or_default()
        },
        sender_name: sender_name.to_string(),
        sender_initials: chatter_core::ui::initials_from_name(sender_name),
        content,
        timestamp: {
            // Convert UTC to local time for display (consistent with sidebar)
            use chrono::Local;
            msg.created_at
                .with_timezone(&Local)
                .format("%H:%M")
                .to_string()
        },
        timestamp_ns: msg.sent_at.to_string(),
        day_label: chatter_core::ui::chat_data::day_label(msg.created_at),
        is_edited: msg.is_edited,
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
        delivery_status: msg.delivery_status.clone(),
        failure_reason: msg.failure_reason.clone().unwrap_or_default(),
        is_pinned: msg.is_pinned,
        is_call,
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

pub fn messages_to_display_batch(
    messages: &[Message],
    sender_names: impl FnOnce(&[Message]) -> Vec<String>,
    sender_avatar: impl Fn(&Message) -> Option<Vec<u8>>,
    resolve_attachment: impl Fn(&chatter_core::domain::AttachmentId) -> Option<Attachment>,
    reply_author_name: impl Fn(&Message) -> String,
    reply_not_found: impl Fn(&Message) -> bool,
    reply_parent_media: impl Fn(&Message) -> chatter_core::client::ReplyParentMedia,
) -> Vec<MessageDisplayData> {
    let mut display_messages = Vec::with_capacity(messages.len());
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
        (Option<Vec<u8>>, Option<crate::bin_ui::decode_cache::Pixels>),
    > = std::collections::HashMap::new();

    for (i, msg) in messages.iter().enumerate() {
        let identity = display_senders[i].as_ref();
        let sender = names.get(i).cloned().unwrap_or_default();
        let (avatar_bytes, avatar_pixels) = avatars
            .entry(identity.sender_id)
            .or_insert_with(|| {
                let bytes = sender_avatar(identity);
                let pixels = bytes
                    .as_deref()
                    .and_then(crate::bin_ui::decode_cache::avatar);
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
        display_data.sender_avatar_decoded = avatar_pixels;

        if display_data.is_reply {
            display_data.reply_to_sender_name = reply_author_name(msg);
            display_data.reply_to_not_found = reply_not_found(msg);
            apply_reply_parent_media(&mut display_data, reply_parent_media(msg));
        }

        display_messages.push(display_data);
    }

    // Pre-decode attachment + reply thumbnails before UI thread
    for msg in &mut display_messages {
        msg.decode_attachment_thumbnails();
    }

    display_messages
}

#[cfg(test)]
mod tests {
    use slint::Model;

    use super::*;

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
        msg.content = MessageContent::Call(CallMessage::started(
            "call-fixture".into(),
            caller,
            String::new(),
        ));
        let identity = call_display_sender(&msg);
        assert_eq!(identity.sender_id, Some(caller));
        assert_eq!(
            msg.sender_id,
            Some(end_sender),
            "domain source stays intact"
        );
        let display = message_to_display_data(&msg, "Caller", &[], None);
        assert_eq!(display.sender_id, caller.to_string());
        assert_eq!(display.sender_initials, "C");
        assert!(!display.to_slint_with_user(&end_sender.to_string()).is_own);
    }

    #[test]
    fn apply_edit_replaces_the_blocks_the_bubble_draws() {
        let mut row = MessageItemData::default();
        (row.content, row.rich_content) = body_fields("hi again");
        assert_eq!(
            row.rich_content.row_count(),
            0,
            "plain text draws through the plain Text"
        );

        apply_edit(&mut row, "hi **againX**");

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
        (row.content, row.rich_content) = body_fields("[web](https://example.org)");
        assert_eq!(row.content, "web");
        assert_eq!(row.rich_content.row_count(), 1);
        apply_edit(&mut row, "web");
        assert_eq!(row.rich_content.row_count(), 0);
    }

    #[test]
    fn code_blocks_are_their_own_rows() {
        let (_, blocks) = body_fields("look\n```\nlet x = 1;\n```");
        let blocks: Vec<_> = blocks.iter().collect();
        assert_eq!(blocks.len(), 2);
        assert!(!blocks[0].is_code_block);
        assert!(blocks[1].is_code_block);
        assert_eq!(blocks[1].plain, "let x = 1;");
    }
}
