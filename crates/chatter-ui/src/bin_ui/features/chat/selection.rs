//! Message multi-select.
//!
//! Entered by the long hold on a bubble (tier 2 of the hold gesture) or by
//! "Select" in the message menu. While active, a plain tap toggles selection on
//! a bubble instead of opening it, the selection bar shows the count, and
//! Android Back exits the mode before it closes any modal (see `features::back`).
//!
//! The selected set lives here rather than in Slint because Slint has no set
//! primitive — membership would have to be a linear scan in an expression. It
//! deliberately does *not* live in `chatter-core`: it is transient view state
//! that dies with the view, not business logic. The moment a selection needs to
//! survive a conversation switch or a restart, it belongs in core instead.
//!
//! The per-row `selected` flag on `MessageItemData` is the only thing the UI
//! renders from, so this module owns keeping that flag in sync with the set.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::UserId;

use crate::MainWindow;

/// Rewrite the `selected` flag on every row so it matches `selected_ids`, and
/// publish the count. Only writes rows that actually changed — `set_row_data`
/// notifies the view, and rewriting all of them on every tap would repaint the
/// whole list.
fn sync_model(w: &MainWindow, selected_ids: &HashSet<String>) {
    let model = w.get_messages();
    for i in 0..model.row_count() {
        let Some(mut row) = model.row_data(i) else {
            continue;
        };
        let should_be = selected_ids.contains(row.message_id.as_str());
        if row.selected != should_be {
            row.selected = should_be;
            model.set_row_data(i, row);
        }
    }
    w.set_selected_message_count(selected_ids.len() as i32);
}

fn refresh_delete_everyone(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    selected_ids: &HashSet<String>,
) {
    let handle = ui_ctx.conversation(ui_ctx.chat_state().active_conversation());
    let model = w.get_messages();
    let resolved_count = model
        .iter()
        .filter(|row| selected_ids.contains(row.message_id.as_str()))
        .count();
    let all_ok = !selected_ids.is_empty()
        && resolved_count == selected_ids.len()
        && model
            .iter()
            .filter(|row| selected_ids.contains(row.message_id.as_str()))
            .all(|row| {
                let Ok(author) = row.sender_id.parse::<UserId>() else {
                    return false;
                };
                let Ok(ts) = row.timestamp_ns.parse() else {
                    return false;
                };
                handle.can_delete_for_everyone(&author, ts).unwrap_or(false)
            });
    w.global::<crate::SelectionChrome>()
        .set_can_delete_everyone(all_ok);
}

/// Leave selection mode and drop the marks.
fn exit(w: &MainWindow, selected_ids: &Rc<RefCell<HashSet<String>>>) {
    selected_ids.borrow_mut().clear();
    sync_model(w, &selected_ids.borrow());
    w.set_selection_mode(false);
}

/// Wire the multi-select callbacks.
pub fn wire_selection(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let selected_ids: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));

    // Toggle one message. Deselecting the last one leaves selection mode
    // entirely — a selection bar reading "0 selected" with a dead delete button
    // is a dead end the user has to think their way out of.
    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let ui_ctx = ui_ctx.clone();
        window.on_message_selection_toggled(move |message_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let id = message_id.to_string();
            if id.is_empty() {
                return;
            }
            {
                let mut set = selected_ids.borrow_mut();
                if !set.remove(&id) {
                    set.insert(id);
                }
            }
            if selected_ids.borrow().is_empty() {
                exit(&w, &selected_ids);
                w.global::<crate::SelectionChrome>()
                    .set_can_delete_everyone(false);
            } else {
                w.set_selection_mode(true);
                sync_model(&w, &selected_ids.borrow());
                refresh_delete_everyone(&w, &ui_ctx, &selected_ids.borrow());
            }
        });
    }

    // Explicit exit (the bar's close button). Android Back routes here too.
    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        window.on_selection_cleared(move || {
            let Some(w) = weak.upgrade() else { return };
            exit(&w, &selected_ids);
        });
    }

    // Bulk copy: the selected messages' text, in list (time) order, one
    // blank line apart. Media-only messages contribute nothing.
    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let ui_ctx = ui_ctx.clone();
        window.on_selection_copy_clicked(move || {
            let Some(w) = weak.upgrade() else { return };
            let targets: Vec<(String, String)> = {
                let set = selected_ids.borrow();
                let model = w.get_messages();
                model
                    .iter()
                    .filter(|row| set.contains(row.message_id.as_str()))
                    .map(|row| (row.sender_id.to_string(), row.timestamp_ns.to_string()))
                    .collect()
            };
            let handle = ui_ctx.conversation(ui_ctx.chat_state().active_conversation());
            std::thread::spawn(move || {
                let texts: Vec<String> = targets
                    .iter()
                    .filter_map(|(sender, ts)| {
                        let sender = sender.parse().ok()?;
                        let ts: u64 = ts.parse().ok()?;
                        handle.get_message_text(&sender, ts).ok().flatten()
                    })
                    .collect();
                if !texts.is_empty() {
                    crate::bin_ui::features::chat::actions::copy_text(&texts.join("\n\n"));
                }
            });
            exit(&w, &selected_ids);
        });
    }

    // Bulk forward: the selected messages, in list (time) order, through the
    // same picker and confirm path as a single forward.
    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let ui_ctx = ui_ctx.clone();
        window.on_selection_forward_clicked(move || {
            let Some(w) = weak.upgrade() else { return };
            let sources: Vec<String> = {
                let set = selected_ids.borrow();
                let model = w.get_messages();
                model
                    .iter()
                    .filter(|row| set.contains(row.message_id.as_str()))
                    .map(|row| format!("{}:{}", row.sender_id, row.timestamp_ns))
                    .collect()
            };
            if sources.is_empty() {
                return;
            }
            let preview = if sources.len() == 1 {
                "1 message".to_string()
            } else {
                format!("{} messages", sources.len())
            };
            exit(&w, &selected_ids);
            super::actions::open_forward_modal(&w, &ui_ctx, sources, preview);
        });
    }

    // Bulk delete-for-me. Reuses the existing single-message wire rather than
    // reimplementing the delete path, so permissions and the local/sync split
    // stay in exactly one place.
    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        window.on_selection_delete_clicked(move || {
            let Some(w) = weak.upgrade() else { return };
            // Resolve ids to (sender_id, timestamp_ns) up front: invoking the
            // delete callback mutates the model, so we must not be iterating it.
            let targets: Vec<(SharedString, SharedString)> = {
                let set = selected_ids.borrow();
                let model = w.get_messages();
                model
                    .iter()
                    .filter(|row| set.contains(row.message_id.as_str()))
                    .map(|row| (row.sender_id.clone(), row.timestamp_ns.clone()))
                    .collect()
            };
            tracing::info!(count = targets.len(), "UI callback: bulk delete-for-me");
            for (sender_id, timestamp_ns) in targets {
                w.invoke_delete_message(sender_id, timestamp_ns);
            }
            exit(&w, &selected_ids);
        });
    }

    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let ui_ctx = ui_ctx.clone();
        window
            .global::<crate::SelectionChrome>()
            .on_select_all(move || {
                let Some(w) = weak.upgrade() else { return };
                {
                    let mut set = selected_ids.borrow_mut();
                    set.clear();
                    let model = w.get_messages();
                    for row in model.iter() {
                        let id = row.message_id.to_string();
                        if !id.is_empty() && !row.is_deleted {
                            set.insert(id);
                        }
                    }
                }
                if selected_ids.borrow().is_empty() {
                    exit(&w, &selected_ids);
                    w.global::<crate::SelectionChrome>()
                        .set_can_delete_everyone(false);
                } else {
                    w.set_selection_mode(true);
                    sync_model(&w, &selected_ids.borrow());
                    refresh_delete_everyone(&w, &ui_ctx, &selected_ids.borrow());
                }
            });
    }

    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let ui_ctx = ui_ctx.clone();
        window
            .global::<crate::SelectionChrome>()
            .on_delete_everyone(move || {
                let Some(w) = weak.upgrade() else { return };
                let handle = ui_ctx.conversation(ui_ctx.chat_state().active_conversation());
                let targets: Vec<(SharedString, SharedString)> = {
                    let set = selected_ids.borrow();
                    let model = w.get_messages();
                    model
                        .iter()
                        .filter(|row| set.contains(row.message_id.as_str()))
                        .map(|row| (row.sender_id.clone(), row.timestamp_ns.clone()))
                        .collect()
                };
                // Recheck the entire selection at confirmation time. Never silently
                // delete only an eligible subset or substitute delete-for-me.
                if targets.is_empty() || targets.len() != selected_ids.borrow().len()
                    || !targets.iter().all(|(author, ts)| {
                        author.parse().ok().zip(ts.parse().ok()).is_some_and(|(author, ts)| {
                            handle.can_delete_for_everyone(&author, ts).unwrap_or(false)
                        })
                    })
                {
                    refresh_delete_everyone(&w, &ui_ctx, &selected_ids.borrow());
                    w.set_toast_message("These messages can no longer be deleted for both.".into());
                    w.set_toast_visible(true);
                    return;
                }
                tracing::info!(
                    count = targets.len(),
                    "UI callback: bulk delete-for-everyone"
                );
                for (sender_id, timestamp_ns) in targets {
                    w.invoke_delete_everyone_message(sender_id, timestamp_ns);
                }
                exit(&w, &selected_ids);
                w.global::<crate::SelectionChrome>()
                    .set_can_delete_everyone(false);
            });
    }
}
