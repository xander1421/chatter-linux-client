//! Helper functions for chat UI operations.

use slint::Model;
use chatter_core::domain::UserId;
use crate::MainWindow;

/// Parsed reply context from UI strings.
pub struct ReplyContext {
    pub sender_id: UserId,
    pub timestamp: u64,
}

/// Parse reply context from UI string values.
///
/// Returns `None` if either string fails to parse. Logs errors on failure.
pub fn parse_reply_context(sender_str: &str, timestamp_str: &str) -> Option<ReplyContext> {
    let sender_id: UserId = match sender_str.parse() {
        Ok(id) => id,
        Err(_) => {
            tracing::error!("Invalid reply_to_sender: {}", sender_str);
            return None;
        }
    };
    let timestamp: u64 = match timestamp_str.parse() {
        Ok(ts) => ts,
        Err(_) => {
            tracing::error!("Invalid reply_to_timestamp: {}", timestamp_str);
            return None;
        }
    };
    Some(ReplyContext { sender_id, timestamp })
}

/// Build a `MessageItemData` for a sent (own) message.
///
/// Most fields are constant for sent messages — this avoids repeating
/// 28 fields in every send branch.
pub fn build_own_message_item(
    message_id: &str,
    sender_id: &str,
    sender_name: &str,
    text: &str,
    timestamp: &str,
    timestamp_ns: u64,
    slint_attachments: Vec<crate::MessageAttachmentData>,
    is_reply: bool,
    reply_to_sender_name: &str,
    reply_to_preview: &str,
    reply_to_thumbnail: slint::Image,
    reply_to_has_thumbnail: bool,
    reply_to_thumbnail_width: i32,
    reply_to_thumbnail_height: i32,
    reply_media: &chatter_core::client::ReplyParentMedia,
    delivery_status: &str,
) -> crate::MessageItemData {
    use slint::{ModelRc, VecModel};

    crate::MessageItemData {
        // See converters::message — selection marking is applied afterwards.
        selected: false,
        message_id: message_id.into(),
        sender_id: sender_id.into(),
        sender_name: sender_name.into(),
        sender_initials: crate::bin_ui::converters::get_initials(sender_name).into(),
        sender_avatar_source: slint::Image::default(),
        has_avatar: false,
        content: text.into(),
        rich_content: slint::ModelRc::default(),
        timestamp: timestamp.into(),
        timestamp_ns: timestamp_ns.to_string().into(),
        is_own: true,
        is_edited: false,
        is_deleted: false,
        is_system: false,
        is_ai: false,
        has_attachments: !slint_attachments.is_empty(),
        attachment_count: slint_attachments.len() as i32,
        attachments: ModelRc::new(VecModel::from(slint_attachments)),
        has_reactions: false,
        reaction_summary: "".into(),
        reactions: ModelRc::default(),
        reply_count: 0,
        reply_count_text: "".into(),
        is_thread_reply: false,
        is_reply,
        // Optimistic echo identifies its reply by (sender, timestamp), not a
        // parent MessageId; leave empty (tap-to-jump suppressed) until the
        // canonical DB-converted message — which carries reply_to — replaces it.
        reply_to_message_id: "".into(),
        // Own outgoing reply: the parent existed at send time.
        reply_to_not_found: false,
        reply_to_sender_name: reply_to_sender_name.into(),
        reply_to_preview: reply_to_preview.into(),
        // Parent-media snapshot (resolved at send time) so the echo shows the
        // rich quote — inline audio player, media label — immediately, not text
        // until the canonical DB message replaces it.
        reply_to_category: reply_media.category.clone().into(),
        reply_to_attachment_id: reply_media.attachment_id.clone().into(),
        reply_to_duration: reply_media.duration.clone().into(),
        reply_to_filename: reply_media.filename.clone().into(),
        reply_to_waveform: ModelRc::new(VecModel::from(
            reply_media.waveform.iter().copied()
                .map(crate::bin_ui::converters::waveform_bar).collect::<Vec<_>>(),
        )),
        reply_to_has_thumbnail,
        reply_to_thumbnail,
        reply_to_thumbnail_width,
        reply_to_thumbnail_height,
        show_sender: true,
        delivery_status: delivery_status.into(),
        failure_reason: Default::default(),
        is_pinned: false,
        is_call: false,
        call_type: "".into(),
        call_duration: "".into(),
        // Just sent, so today's chip.
        date_separator_text: chatter_core::ui::chat_data::day_label(chrono::Utc::now()).into(),
        is_continuation: false,
        show_date_separator: false,
    }
}

/// Append a message to the UI message list and scroll to bottom.
///
/// This is the common pattern after a message is sent: push onto the model
/// in place (the list only ever holds the rows on screen, and a fresh model
/// would make it re-measure and re-seek), clear composer, and auto-scroll.
pub fn append_message_to_ui(window: &crate::MainWindow, msg_item: crate::MessageItemData) {
    window.set_is_at_bottom(true);
    edit_messages(window, |model| model.push(msg_item));
    window.set_composer_text("".into());
}

/// Edit the open message list in place. Every path that changes one row
/// funnels through here so none of them swaps the model out from under the
/// list: a swap costs a full re-measure and can drift the scroll position by
/// a row. The model is always the `VecModel` the open installed; the fallback
/// only guards a test that installed something else.
pub(crate) fn edit_messages(
    window: &crate::MainWindow,
    f: impl FnOnce(&slint::VecModel<crate::MessageItemData>),
) {
    let model = window.get_messages();
    match model.as_any().downcast_ref::<slint::VecModel<crate::MessageItemData>>() {
        Some(vec) => f(vec),
        None => tracing::warn!("message list model is not a VecModel; edit dropped"),
    }
}

// ============================================================================
// Shared UI update helpers
// ============================================================================

/// Update a message's reaction summary in the UI model.
///
/// Finds the message by sender_id + timestamp_ns and rewrites that one row.
/// Used by all 4 reaction paths (anon DM, anon group, regular DM, channel).
pub(crate) fn update_reaction_in_ui(
    window: &MainWindow,
    sender_id: &str,
    timestamp_ns: &str,
    emoji: &str,
    remove: bool,
) {
    use chatter_core::domain::message::ReactionCount;
    use crate::bin_ui::converters::{reaction_counts_to_entries, reactions_model_to_counts};

    edit_messages(window, |model| {
        for i in 0..model.row_count() {
            let Some(mut m) = model.row_data(i) else { continue };
            if m.sender_id.as_str() != sender_id || m.timestamp_ns.as_str() != timestamp_ns {
                continue;
            }
            let mut counts = reactions_model_to_counts(&m.reactions);
            ReactionCount::apply(&mut counts, emoji, remove);
            m.has_reactions = !counts.is_empty();
            m.reaction_summary = ReactionCount::summary_string(&counts).into();
            m.reactions = slint::ModelRc::new(slint::VecModel::from(reaction_counts_to_entries(&counts)));
            model.set_row_data(i, m);
        }
    });
}

/// Remove a message from the UI model by sender_id + timestamp_ns.
///
/// Unregisters any inline GIF animations for the deleted message.
/// Used by all delete paths (anon DM, anon group, regular DM, channel)
/// for both delete-for-me and delete-for-everyone.
pub(crate) fn remove_message_from_ui(
    window: &MainWindow,
    sender_id: &str,
    timestamp_ns: &str,
    unregister_gifs: impl Fn(&crate::MessageItemData),
) {
    remove_messages_where(window, |m| {
        let hit = m.sender_id.as_str() == sender_id && m.timestamp_ns.as_str() == timestamp_ns;
        if hit {
            unregister_gifs(m);
        }
        hit
    });
}

/// Remove every row `gone` matches, in place, back to front so the indices
/// stay valid.
pub(crate) fn remove_messages_where(
    window: &MainWindow,
    gone: impl Fn(&crate::MessageItemData) -> bool,
) {
    edit_messages(window, |model| {
        for i in (0..model.row_count()).rev() {
            if model.row_data(i).is_some_and(|m| gone(&m)) {
                model.remove(i);
            }
        }
    });
}
