//! Message converters.

use slint::{ModelRc, VecModel};

use chatter_core::domain::mention::MentionMember;
use chatter_core::ui::message_row::MessageRow;

use crate::{MessageItemData, RichTextBlock};

use super::attachment::{
    AttachmentDisplayData, assign_album_positions, attachment_to_display_data,
    standalone_attachment_indices,
};
use chatter_core::ui::markdown::{MarkdownBlock, StyledParagraph, parse_blocks, strip_markdown};
use super::utils::reaction_counts_to_entries;

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
                    text: slint::StyledText::from_paragraphs(
                        paras.into_iter().map(styled_paragraph),
                    ),
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

/// Core's paragraph as the vendored Slint `StyledTextParagraph`. The style
/// match is exhaustive so a new core style fails to compile here.
fn styled_paragraph(p: StyledParagraph) -> i_slint_core::styled_text::StyledTextParagraph {
    use chatter_core::ui::markdown::Style as Core;
    use i_slint_core::styled_text::{FormattedSpan, Style};
    i_slint_core::styled_text::StyledTextParagraph {
        text: p.text,
        formatting: p
            .formatting
            .into_iter()
            .map(|f| FormattedSpan {
                range: f.range,
                style: match f.style {
                    Core::Strong => Style::Strong,
                    Core::Emphasis => Style::Emphasis,
                    Core::Strikethrough => Style::Strikethrough,
                    Core::Code => Style::Code,
                    Core::Link => Style::Link,
                },
            })
            .collect(),
        links: p.links,
    }
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

/// A core [`MessageRow`] with its images prepared for the list (can be sent
/// across threads). Everything but pixels and the attachment rows comes from
/// core unchanged.
#[derive(Clone, Debug)]
pub struct MessageDisplayData {
    pub row: MessageRow,
    pub attachments: Vec<AttachmentDisplayData>,
    /// Cache lifetime of prepared pixels; publication after pause/logout keeps
    /// metadata placeholders instead of restoring revoked images.
    pub prepared_generation: Option<u64>,
    /// Pre-decoded thumbnail of the quoted media, decoded from
    /// `row.reply_to_thumbnail_bytes` before hitting the UI thread.
    pub reply_to_thumbnail_decoded: Option<crate::bin_ui::decode_cache::Pixels>,
    /// The sender's avatar at row size, shared by every row of that sender
    /// (`decode_cache::avatar`).
    pub sender_avatar_decoded: Option<crate::bin_ui::decode_cache::Pixels>,
}

impl From<MessageRow> for MessageDisplayData {
    fn from(mut row: MessageRow) -> Self {
        let attachments = std::mem::take(&mut row.attachments)
            .iter()
            .map(attachment_to_display_data)
            .collect();
        Self {
            row,
            attachments,
            prepared_generation: None,
            reply_to_thumbnail_decoded: None,
            sender_avatar_decoded: None,
        }
    }
}

impl MessageDisplayData {
    /// Pre-decode all thumbnails (attachments, avatar, reply bubble) before invoke_from_event_loop.
    pub fn decode_attachment_thumbnails(&mut self, generation: u64) {
        super::decode_inline_thumbnails_guarded(&mut self.attachments, generation);

        if self.sender_avatar_decoded.is_none() {
            if let Some(bytes) = &self.row.sender_avatar_bytes {
                self.sender_avatar_decoded =
                    crate::bin_ui::decode_cache::avatar_guarded(bytes, generation);
            }
        }

        self.decode_reply_thumbnail();
    }

    fn decode_reply_thumbnail(&mut self) {
        // Quote thumbnails are small and determine the quote's first layout.
        if let Some(bytes) = &self.row.reply_to_thumbnail_bytes {
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

        let viewer = self.row.for_viewer(current_user_id);
        let is_own = viewer.is_own;

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

        let (content, rich_content) = body_fields(
            &viewer.body,
            &self.row.mention_members,
            self.row.mention_broadcast,
        );

        MessageItemData {
            // Multi-select marking is applied by features::chat::selection after
            // conversion; a freshly converted row is never pre-selected.
            message_id: self.row.message_id.clone().into(),
            sender_id: self.row.sender_id.clone().into(),
            sender_name: self.row.sender_name.clone().into(),
            sender_initials: self.row.sender_initials.clone().into(),
            sender_avatar_source,
            has_avatar,
            content,
            rich_content,
            timestamp: self.row.timestamp.clone().into(),
            timestamp_ns: self.row.timestamp_ns.clone().into(),
            is_own,
            is_edited: self.row.is_edited,
            disappearing: self.row.disappearing,
            is_deleted: self.row.is_deleted,
            is_system: self.row.is_system,
            is_ai: self.row.is_ai,
            has_attachments: !slint_attachments.is_empty() || self.row.has_attachments,
            attachment_count: (slint_attachments.len() as i32).max(self.row.attachment_count),
            attachments: ModelRc::new(VecModel::from(slint_attachments)),
            standalone_attachment_indices,
            has_reactions: self.row.has_reactions,
            reaction_summary: chatter_core::domain::message::ReactionCount::summary_string(
                &self.row.reaction_counts,
            )
            .into(),
            reactions: ModelRc::new(VecModel::from(reaction_counts_to_entries(
                &self.row.reaction_counts,
                &self.row.current_user_reactions,
            ))),
            reply_count: self.row.reply_count,
            reply_count_text: self.row.reply_count_text.clone().into(),
            is_thread_reply: self.row.is_thread_reply,
            is_reply: self.row.is_reply,
            reply_to_message_id: self.row.reply_to_message_id.clone().into(),
            reply_to_not_found: self.row.reply_to_not_found,
            reply_to_sender_name: self.row.reply_to_sender_name.clone().into(),
            reply_to_preview: self.row.reply_to_preview.clone().into(),
            reply_to_category: self.row.reply_to_category.clone().into(),
            reply_to_attachment_id: self.row.reply_to_attachment_id.clone().into(),
            reply_to_duration: self.row.reply_to_duration.clone().into(),
            reply_to_filename: self.row.reply_to_filename.clone().into(),
            reply_to_waveform: ModelRc::new(VecModel::from(
                self.row.reply_to_waveform
                    .iter()
                    .copied()
                    .map(chatter_core::ui::format::waveform_bar)
                    .collect::<Vec<_>>(),
            )),
            reply_to_has_thumbnail,
            reply_to_thumbnail,
            reply_to_thumbnail_width: reply_to_thumb_w,
            reply_to_thumbnail_height: reply_to_thumb_h,
            show_sender: true,
            failure_reason: self.row.failure_reason.clone().into(),
            delivery_status: viewer.delivery_status.into(),
            is_pinned: self.row.is_pinned,
            is_call: self.row.is_call,
            call_type: viewer.call_type.into(),
            call_duration: self.row.call_duration.clone().into(),
            call_is_video: self.row.call_is_video,
            date_separator_text: self.row.day_label.clone().into(),
            ..Default::default()
        }
    }
}

/// Apply prepared pixels to existing rows without replaying stale transfer,
/// delivery, deletion or attachment membership snapshots.
pub fn hydrate_message_media(row: &mut MessageItemData, prepared: &MessageDisplayData) {
    use slint::Model;
    if prepared.prepared_generation != Some(crate::bin_ui::decode_cache::thumbnail_generation())
        || row.is_deleted
        || row.message_id.as_str() != prepared.row.message_id
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
    if row.reply_to_message_id.as_str() == prepared.row.reply_to_message_id {
        if let Some(pixels) = &prepared.reply_to_thumbnail_decoded {
            row.reply_to_thumbnail = slint::Image::from_rgba8(pixels.clone());
            row.reply_to_has_thumbnail = true;
            row.reply_to_thumbnail_width = pixels.width() as i32;
            row.reply_to_thumbnail_height = pixels.height() as i32;
        }
    }
}

/// Core rows to prepared display rows. `generation` is the cache lifetime the
/// caller captured BEFORE core built the rows: a lock while they were built
/// must not let them adopt the new lifetime.
pub async fn prepare_rows(
    rows: Vec<MessageRow>,
    attachments: bool,
    generation: u64,
) -> Vec<MessageDisplayData> {
    let display = rows.into_iter().map(MessageDisplayData::from).collect();
    prepare_display_media_guarded(display, attachments, generation).await
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
    chatter_core::media::run_attachment_work(move || {
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
                if let Some(bytes) = &row.row.sender_avatar_bytes {
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
    use chatter_core::domain::message::{Message, MessageContent};
    use chatter_core::ui::message_row::{call_display_sender, message_to_row, messages_to_rows};

    fn display(
        msg: &Message,
        sender: &str,
        attachments: &[chatter_core::domain::attachment::Attachment],
    ) -> MessageDisplayData {
        message_to_row(msg, sender, attachments, None).into()
    }

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
        let mut prepared = display(&message, "sender", &[a, b]);
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
        let mut data = display(&message, "sender", &[attachment]);
        data.row.sender_avatar_bytes = data.attachments.first().map(|a| a.thumbnail_bytes.clone());
        data.row.reply_to_thumbnail_bytes =
            data.row.sender_avatar_bytes.as_deref().map(|b| b.to_vec());
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
        let generation = crate::bin_ui::decode_cache::thumbnail_generation();
        let rows = messages_to_rows(
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
        );
        let rows = prepare_rows(rows, true, generation).await;
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
        let work = tokio::spawn(chatter_core::media::run_attachment_work(
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
        let mut row = display(&message, "sender", &[]);
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(640, 320)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        row.row.reply_to_thumbnail_bytes = Some(bytes.into_inner());
        let preparing = tokio::spawn(prepare_display_media(vec![row], true));
        tokio::task::yield_now().await;
        assert!(!preparing.is_finished());
        crate::bin_ui::decode_cache::on_lock();
        release_tx.send(()).unwrap();
        work.await.unwrap().unwrap();
        let rows = preparing.await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].row.message_id, message.id.to_string());
        assert!(rows[0].reply_to_thumbnail_decoded.is_none());
        let rows = prepare_display_media(rows, true).await;
        let pixels = rows[0].reply_to_thumbnail_decoded.as_ref().unwrap();
        assert_eq!((pixels.width(), pixels.height()), (640, 320));
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
                let display = display(&msg, "Caller", &[]);
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
                    assert_eq!(row.call_duration.as_str(), display.row.call_duration);
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
        let display = display(&msg, "Caller", &[]);
        assert_eq!(display.row.sender_id, end_sender.to_string());
        assert_eq!(display.row.call_initiator_id, caller.to_string());
        assert_eq!(display.row.sender_initials, "C");
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
