//! Header callbacks.
//!
//! Handles channel header actions (search, pinned, settings, load more).

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::MessageId;
use chatter_core::state::ActiveConversation;

use crate::MainWindow;
use crate::bin_ui::converters::messages_to_display_batch_deferred;

thread_local! {
    static PENDING_VISIBLE_THUMBNAILS: std::cell::RefCell<std::collections::HashMap<String, super::history_owner::HistoryOwner>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

thread_local! {
    static PENDING_JUMP: std::cell::RefCell<Option<(ActiveConversation, String)>> = const { std::cell::RefCell::new(None) };
}

thread_local! {
    // (start, end, row_count) of the media window the last eviction pass saw.
    // Lets the hot per-frame call evict only the rows that scrolled out of the
    // window, instead of rescanning the whole model every frame.
    static LAST_MEDIA_WINDOW: std::cell::Cell<Option<(usize, usize, usize)>> =
        const { std::cell::Cell::new(None) };
}

/// Row indices whose thumbnails should be dropped this pass. When the model
/// size is unchanged since the last pass, only the rows that left the window
/// are returned (`prev \ current`) — O(delta). When the size changed (history
/// prepended, a page loaded, a conversation switched) the previous indices no
/// longer map to the same rows, so a full sweep (`0..row_count` minus the
/// current window) is returned instead. Evicting an off-window thumbnail is
/// always safe — it reloads when the row is scrolled back — so a stale diff can
/// only cost an extra reload or defer one eviction, never show a wrong row.
fn media_evict_rows(
    prev: Option<(usize, usize, usize)>,
    cur_start: usize,
    cur_end: usize,
    row_count: usize,
) -> Vec<usize> {
    let in_window = |i: usize| i >= cur_start && i <= cur_end;
    match prev {
        Some((p_start, p_end, p_count)) if p_count == row_count => (p_start..=p_end)
            .filter(|&i| i < row_count && !in_window(i))
            .collect(),
        _ => (0..row_count).filter(|&i| !in_window(i)).collect(),
    }
}

/// Queue navigation until the source conversation's model has been installed.
pub(crate) fn jump_to_conversation_message(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    conversation: ActiveConversation,
    message_id: &str,
) {
    if ui_ctx.active_conversation() == conversation
        && !w.global::<crate::ChatPane>().get_loading_messages()
    {
        jump_to_message(w, ui_ctx, message_id);
        return;
    }
    PENDING_JUMP
        .with(|pending| *pending.borrow_mut() = Some((conversation, message_id.to_owned())));
    super::sidebar::select_conversation(w, conversation);
}

/// Call after installing a conversation's messages and clearing loading.
pub(crate) fn finish_pending_jump(w: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    if w.global::<crate::ChatPane>().get_loading_messages() {
        return;
    }
    let target = PENDING_JUMP.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending
            .as_ref()
            .is_some_and(|(conversation, _)| *conversation == ui_ctx.active_conversation())
        {
            pending.take()
        } else {
            None
        }
    });
    if let Some((_, message_id)) = target {
        jump_to_message(w, ui_ctx, &message_id);
    }
}

/// A manual switch away cancels pending navigation rather than resurrecting it.
pub(crate) fn cancel_pending_jump_for(conversation: ActiveConversation) {
    PENDING_JUMP.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending
            .as_ref()
            .is_some_and(|(target, _)| *target != conversation)
        {
            *pending = None;
        }
    });
}

/// Wire header callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_pinned_navigation(window, ui_ctx);
    wire_chat_settings_clicked(window, ui_ctx);
    wire_invite_member(window, ui_ctx);
    wire_load_more(window, ui_ctx);
    wire_visible_media(window, ui_ctx);
}

fn wire_visible_media(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_visible_media_rows(move |first, last| {
            if let Some(w) = weak.upgrade() {
                if maintain_history_window(&w, &client, first, last) { return; }
                refresh_visible_media(&w, &client, first, last);
            }
        });
}

/// Retain a viewport window, leaving all messages durable in core storage.
fn maintain_history_window(
    w: &MainWindow,
    client: &Arc<ChatterClient>,
    first: i32,
    last: i32,
) -> bool {
    let pane = w.global::<crate::ChatPane>();
    if pane.get_loading_messages() || last < first || last < 0 {
        return false;
    }
    let conversation = client.active_conversation();
    let Some(channel) = conversation.channel_id() else {
        return false;
    };
    if client.message_is_loading(&channel) {
        return false;
    }
    let rows = pane.get_messages();
    let Some(model) = rows
        .as_any()
        .downcast_ref::<crate::bin_ui::message_model::MessageModel>()
    else {
        return false;
    };
    if model.has_newer.get() && (last as usize).saturating_add(16) >= model.row_count() {
        let cursor = model.newer_cursor.borrow().clone().or_else(|| {
            model
                .row_data(model.row_count().saturating_sub(1))
                .map(|row| row.message_id.to_string())
        });
        if let Some(cursor) = cursor.and_then(|id| id.parse().ok()) {
            load_history(
                w,
                client,
                HistoryLoad::Newer(cursor, model.deferred_revision.get()),
            );
        }
        return false;
    }
    // Active gestures and navigation keep their complete selection/target.
    if pane.get_selection_mode()
        || !pane.get_highlighted_message_id().is_empty()
        || (pane.get_unread_index() >= 0 && pane.get_unread_count() > 0)
        || model.has_pending()
    {
        return false;
    }
    let mut protected = Vec::new();
    if pane.get_unread_index() >= 0 {
        protected.push(pane.get_unread_index() as usize);
    }
    if let Some(index) = model.row_of(pane.get_thread_root_message_id().as_str()) {
        protected.push(index);
    }
    let range = chatter_core::services::message::retained_history_range(
        model.row_count(),
        first.max(0) as usize,
        last as usize,
        &protected,
    );
    if range.start == 0 && range.end == model.row_count() {
        return false;
    }
    let (front, _) = model.retain_range(range);
    if front > 0 {
        if pane.get_unread_index() >= 0 {
            pane.set_unread_index(pane.get_unread_index() - front as i32);
        }
        pane.set_history_head_offset(pane.get_history_head_offset().saturating_add(front as i32));
        crate::bin_ui::features::chat::recompute_continuation_at(model, 0);
    }
    pane.set_history_has_newer(model.has_newer.get());
    if model.has_newer.get() {
        pane.set_is_at_bottom(false);
    }
    true
}

// Keep one small scroll cushion so a thumbnail is ready before its row enters
// the viewport. Indices change when older history is prepended.
fn media_window(first: i32, last: i32, row_count: usize) -> Option<std::ops::RangeInclusive<usize>> {
    if row_count == 0 || last < first || last < 0 {
        return None;
    }
    let start = (first.max(0) as usize).saturating_sub(8).min(row_count - 1);
    let end = (last as usize).saturating_add(8).min(row_count - 1);
    Some(start..=end)
}

#[cfg(test)]
mod media_window_tests {
    use super::{media_evict_rows, media_window};

    #[test]
    fn scroll_cushion_clamps_at_model_edges_and_rejects_empty_ranges() {
        assert_eq!(media_window(0, 3, 20), Some(0..=11));
        assert_eq!(media_window(17, 19, 20), Some(9..=19));
        assert_eq!(media_window(0, -1, 20), None);
        assert_eq!(media_window(0, 0, 0), None);
    }

    #[test]
    fn evict_full_sweep_on_first_pass_and_size_change() {
        // First pass (no previous window): every off-window row.
        assert_eq!(media_evict_rows(None, 2, 5, 10), vec![0, 1, 6, 7, 8, 9]);
        // Model grew (history prepended): previous indices are stale, so sweep.
        assert_eq!(
            media_evict_rows(Some((0, 5, 10)), 0, 5, 12),
            vec![6, 7, 8, 9, 10, 11]
        );
    }

    #[test]
    fn evict_only_rows_that_left_the_window_when_size_unchanged() {
        // Scrolled down from [0,5] to [3,8]: only 0,1,2 left the window.
        assert_eq!(media_evict_rows(Some((0, 5, 10)), 3, 8, 10), vec![0, 1, 2]);
        // No movement: nothing to evict.
        assert!(media_evict_rows(Some((3, 8, 10)), 3, 8, 10).is_empty());
    }
}

/// Keep only viewport thumbnails plus a small scroll cushion in the model.
/// Older pages can have a downloaded original but no sidecar thumbnail in
/// their metadata; opening the viewer also refreshes those visible rows.
pub(crate) fn refresh_visible_media(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    first: i32,
    last: i32,
) {
    crate::bin_ui::gif_state::auto_register_gifs_from_messages(window.as_weak(), ui_ctx.clone());
    if window.global::<crate::ChatPane>().get_loading_messages() || last < first || last < 0 {
        return;
    }
    let conversation = ui_ctx.active_conversation();
    if conversation.channel_id().is_none() {
        return;
    }
    let owner = super::history_owner::HistoryOwner::capture(ui_ctx, conversation);
    PENDING_VISIBLE_THUMBNAILS.with(|pending| {
        pending
            .borrow_mut()
            .retain(|_, request| request.is_current())
    });
    let model = window.global::<crate::ChatPane>().get_messages();
    let row_count = model.row_count();
    let Some(window_rows) = media_window(first, last, row_count) else {
        return;
    };
    // Slint rows own the decoded pixel buffer. Drop it as soon as a row is
    // outside the cushion; the bounded decode cache can then evict it. Only the
    // rows that left the window since the last pass are visited (O(delta)) — the
    // old loop rescanned the whole model on every scroll frame, which is what
    // made a long conversation scroll janky.
    let prev = LAST_MEDIA_WINDOW.with(|w| w.get());
    for index in media_evict_rows(prev, *window_rows.start(), *window_rows.end(), row_count) {
        let Some(row) = model.row_data(index) else {
            continue;
        };
        if let Some(attachments) = row
            .attachments
            .as_any()
            .downcast_ref::<slint::VecModel<crate::MessageAttachmentData>>()
        {
            for attachment_index in 0..attachments.row_count() {
                let Some(mut attachment) = attachments.row_data(attachment_index) else {
                    continue;
                };
                if attachment.has_thumbnail {
                    attachment.thumbnail = slint::Image::default();
                    attachment.has_thumbnail = false;
                    attachments.set_row_data(attachment_index, attachment);
                }
            }
        }
    }
    LAST_MEDIA_WINDOW.with(|w| w.set(Some((*window_rows.start(), *window_rows.end(), row_count))));
    let mut ids = Vec::new();
    for index in window_rows {
        let Some(row) = model.row_data(index) else {
            continue;
        };
        for attachment in row.attachments.iter() {
            // The grid paints at most ten media tiles. Hidden overflow remains
            // available to the viewer but needs no inline thumbnail texture.
            if (attachment.is_image || attachment.is_video)
                // Static images use the bounded, cancellable core preview loader.
                && (!attachment.is_image || attachment.mime_type == "image/gif"
                    || attachment.filename.to_lowercase().ends_with(".gif"))
                && (attachment.album_count < 2 || attachment.album_index < 10)
                && !attachment.has_thumbnail
            {
                let id = attachment.id.to_string();
                let inserted = PENDING_VISIBLE_THUMBNAILS.with(|pending| {
                    let mut pending = pending.borrow_mut();
                    if pending.contains_key(&id) {
                        false
                    } else {
                        pending.insert(id.clone(), owner.clone());
                        true
                    }
                });
                if inserted {
                    ids.push(id);
                }
            }
        }
    }

    for id in ids {
        let weak = window.as_weak();
        let client = ui_ctx.clone();
        let owner = owner.clone();
        let session = ui_ctx.session_guard();
        let generation = crate::bin_ui::decode_cache::thumbnail_generation();
        crate::rt_handle().spawn(async move {
            let (pixels, dimensions) =
                match hydrate_visible_media(&client, &id, session, &owner, generation).await {
                    Some((pixels, dimensions)) => (Some(pixels), dimensions),
                    None => (None, None),
                };
            let _ = slint::invoke_from_event_loop(move || {
                PENDING_VISIBLE_THUMBNAILS.with(|pending| {
                    let mut pending = pending.borrow_mut();
                    if owner.is_current() {
                        pending.remove(&id);
                    }
                });
                let Some(w) = weak.upgrade() else { return };
                if !owner.is_current() {
                    return;
                }
                let Some(pixels) = pixels else { return };
                let model = w.global::<crate::ChatPane>().get_messages();
                let Some(window_rows) = media_window(
                    w.global::<crate::ChatPane>()
                        .get_message_first_visible_row(),
                    w.global::<crate::ChatPane>().get_message_last_visible_row(),
                    model.row_count(),
                ) else {
                    return;
                };
                for row_index in window_rows {
                    let Some(row) = model.row_data(row_index) else {
                        continue;
                    };
                    for attachment_index in 0..row.attachments.row_count() {
                        let Some(mut attachment) = row.attachments.row_data(attachment_index)
                        else {
                            continue;
                        };
                        if attachment.id.as_str() == id && !attachment.has_thumbnail {
                            if let Some((width, height)) = dimensions {
                                attachment.width = width as i32;
                                attachment.height = height as i32;
                            }
                            attachment.thumbnail = slint::Image::from_rgba8(pixels.clone());
                            attachment.has_thumbnail = true;
                            if let Some(attachments) = row
                                .attachments
                                .as_any()
                                .downcast_ref::<slint::VecModel<crate::MessageAttachmentData>>()
                            {
                                attachments.set_row_data(attachment_index, attachment);
                            }
                        }
                    }
                }
            });
        });
    }
}

/// Core owns routing/authentication/bounded decoding for every viewport miss.
async fn hydrate_visible_media(
    client: &Arc<ChatterClient>,
    id: &str,
    session: chatter_core::client::SessionGuard,
    owner: &super::history_owner::HistoryOwner,
    generation: u64,
) -> Option<(crate::bin_ui::decode_cache::Pixels, Option<(u32, u32)>)> {
    if !owner.is_current() || !session.is_current() {
        return None;
    }
    if let Some(pixels) = crate::bin_ui::decode_cache::cached_thumbnail(id, 0) {
        return Some((pixels, None));
    }
    let preview = client
        .prepare_visible_attachment_preview(id.parse().ok()?, session)
        .await?;
    if !owner.is_current() {
        return None;
    }
    let dimensions = Some((preview.width, preview.height));
    let pixels = crate::bin_ui::decode_cache::prepared_thumbnail(
        id,
        generation,
        image::DynamicImage::ImageRgba8(preview.pixels),
    )?;
    Some((pixels, dimensions))
}

fn wire_pinned_navigation(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Previous pinned message
    let weak_window = window.as_weak();
    let ui_ctx_prev = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_pinned_prev_clicked(move || {
            ui_ctx_prev.pinned_navigate_prev();
            update_pinned_ui_from_loaded_messages(
                weak_window.clone(),
                ui_ctx_prev.clone(),
                ui_ctx_prev.active_conversation(),
            );
        });

    // Next pinned message
    let weak_window = window.as_weak();
    let ui_ctx_next = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_pinned_next_clicked(move || {
            ui_ctx_next.pinned_navigate_next();
            update_pinned_ui_from_loaded_messages(
                weak_window.clone(),
                ui_ctx_next.clone(),
                ui_ctx_next.active_conversation(),
            );
        });

    // Dismiss pinned bar
    let ui_ctx_dismiss = ui_ctx.clone();
    let weak_window = window.as_weak();

    window
        .global::<crate::ChatPane>()
        .on_pinned_dismiss_clicked(move || {
            ui_ctx_dismiss.pinned_dismiss();
            if let Some(w) = weak_window.upgrade() {
                w.global::<crate::ChatPane>().set_has_pinned_messages(false);
            }
            tracing::debug!("Pinned bar dismissed");
        });

    // Header menu opens the complete core-ordered pin list, not just loaded rows.
    let weak_window = window.as_weak();
    let ui_ctx_show = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_pinned_show_clicked(move || {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_show.clone();
            let conversation = ui_ctx.active_conversation();
            let revision = ui_ctx.auth_snapshot().session_revision;
            tokio::spawn(async move {
                let mut items = ui_ctx.pinned_display_items(conversation).await;
                for item in &mut items {
                    item.preview = pinned_plaintext_preview(
                        &ui_ctx,
                        conversation,
                        &item.message_id,
                        &item.preview,
                    );
                }
                #[cfg(target_os = "android")]
                let items = {
                    let client = ui_ctx.clone();
                    tokio::task::spawn_blocking(move || {
                        for item in items.iter_mut().filter(|i| i.thumbnail.is_empty()).take(4) {
                            item.thumbnail =
                                pinned_video_thumbnail(&client, conversation, &item.message_id);
                        }
                        items
                    })
                    .await
                    .unwrap_or_default()
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else {
                        return;
                    };
                    if ui_ctx.auth_snapshot().session_revision != revision {
                        return;
                    }
                    if ui_ctx.active_conversation() != conversation {
                        return;
                    }
                    let rows: Vec<crate::PinnedItem> = items
                        .into_iter()
                        .map(|item| crate::PinnedItem {
                            message_id: item.message_id.to_string().into(),
                            preview: item.preview.into(),
                            sender_name: item.sender_name.into(),
                            sent_time: item.sent_time.into(),
                            thumbnail: crate::bin_ui::converters::avatar_bytes_to_image(
                                &item.thumbnail,
                            )
                            .unwrap_or_default(),
                        })
                        .collect();
                    w.global::<crate::ChatPane>()
                        .set_pinned_items(ModelRc::new(slint::VecModel::from(rows)));
                    super::back::push(&w, "pinned-messages");
                });
            });
        });
    let weak_window = window.as_weak();
    let ui_ctx_selected = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_pinned_item_selected(move |message_id| {
            let Some(w) = weak_window.upgrade() else {
                return;
            };
            super::back::close(&w, "pinned-messages");
            jump_to_message(&w, &ui_ctx_selected, &message_id);
        });

    // Go to pinned message (scroll to it in chat)
    let weak_window = window.as_weak();
    let ui_ctx_goto = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_pinned_go_to_message(move || {
            if let (Some(message_id), Some(w)) = (
                ui_ctx_goto.pinned_current_message_id(),
                weak_window.upgrade(),
            ) {
                jump_to_message(&w, &ui_ctx_goto, &message_id.to_string());
            }
        });
}

/// Resolve the full text before stripping/truncating; core previews can already
/// end halfway through a formatting marker. Non-text captions remain literal.
fn pinned_plaintext_preview(
    ui_ctx: &ChatterClient,
    conversation: ActiveConversation,
    message_id: &MessageId,
    caption: &str,
) -> String {
    let message = match conversation {
        ActiveConversation::AnonGroup(_) => ui_ctx.get_anon_group_message(
            &chatter_core::domain::anon_group::AnonGroupMessageId::from_string(
                message_id.to_string(),
            ),
        ),
        _ => ui_ctx.get_message_local(message_id),
    };
    crate::bin_ui::converters::pin_preview::preview_or_caption(
        message.as_ref().and_then(|message| message.as_text()),
        caption,
    )
}

/// Missing old video pins can derive a local frame before history has been loaded.
#[cfg(target_os = "android")]
fn pinned_video_thumbnail(
    client: &ChatterClient,
    conversation: ActiveConversation,
    id: &MessageId,
) -> Vec<u8> {
    let message = match conversation {
        ActiveConversation::AnonGroup(_) => client.get_anon_group_message(
            &chatter_core::domain::anon_group::AnonGroupMessageId::from_string(id.to_string()),
        ),
        _ => client.get_message_local(id),
    };
    let Some(message) = message else {
        return Vec::new();
    };
    for id in message.attachments.iter().take(16) {
        let Some(attachment) = client.get_attachment_metadata(id) else {
            continue;
        };
        if !attachment.metadata.is_video() {
            continue;
        }
        if let Some(bytes) = attachment.metadata.thumbnail.filter(|b| !b.is_empty()) {
            return bytes;
        }
        return client
            .decrypt_and_extract_video_thumbnail(id)
            .map(|result| result.image_bytes)
            .unwrap_or_default();
    }
    Vec::new()
}

pub(crate) fn refresh_pinned_ui(w: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    update_pinned_ui_from_loaded_messages(
        w.as_weak(),
        ui_ctx.clone(),
        ui_ctx.active_conversation(),
    );
}

/// Update pinned UI by reading the preview from loaded UI messages first,
/// falling back to PinService (regular channels) or AnonGroupService (private groups).
fn update_pinned_ui_from_loaded_messages(
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    conversation: ActiveConversation,
) {
    let state = ui_ctx.chat_snapshot();
    let current_index = state.pinned.current_index as i32;
    let total_count = state.pinned.pinned_message_ids.len() as i32;
    let current_msg_id = ui_ctx.pinned_current_message_id();

    // First try to get preview from currently loaded UI messages (works for all types)
    let msg_id_str = current_msg_id.map(|id| id.to_string()).unwrap_or_default();
    let ui_preview = weak_window.upgrade().and_then(|w| {
        let messages = w.global::<crate::ChatPane>().get_messages();
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.message_id.as_str() == msg_id_str {
                    let preview = crate::bin_ui::features::auth::inbox::utils::generate_preview_from_ui_message(&msg);
                    if !preview.is_empty() {
                        let thumbnail = msg.attachments.iter()
                            .find(|a| (a.is_image || a.is_video) && a.has_thumbnail)
                            .map(|a| a.thumbnail).unwrap_or_default();
                        let missing_video = msg.attachments.iter().any(|a| a.is_video && !a.has_thumbnail);
                        return Some((preview, thumbnail, missing_video));
                    }
                }
            }
        }
        None
    });

    if let Some((preview, thumbnail, missing_video)) = ui_preview {
        let derive_video =
            cfg!(target_os = "android") && missing_video && thumbnail.size().width == 0;
        // Already have preview from UI, update directly (already on UI thread from callback)
        if let Some(w) = weak_window.upgrade() {
            w.global::<crate::ChatPane>()
                .set_pinned_current_index(current_index + 1);
            w.global::<crate::ChatPane>()
                .set_pinned_total_count(total_count);
            w.global::<crate::ChatPane>()
                .set_pinned_message_preview(preview.into());
            w.global::<crate::ChatPane>()
                .set_pinned_thumbnail(thumbnail);
            w.global::<crate::ChatPane>()
                .set_has_pinned_messages(total_count > 0);
        }
        if !derive_video {
            return;
        }
    }

    // Fall back to DB lookup in background thread
    let revision = ui_ctx.auth_snapshot().session_revision;
    std::thread::spawn(move || {
        let preview = match current_msg_id {
            Some(msg_id) => pinned_plaintext_preview(
                &ui_ctx,
                conversation,
                &msg_id,
                &ui_ctx.pinned_message_preview(&conversation, &msg_id),
            ),
            None => String::new(),
        };

        let thumbnail = current_msg_id
            .and_then(|id| ui_ctx.get_pinned_message(&id))
            .map(|pin| pin.thumbnail)
            .unwrap_or_default();
        #[cfg(target_os = "android")]
        let thumbnail = if thumbnail.is_empty() {
            current_msg_id
                .map(|id| pinned_video_thumbnail(&ui_ctx, conversation, &id))
                .unwrap_or_default()
        } else {
            thumbnail
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_window.upgrade() {
                if ui_ctx.active_conversation() != conversation
                    || ui_ctx.pinned_current_message_id() != current_msg_id
                    || ui_ctx.auth_snapshot().session_revision != revision
                {
                    return;
                }
                w.global::<crate::ChatPane>().set_pinned_thumbnail(
                    crate::bin_ui::converters::avatar_bytes_to_image(&thumbnail)
                        .unwrap_or_default(),
                );
                w.global::<crate::ChatPane>()
                    .set_pinned_current_index(current_index + 1);
                w.global::<crate::ChatPane>()
                    .set_pinned_total_count(total_count);
                w.global::<crate::ChatPane>()
                    .set_pinned_message_preview(preview.into());
                w.global::<crate::ChatPane>()
                    .set_has_pinned_messages(total_count > 0);
            }
        });
    });
}

fn wire_chat_settings_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_chat_settings_clicked(move || {
            let conversation = ui_ctx.active_conversation();

            if let ActiveConversation::AnonGroup(group_id) = conversation {
                // Private group settings - move DB queries to background thread
                let ui_ctx = ui_ctx.clone();
                let weak = weak_window.clone();
                let opened = ActiveConversation::AnonGroup(group_id);
                if !super::modals::settings_open_allowed(ui_ctx.active_conversation(), opened) {
                    return;
                }
                let group_id_str = group_id.to_string();
                if let Some(w) = weak.upgrade() {
                    let already = w
                        .global::<crate::ModalHostState>()
                        .get_channel_settings_id()
                        .as_str()
                        == group_id_str
                        && w.global::<crate::ModalHostState>()
                            .get_modal_stack()
                            .iter()
                            .any(|s| s.as_str() == "channel-settings");
                    if !already {
                        super::modals::show_channel_settings_shell(
                            &w,
                            &group_id_str,
                            "private-group",
                        );
                    }
                }

                std::thread::spawn(move || {
                    // Shared with the live refresh on membership events
                    // (`refresh_members_if_open`), so both read the same roster.
                    let Some(roster) = ui_ctx.private_group_settings_roster(group_id) else {
                        let ui_ctx = ui_ctx.clone();
                        let weak = weak.clone();
                        let id = group_id_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(w) = weak.upgrade() else { return };
                            let current_id = w
                                .global::<crate::ModalHostState>()
                                .get_channel_settings_id();
                            let on_stack = w
                                .global::<crate::ModalHostState>()
                                .get_modal_stack()
                                .iter()
                                .any(|s| s.as_str() == "channel-settings");
                            if !super::modals::settings_refresh_allowed(
                                ui_ctx.active_conversation(),
                                opened,
                                current_id.as_str(),
                                &id,
                                on_stack,
                            ) {
                                return;
                            }
                            super::modals::present_settings_error(
                                &w,
                                &id,
                                "private-group",
                                "Private group not found",
                            );
                        });
                        return;
                    };
                    let muted = ui_ctx.is_conversation_muted(opened).unwrap_or(false);
                    let notification_preference = ui_ctx
                        .conversation_notification_preference(opened)
                        .unwrap_or_default();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(w) = weak.upgrade() else { return };
                        let current_id = w
                            .global::<crate::ModalHostState>()
                            .get_channel_settings_id();
                        let on_stack = w
                            .global::<crate::ModalHostState>()
                            .get_modal_stack()
                            .iter()
                            .any(|s| s.as_str() == "channel-settings");
                        if !super::modals::settings_refresh_allowed(
                            ui_ctx.active_conversation(),
                            opened,
                            current_id.as_str(),
                            &group_id_str,
                            on_stack,
                        ) {
                            return;
                        }
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_id(group_id_str.as_str().into());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_name(roster.name.as_str().into());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_name_draft(roster.name.as_str().into());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_description("".into());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_description_draft("".into());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_ai_enabled(ui_ctx.ai_enabled_for(&group_id_str));
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_muted(muted);
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_notification_pref(
                                super::modals::preference_label(notification_preference).into(),
                            );
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_type("private-group".into());
                        super::modals::apply_private_group_roster(&w, roster);
                        // Clear a previous channel's ban list.
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_can_view_bans(false);
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_banned_members(ModelRc::default());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_loading(false);
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_error("".into());
                        tracing::debug!("Opened settings for private group {}", group_id);
                    });
                });
                return;
            }

            // A DM's settings screen is the peer's profile — block, verify, safety
            // number, remove friend all live there, the same way Telegram hangs a
            // DM's settings off the contact page. The kebab used to route to
            // channel settings instead, which for a channel-free (Signal-style)
            // server DM missed in `get_channel_local` and silently did nothing:
            // the button was on screen in every DM and had never worked.
            if let ActiveConversation::ServerDm { peer_id } = conversation {
                let weak = weak_window.clone();
                let ui_ctx = ui_ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        super::modals::open_user_profile(&w, &peer_id, Some(&ui_ctx));
                        crate::bin_ui::features::back::push(&w, "user-profile");
                    }
                });
                return;
            }

            // Regular channel settings
            let channel_id = match conversation {
                ActiveConversation::Channel(id) => id,
                _ => {
                    tracing::warn!("Cannot open channel settings: no active channel");
                    return;
                }
            };

            super::modals::open_channel_settings(weak_window.clone(), ui_ctx.clone(), channel_id);
        });
}

fn wire_invite_member(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_invite_member(move || {
            let conversation = ui_ctx.active_conversation();
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();

            match conversation {
                ActiveConversation::AnonGroup(group_id) => {
                    crate::bin_ui::features::modals::open_private_group_invite_modal(
                        weak, ui_ctx, group_id,
                    );
                }
                ActiveConversation::Channel(channel_id) => {
                    crate::bin_ui::features::modals::open_invite_member_modal(
                        weak, ui_ctx, channel_id,
                    );
                }
                _ => {
                    tracing::debug!("Invite not available for this conversation type");
                }
            }
        });
}

fn wire_load_more(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    let latest_window = window.as_weak();
    let latest_client = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_request_latest(move || {
            if let Some(w) = latest_window.upgrade() {
                load_history(&w, &latest_client, HistoryLoad::Newest);
            }
        });
    window.global::<crate::ChatPane>().on_load_more(move || {
        if let Some(w) = weak_window.upgrade() {
            let oldest = w
                .global::<crate::ChatPane>()
                .get_messages()
                .row_data(0)
                .and_then(|m| m.message_id.parse::<MessageId>().ok());
            if let Some(oldest) = oldest {
                load_history(&w, &ui_ctx, HistoryLoad::Older(oldest));
            }
        }
    });
}

/// Row of the open list holding `message_id`.
fn row_of(w: &MainWindow, message_id: &str) -> Option<usize> {
    let messages = w.global::<crate::ChatPane>().get_messages();
    if let Some(model) = messages
        .as_any()
        .downcast_ref::<crate::bin_ui::message_model::MessageModel>()
    {
        return model.row_of(message_id);
    }
    (0..messages.row_count()).find(|&i| {
        messages
            .row_data(i)
            .is_some_and(|m| m.message_id.as_str() == message_id)
    })
}

/// Scroll the open conversation to `message_id` and flash it: a quote's
/// parent, a search hit, the pinned message. One that is older than what is
/// loaded has the history back to it loaded first. UI thread.
pub(crate) fn jump_to_message(w: &MainWindow, ui_ctx: &Arc<ChatterClient>, message_id: &str) {
    let Some(row) = row_of(w, message_id) else {
        match message_id.parse::<MessageId>() {
            // A thread reply is not a row of this list: open its thread, as
            // the "N replies" chip does.
            Ok(id) => match ui_ctx.get_message_local(&id).and_then(|m| m.thread_id) {
                Some(root) => {
                    let pane = w.global::<crate::ChatPane>();
                    pane.set_thread_focus_reply_id(message_id.into());
                    pane.invoke_thread_clicked(root.to_string().into());
                }
                None => load_history(w, ui_ctx, HistoryLoad::Reaching(id)),
            },
            Err(_) => {
                tracing::debug!(message_id, "Jump target is not a stored message");
                report_jump_unavailable(w);
            }
        }
        return;
    };
    w.global::<crate::ChatPane>()
        .set_scroll_to_message_index(row as i32);
    w.global::<crate::ChatPane>()
        .set_highlighted_message_id(message_id.into());

    // A brief pulse, not a permanent highlight. The index goes back with it:
    // the list acts on it changing, so left in place a second jump to the
    // same row would do nothing.
    let weak = w.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(1500), move || {
        if let Some(w) = weak.upgrade() {
            w.global::<crate::ChatPane>()
                .set_highlighted_message_id(slint::SharedString::new());
            w.global::<crate::ChatPane>()
                .set_scroll_to_message_index(-1);
        }
    });
}

/// A jump whose target is not in this device's history (deleted, expired,
/// never synced here) says so instead of doing nothing. Local
/// history only: there is no server fetch behind a jump. UI thread.
fn report_jump_unavailable(w: &MainWindow) {
    w.set_toast_message("That message isn't available on this device.".into());
    w.set_toast_variant("error".into());
    w.set_toast_visible(true);
}

/// `report_jump_unavailable` from the history thread, for the request that
/// is still current.
fn report_jump_unavailable_later(
    weak: slint::Weak<MainWindow>,
    owner: super::history_owner::HistoryOwner,
) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            if owner.is_current() {
                report_jump_unavailable(&w);
            }
        }
    });
}

/// Older history requested by the list, or a jump target that is not loaded.
enum HistoryLoad {
    /// One page older than the oldest loaded row, inserted above it.
    Older(MessageId),
    Newer(MessageId, u64),
    Newest,
    /// The newest rows grown back to this message, installed in place of the
    /// list, then jumped to.
    Reaching(MessageId),
}

/// Load history into the open list. The oldest loaded row is the paging
/// cursor — the list itself, not a copy of where it was last time. UI thread.
///
/// A jump is not a prepend. Inserting rows in the same turn as the seek
/// races the list's own bookkeeping (message_list.slint `settle`: a pass
/// that sees `grown != 0` drops `jump-row`, then restores the old top row),
/// and a jump to a search hit older than the open page stayed at the bottom
/// (device ui135, desktop Foxtrot, "p135-007"). It installs the page core
/// grew back to the target as a fresh list instead, the way a conversation
/// opens, and jumps once that list exists.
fn load_history(w: &MainWindow, ui_ctx: &Arc<ChatterClient>, load: HistoryLoad) {
    // Bind this request before spawning; a later manual switch must never
    // reinterpret the target as belonging to the newly opened conversation.
    let conversation = ui_ctx.active_conversation();
    let Some(channel_id) = conversation.channel_id() else {
        return;
    };
    let Some(conversation_type) = ui_ctx.active_conversation_type() else {
        return;
    };
    let jump = matches!(load, HistoryLoad::Reaching(_));
    let Some(owner) = super::history_owner::HistoryOwner::admit_older(ui_ctx, conversation) else {
        if matches!(load, HistoryLoad::Newest) {
            let request = super::history_owner::HistoryOwner::capture(ui_ctx, conversation);
            let weak = w.as_weak();
            let client = ui_ctx.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(16), move || {
                if request.is_current() {
                    if let Some(w) = weak.upgrade() {
                        load_history(&w, &client, HistoryLoad::Newest);
                    }
                }
            });
        }
        if jump {
            w.set_toast_message("History is still loading. Try again in a moment.".into());
            w.set_toast_variant("info".into());
            w.set_toast_visible(true);
        }
        return;
    };
    // A fresh page is on its way: the previous failure's Retry chip goes.
    w.global::<crate::ChatPane>().set_history_load_failed(false);
    let weak = w.as_weak();
    let ui_ctx = ui_ctx.clone();

    std::thread::spawn(move || {
        if !owner.is_current() {
            ui_ctx.set_message_loading(channel_id, false);
            return;
        }
        let messages = match &load {
            HistoryLoad::Older(before) => {
                ui_ctx.get_older_messages_local_asc(&channel_id, &conversation_type, before)
            }
            HistoryLoad::Newer(after, _) => {
                ui_ctx.get_newer_messages_local_asc(&channel_id, &conversation_type, after)
            }
            HistoryLoad::Newest => ui_ctx.get_messages_local_asc(
                &channel_id,
                &conversation_type,
                None,
                chatter_core::config::MESSAGE_PAGE_SIZE as u32,
            ),
            HistoryLoad::Reaching(target) => {
                ui_ctx.get_messages_local_reaching(&channel_id, &conversation_type, target)
            }
        };
        ui_ctx.set_message_loading(channel_id, false);
        let messages = match messages {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::error!("Failed to load more messages: {}", e);
                if jump {
                    report_jump_unavailable_later(weak, owner);
                } else {
                    // Ordinary paging: the list keeps its rows and anchor and
                    // shows a Retry chip (message_list.slint `load-failed`).
                    // Only for the conversation that asked.
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            if owner.is_current() {
                                w.global::<crate::ChatPane>().set_history_load_failed(true);
                            }
                        }
                    });
                }
                return;
            }
        };
        if messages.is_empty() {
            if matches!(load, HistoryLoad::Newest) {
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    if !owner.is_current() {
                        return;
                    }
                    let pane = w.global::<crate::ChatPane>();
                    let pending = pane
                        .get_messages()
                        .iter()
                        .filter(|row| row.message_id.starts_with("pending:"))
                        .collect::<Vec<_>>();
                    pane.set_messages(ModelRc::new(
                        crate::bin_ui::message_model::MessageModel::from(pending),
                    ));
                    pane.set_history_has_newer(false);
                    pane.set_is_at_bottom(true);
                });
                return;
            }
            if let HistoryLoad::Newer(_, revision) = load {
                let weak = weak.clone();
                let client = ui_ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else { return };
                    if !owner.is_current() {
                        return;
                    }
                    let pane = w.global::<crate::ChatPane>();
                    let rows = pane.get_messages();
                    let Some(model) = rows
                        .as_any()
                        .downcast_ref::<crate::bin_ui::message_model::MessageModel>()
                    else {
                        return;
                    };
                    if model.deferred_revision.get() == revision {
                        model.has_newer.set(false);
                        pane.set_history_has_newer(false);
                    } else {
                        maintain_history_window(
                            &w,
                            &client,
                            pane.get_message_first_visible_row(),
                            pane.get_message_last_visible_row(),
                        );
                    }
                });
                return;
            }
            tracing::debug!("No older messages for channel: {}", channel_id);
            if jump {
                report_jump_unavailable_later(weak, owner);
            }
            return;
        }

        let fetched_cursor = messages.last().map(|row| row.id.to_string());
        let messages = chatter_core::services::disappearing::visible_history_messages(messages);
        if !owner.is_current() {
            return;
        }
        // Convert older messages to display data
        let mut display_messages = crate::rt_handle().block_on(messages_to_display_batch_deferred(
            &messages,
            |ms| ui_ctx.message_sender_display_names(ms),
            |m| ui_ctx.message_sender_avatar(m),
            crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
            |m| ui_ctx.reply_author_display_name(m),
            |m| ui_ctx.reply_parent_missing_for(m),
            |m| ui_ctx.reply_parent_media(m),
            |m| ui_ctx.mention_members(&m.conversation_type, &m.conversation_id),
        ));
        crate::bin_ui::converters::mark_disappearing(&ui_ctx, &mut display_messages);
        crate::bin_ui::converters::apply_reply_counts(
            &mut display_messages,
            &ui_ctx.thread_reply_counts(&channel_id, &conversation_type),
        );

        // Get current user ID for is_own calculation
        let current_user_id = ui_ctx
            .session_current_user_id()
            .map(|id| id.to_string())
            .unwrap_or_default();

        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            // The bytes remain private to their original session and history request.
            if !owner.is_current() {
                return;
            }
            // Convert display data to Slint types (must be done on UI thread)
            let slint_messages: Vec<_> = display_messages
                .iter()
                .map(|d| d.to_slint_preview_with_user(&current_user_id))
                .collect();

            if matches!(load, HistoryLoad::Newest) {
                let pane = w.global::<crate::ChatPane>();
                let mut rows = slint_messages;
                let target = rows
                    .last()
                    .map(|row| row.message_id.to_string())
                    .unwrap_or_default();
                // Pending rows are read at apply time: a send that already
                // failed/settled cannot be resurrected by this async reload.
                rows.extend(
                    pane.get_messages()
                        .iter()
                        .filter(|row| row.message_id.starts_with("pending:")),
                );
                install_and_jump(&w, &ui_ctx, owner, rows, target);
                let rows = pane.get_messages();
                if let Some(model) = rows
                    .as_any()
                    .downcast_ref::<crate::bin_ui::message_model::MessageModel>()
                {
                    model.has_newer.set(true);
                    *model.newer_cursor.borrow_mut() = fetched_cursor;
                    pane.set_history_has_newer(true);
                }
                return;
            }
            if let HistoryLoad::Newer(_, _) = load {
                let pane = w.global::<crate::ChatPane>();
                let rows = pane.get_messages();
                if let Some(model) = rows
                    .as_any()
                    .downcast_ref::<crate::bin_ui::message_model::MessageModel>()
                {
                    let fresh = ui_ctx.unseen_history_rows(slint_messages, model.ids(), |row| {
                        row.message_id.as_str()
                    });
                    let start = if model.has_pending() {
                        0
                    } else {
                        model.row_count()
                    };
                    model.append_page(fresh);
                    *model.newer_cursor.borrow_mut() = fetched_cursor;
                    if pane.get_conversation_kind() == crate::ConversationKind::Channel {
                        for index in start..model.row_count() {
                            crate::bin_ui::features::chat::recompute_continuation_at(model, index);
                        }
                    }
                    // Continue over invisible expired rows and close the gap
                    // only after an empty page observes no deferred live input.
                    maintain_history_window(
                        &w,
                        &ui_ctx,
                        pane.get_message_first_visible_row(),
                        pane.get_message_last_visible_row(),
                    );
                }
                return;
            }
            let HistoryLoad::Reaching(target) = load else {
                let current_model = w.global::<crate::ChatPane>().get_messages();
                let current_ids = current_model
                    .as_any()
                    .downcast_ref::<crate::bin_ui::message_model::MessageModel>()
                    .map(|model| model.ids())
                    .unwrap_or_else(|| {
                        current_model
                            .iter()
                            .map(|msg| msg.message_id.to_string())
                            .collect()
                    });

                // Prepend older messages (older go at top), skipping
                // duplicates. Inserted in place: rows added above the
                // first visible one shift the list's index without
                // moving what is on screen, so history loads without
                // a jump.
                let older = ui_ctx.unseen_history_rows(
                    slint_messages,
                    current_ids,
                    |msg: &crate::MessageItemData| msg.message_id.as_str(),
                );
                tracing::debug!("Loaded {} older messages for channel", older.len());
                crate::bin_ui::features::chat::edit_messages(&w, |model| {
                    let count = model.prepend(older);
                    // Only the prepended page and the old first row can change.
                    (0..=count).collect()
                });
                return;
            };
            install_and_jump(&w, &ui_ctx, owner, slint_messages, target.to_string());
        });
    });
}

/// Replace the open list with `rows` and jump to `target` in it.
///
/// `loading-messages` is raised for the swap so the list (which lives under
/// `!loading`, message_list.slint) is torn down and built fresh on the new
/// model — no previous row count for the jump to be measured against. It is
/// lowered on a later event-loop turn, not in this one: a property set and
/// reset in the same turn is never observed, and the old list would survive.
/// The jump then lands on a fresh list, as it does for a search hit that
/// opens its conversation (`finish_pending_jump`). UI thread.
fn install_and_jump(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    owner: super::history_owner::HistoryOwner,
    rows: Vec<crate::MessageItemData>,
    target: String,
) {
    let pane = w.global::<crate::ChatPane>();
    pane.set_loading_messages(true);
    pane.set_messages(ModelRc::new(
        crate::bin_ui::message_model::MessageModel::from(rows),
    ));
    let rows = pane.get_messages();
    if let Some(model) = rows
        .as_any()
        .downcast_ref::<crate::bin_ui::message_model::MessageModel>()
    {
        model.has_newer.set(true);
        pane.set_history_has_newer(true);
    }
    let weak = w.as_weak();
    let ui_ctx = ui_ctx.clone();
    slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
        let Some(w) = weak.upgrade() else { return };
        if !owner.is_current() {
            return;
        }
        w.global::<crate::ChatPane>().set_loading_messages(false);
        if row_of(&w, &target).is_some() {
            jump_to_message(&w, &ui_ctx, &target);
        } else {
            report_jump_unavailable(&w);
        }
    });
}

#[cfg(test)]
mod visible_preview_tests {
    use super::*;

    #[test]
    fn visible_hydration_is_small_durable_and_cannot_adopt_a_late_view_or_session() {
        const CHILD: &str = "CHATTER_VISIBLE_IMAGE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let test = format!(
                "{}::visible_hydration_is_small_durable_and_cannot_adopt_a_late_view_or_session",
                module_path!().split_once("::").unwrap().1
            );
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _runtime = runtime.enter();
        let dir = tempfile::tempdir().unwrap();
        let database = chatter_core::system::Directories::with_base_dir(dir.path().to_path_buf())
            .unwrap()
            .database_path();
        std::fs::write(
            database
                .parent()
                .unwrap()
                .join(chatter_core::system::keyring::NO_KEYRING_MARKER),
            b"",
        )
        .unwrap();
        let client = ChatterClient::builder()
            .data_dir(dir.path().to_string_lossy().to_string())
            .build()
            .unwrap();
        let a = ActiveConversation::Channel(chatter_core::domain::ids::ChannelId::new());
        let b = ActiveConversation::Channel(chatter_core::domain::ids::ChannelId::new());
        client.select_conversation(a);
        let mut plaintext = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2048, 1024)
            .write_to(&mut plaintext, image::ImageFormat::Jpeg)
            .unwrap();
        let encrypted =
            chatter_core::crypto::attachment_crypto::AttachmentCrypto::encrypt(plaintext.get_ref())
                .unwrap();
        let source = chatter_core::domain::attachment::Attachment {
            id: chatter_core::domain::ids::AttachmentId::new(),
            metadata: chatter_core::domain::attachment::FileMetadata::new(
                "visible.jpg".into(),
                "image/jpeg".into(),
                plaintext.get_ref().len() as u64,
            ),
            status: chatter_core::domain::attachment::AttachmentStatus::Ready,
            content_location: chatter_core::domain::attachment::ContentLocation::OnFilesystem,
            remote_url: Some("owned-visible-image".into()),
            encryption_key: Some(encrypted.key.to_vec()),
            encryption_iv: Some(encrypted.digest.to_vec()),
            created_at: chrono::Utc::now(),
            secret_chat: false,
        };
        client
            .attachments()
            .attachment_repo()
            .insert(&source)
            .unwrap();
        chatter_core::services::attachment::ensure_attachments_dir().unwrap();
        let path = chatter_core::services::attachment::attachment_file_path(&source.id);
        std::fs::write(&path, &encrypted.ciphertext).unwrap();
        let id = source.id.to_string();
        // Construct requests before the executor gets a chance to poll them.
        // Neither an A->B->A selection nor a changed account may be adopted.
        for logout in [false, true] {
            let owner = super::super::history_owner::HistoryOwner::begin(&client, a);
            let pending = hydrate_visible_media(
                &client,
                &id,
                client.session_guard(),
                &owner,
                crate::bin_ui::decode_cache::thumbnail_generation(),
            );
            if logout {
                client.testing_invalidate_session();
            } else {
                client.select_conversation(b);
                super::super::history_owner::HistoryOwner::begin(&client, b);
                client.select_conversation(a);
                super::super::history_owner::HistoryOwner::begin(&client, a);
            }
            assert!(runtime.block_on(pending).is_none());
            assert!(
                client
                    .get_attachment_metadata(&source.id)
                    .unwrap()
                    .metadata
                    .thumbnail
                    .is_none()
            );
        }
        let owner = super::super::history_owner::HistoryOwner::begin(&client, a);
        let (pixels, dimensions) = runtime
            .block_on(hydrate_visible_media(
                &client,
                &id,
                client.session_guard(),
                &owner,
                crate::bin_ui::decode_cache::thumbnail_generation(),
            ))
            .unwrap();
        assert_eq!((pixels.width(), pixels.height()), (320, 160));
        assert_eq!(dimensions, Some((2048, 1024)));
        assert_eq!(std::fs::read(&path).unwrap(), encrypted.ciphertext);
        // Simulate eviction/reopen: the authenticated original must not be
        // needed again, even after the decoded-memory cache has been wiped.
        std::fs::remove_file(&path).unwrap();
        crate::bin_ui::decode_cache::on_lock();
        let (pixels, dimensions) = runtime
            .block_on(hydrate_visible_media(
                &client,
                &id,
                client.session_guard(),
                &owner,
                crate::bin_ui::decode_cache::thumbnail_generation(),
            ))
            .unwrap();
        assert_eq!((pixels.width(), pixels.height()), (320, 160));
        assert_eq!(dimensions, Some((2048, 1024)));
    }
}
