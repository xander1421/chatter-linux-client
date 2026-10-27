//! Helper functions for chat UI operations.

use crate::MainWindow;
use chatter_core::domain::UserId;
use slint::{ComponentHandle, Model};

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
    Some(ReplyContext {
        sender_id,
        timestamp,
    })
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
    mention_members: &[chatter_core::domain::mention::MentionMember],
    mention_broadcast: bool,
    timestamp: &str,
    timestamp_ns: u64,
    mut slint_attachments: Vec<crate::MessageAttachmentData>,
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

    crate::bin_ui::converters::assign_album_positions(&mut slint_attachments);
    let standalone_attachment_indices =
        crate::bin_ui::converters::standalone_attachment_indices(&slint_attachments);

    let (content, rich_content) = crate::bin_ui::converters::body_fields(text, mention_members, mention_broadcast);
    crate::MessageItemData {
        // See converters::message — selection marking is applied afterwards.
        message_id: message_id.into(),
        sender_id: sender_id.into(),
        sender_name: sender_name.into(),
        sender_initials: crate::bin_ui::converters::initials_from_name(sender_name).into(),
        content,
        rich_content,
        timestamp: timestamp.into(),
        timestamp_ns: timestamp_ns.to_string().into(),
        is_own: true,
        has_attachments: !slint_attachments.is_empty(),
        attachment_count: slint_attachments.len() as i32,
        attachments: ModelRc::new(VecModel::from(slint_attachments)),
        standalone_attachment_indices,
        is_reply,
        // Optimistic echo identifies its reply by (sender, timestamp), not a
        // parent MessageId; the id stays empty (tap-to-jump suppressed) until the
        // canonical DB-converted message — which carries reply_to — replaces it.
        // Own outgoing reply: the parent existed at send time.
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
            reply_media
                .waveform
                .iter()
                .copied()
                .map(crate::bin_ui::converters::waveform_bar)
                .collect::<Vec<_>>(),
        )),
        reply_to_has_thumbnail,
        reply_to_thumbnail,
        reply_to_thumbnail_width,
        reply_to_thumbnail_height,
        show_sender: true,
        delivery_status: delivery_status.into(),
        // Just sent, so today's chip.
        date_separator_text: crate::bin_ui::converters::day_label(chrono::Utc::now()).into(),
        ..Default::default()
    }
}

/// Append a message to the UI message list and scroll to bottom.
///
/// This is the common pattern after a message is sent: push onto the model
/// in place (the list only ever holds the rows on screen, and a fresh model
/// would make it re-measure and re-seek), clear composer, and auto-scroll.
pub fn append_message_to_ui(window: &crate::MainWindow, msg_item: crate::MessageItemData) {
    window.global::<crate::ChatPane>().set_is_at_bottom(true);
    edit_messages(window, |model| model.push(msg_item));
    window
        .global::<crate::ChatPane>()
        .set_composer_text("".into());
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
    let model = window.global::<crate::ChatPane>().get_messages();
    match model
        .as_any()
        .downcast_ref::<slint::VecModel<crate::MessageItemData>>()
    {
        Some(vec) => {
            f(vec);
            if window.global::<crate::ChatPane>().get_conversation_kind()
                == crate::ConversationKind::Channel
            {
                recompute_channel_continuations(vec);
            }
        }
        None => tracing::warn!("message list model is not a VecModel; edit dropped"),
    }
}

/// Recompute grouping from visible neighbours after insertions, prepends and
/// removals. Only changed rows notify Slint, preserving the list's model and
/// scroll anchor.
pub(crate) fn recompute_channel_continuations(model: &slint::VecModel<crate::MessageItemData>) {
    use chatter_core::ui::chat_data::{ChannelGroupingRow, channel_continuation_flags};

    let rows: Vec<_> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect();
    let grouping_rows: Vec<_> = rows
        .iter()
        .map(|row| ChannelGroupingRow {
            sender_id: row.sender_id.as_str(),
            timestamp_ns: row.timestamp_ns.as_str().parse::<u64>().unwrap_or(0),
            day_label: row.date_separator_text.as_str(),
            is_system: row.is_system,
            is_call: row.is_call,
            is_deleted: row.is_deleted,
        })
        .collect();
    let flags = channel_continuation_flags(&grouping_rows);
    for (index, (mut row, is_continuation)) in rows.into_iter().zip(flags).enumerate() {
        if row.is_continuation != is_continuation {
            row.is_continuation = is_continuation;
            model.set_row_data(index, row);
        }
    }
}

// ============================================================================
// Shared UI update helpers
// ============================================================================

/// Refresh one row from storage at UI application time. Queued completions
/// carry only identity, so neither duplicates nor delayed tasks replay a delta
/// or install an old snapshot over newer persisted state.
pub fn refresh_reaction_in_ui(
    window: &MainWindow,
    client: &std::sync::Arc<chatter_core::client::ChatterClient>,
    conversation: chatter_core::state::ActiveConversation,
    author: &UserId,
    sent_at: u64,
) {
    refresh_reaction_snapshot(window, client, conversation, None, author, sent_at);
}

/// Apply only the actual persisted conversation named by the incoming event.
#[doc(hidden)]
pub fn refresh_scoped_reaction_in_ui(
    window: &MainWindow,
    client: &std::sync::Arc<chatter_core::client::ChatterClient>,
    conversation: chatter_core::state::ActiveConversation,
    expected: &chatter_core::domain::reaction::ReactionScope,
    author: &UserId,
    sent_at: u64,
) {
    refresh_reaction_snapshot(
        window,
        client,
        conversation,
        Some(expected),
        author,
        sent_at,
    );
}

fn refresh_reaction_snapshot(
    window: &MainWindow,
    client: &std::sync::Arc<chatter_core::client::ChatterClient>,
    conversation: chatter_core::state::ActiveConversation,
    expected: Option<&chatter_core::domain::reaction::ReactionScope>,
    author: &UserId,
    sent_at: u64,
) {
    // No chat open (the reaction is stored; nothing on screen to refresh).
    if conversation == chatter_core::state::ActiveConversation::None
        || client.active_conversation() != conversation
    {
        return;
    }
    let handle = client.conversation(conversation);
    let snapshot = match match expected {
        Some(scope) => handle.reaction_snapshot_for_scope(scope, author, sent_at),
        None => handle.reaction_snapshot(author, sent_at),
    } {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, "Reaction snapshot unavailable");
            return;
        }
    };
    let apply = |model: &dyn Model<Data = crate::MessageItemData>| {
        for i in 0..model.row_count() {
            let Some(mut m) = model.row_data(i) else {
                continue;
            };
            if m.sender_id.as_str() != author.to_string()
                || m.timestamp_ns.as_str() != snapshot.sent_at.to_string()
            {
                continue;
            }
            m.has_reactions = !snapshot.counts.is_empty();
            m.reaction_summary = snapshot.summary.clone().into();
            m.reactions = slint::ModelRc::new(slint::VecModel::from(
                crate::bin_ui::converters::reaction_counts_to_entries(
                    &snapshot.counts,
                    &snapshot.own_emojis,
                ),
            ));
            model.set_row_data(i, m);
        }
    };
    edit_messages(window, |model| apply(model));
    // The open thread panel keeps its own model; its replies react too.
    apply(&window.global::<crate::ChatPane>().get_thread_replies());
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

/// Apply a queued removal only while its original conversation is still open.
#[doc(hidden)]
pub fn remove_messages_in_conversation(
    window: &MainWindow,
    client: &chatter_core::client::ChatterClient,
    conversation: chatter_core::state::ActiveConversation,
    gone: impl Fn(&crate::MessageItemData) -> bool,
) {
    if client.active_conversation() == conversation {
        remove_messages_where(window, gone);
    }
}

/// Remove every row `gone` matches, in place, back to front so the indices
/// stay valid.
pub(crate) fn remove_messages_where(
    window: &MainWindow,
    gone: impl Fn(&crate::MessageItemData) -> bool,
) {
    crate::bin_ui::features::thread::remove_thread_replies_where(window, &gone);
    let mut removed_ids = Vec::new();
    let pane = window.global::<crate::ChatPane>();
    let mut unread_index = pane.get_unread_index();
    let mut unread_count = pane.get_unread_count();
    edit_messages(window, |model| {
        for i in (0..model.row_count()).rev() {
            if let Some(row) = model.row_data(i).filter(|m| gone(m)) {
                // The pill counts remote rows below the unread boundary, or
                // the live tail when no opening boundary remains. Remove a
                // deleted/expired bubble from that count in the same UI turn.
                if unread_count > 0 && !row.is_own
                    && (i as i32 >= unread_index && unread_index >= 0
                        || unread_index < 0 && i >= model.row_count().saturating_sub(unread_count as usize))
                {
                    unread_count -= 1;
                }
                if unread_index >= 0 && (i as i32) < unread_index {
                    unread_index -= 1;
                }
                removed_ids.push(row.message_id);
                model.remove(i);
            }
        }
    });
    if !removed_ids.is_empty() {
        pane.set_unread_index(if unread_count == 0 { -1 } else { unread_index });
        pane.set_unread_count(unread_count);
        window
            .global::<crate::SelectionChrome>()
            .invoke_messages_removed(slint::ModelRc::new(slint::VecModel::from(removed_ids)));
    }
}

#[cfg(test)]
mod own_echo_tests {
    use slint::Model;

    /// The sender's own bubble is this echo until the conversation is
    /// reopened; it set `content` to the raw text and no blocks, so the
    /// bubble showed `**bold** *italic*` with the markers (S9, 2026-10-05).
    #[test]
    fn own_echo_renders_markdown_like_every_other_row() {
        let row = super::build_own_message_item(
            "m1",
            "me",
            "Me",
            "**bold** *italic* ~~strike~~ and normal text",
            &[],
            false,
            "12:00",
            1,
            Vec::new(),
            false,
            "",
            "",
            slint::Image::default(),
            false,
            0,
            0,
            &chatter_core::client::ReplyParentMedia::default(),
            "sent",
        );
        assert_eq!(row.content, "bold italic strike and normal text");
        assert_eq!(
            row.rich_content.row_count(),
            1,
            "one inline run, drawn as one StyledText"
        );
        assert_eq!(
            row.rich_content.row_data(0).unwrap().plain,
            "bold italic strike and normal text"
        );
    }
}
