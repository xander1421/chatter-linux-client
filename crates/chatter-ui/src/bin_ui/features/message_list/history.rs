//! The retained history window and paging: trimming rows outside the
//! viewport, loading older/newer pages, and the newest-page reload.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::MessageId;
use chatter_core::media::performance_trace as viewport_trace;

use crate::MainWindow;
use super::jump::{install_and_jump, report_jump_unavailable_later};
use chatter_core::client::PageAnchor;

/// Retain a viewport window, leaving all messages durable in core storage.
pub(super) fn maintain_history_window(
    w: &MainWindow,
    client: &Arc<ChatterClient>,
    first: i32,
    last: i32,
) -> bool {
    let _trace = viewport_trace::section(c"Chatter maintain_history_window");
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
    // At the bottom — including the transient top layout while a conversation
    // is still opening on its newest message — the retained window must stay
    // anchored to the tail. Feeding the live (top) visible rows here would
    // evict the newest messages, set has_newer, drop is-at-bottom, and open the
    // chat scrolled up with a visible jump down to the tail. Only a reader who
    // has actually scrolled up (is-at-bottom false) trims the tail to free
    // memory; protected rows are still honored either way.
    let count = model.row_count();
    let (first_eff, last_eff) = if pane.get_is_at_bottom() {
        let tail = count.saturating_sub(1);
        (tail, tail)
    } else {
        (first.max(0) as usize, last as usize)
    };
    let range = chatter_core::ui::retained_history_range(
        count,
        first_eff,
        last_eff,
        &protected,
    );
    viewport_trace::counter(c"Chatter history rows before", count as i64);
    viewport_trace::counter(c"Chatter history retain begin", range.start as i64);
    viewport_trace::counter(c"Chatter history retain end", range.end as i64);
    if range.start == 0 && range.end == model.row_count() {
        return false;
    }
    let (front, back) = model.retain_range(range);
    viewport_trace::counter(c"Chatter history removed front", front as i64);
    viewport_trace::counter(c"Chatter history removed back", back as i64);
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

pub(super) fn wire_load_more(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
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

/// Older history requested by the list, or a jump target that is not loaded.
pub(super) enum HistoryLoad {
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
pub(super) fn load_history(w: &MainWindow, ui_ctx: &Arc<ChatterClient>, load: HistoryLoad) {
    // Bind this request before spawning; a later manual switch must never
    // reinterpret the target as belonging to the newly opened conversation.
    let conversation = ui_ctx.active_conversation();
    let Some(channel_id) = conversation.channel_id() else {
        return;
    };
    let jump = matches!(load, HistoryLoad::Reaching(_));
    let Some(owner) = crate::bin_ui::features::history_owner::HistoryOwner::admit_older(ui_ctx, conversation) else {
        if matches!(load, HistoryLoad::Newest) {
            let request = crate::bin_ui::features::history_owner::HistoryOwner::capture(ui_ctx, conversation);
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
        // Same core method as a conversation open, a different anchor: timer
        // marks, reply counts and timer-notice filtering come with the page.
        let anchor = match &load {
            HistoryLoad::Older(before) => PageAnchor::Older(*before),
            HistoryLoad::Newer(after, _) => PageAnchor::Newer(*after),
            HistoryLoad::Newest => PageAnchor::Newest,
            HistoryLoad::Reaching(target) => PageAnchor::Reaching(*target),
        };
        let generation = crate::bin_ui::decode_cache::thumbnail_generation();
        let page = ui_ctx.open_conversation(conversation, anchor, owner.scope());
        ui_ctx.set_message_loading(channel_id, false);
        let page = match page {
            Ok(page) => page,
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
        // Empty means the store returned nothing, timer notices included.
        if page.last_stored.is_none() {
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

        let fetched_cursor = page.last_stored.map(|id| id.to_string());
        if !owner.is_current() {
            return;
        }
        let display_messages = crate::rt_handle().block_on(
            crate::bin_ui::converters::prepare_rows(page.rows, false, generation),
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
