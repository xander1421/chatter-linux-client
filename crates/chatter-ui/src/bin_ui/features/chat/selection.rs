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

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::services::conversation::{SelectedDeleteFailureReason, SelectedDeleteScope};

use crate::MainWindow;

/// Rewrite the `selected` flag on every row so it matches `selected_ids`, and
/// publish the count. Only writes rows that actually changed — `set_row_data`
/// notifies the view, and rewriting all of them on every tap would repaint the
/// whole list.
fn sync_model(w: &MainWindow, selected_ids: &HashSet<String>) {
    let model = w.global::<crate::ChatPane>().get_messages();
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
    w.global::<crate::ChatPane>()
        .set_selected_message_count(selected_ids.len() as i32);
}

fn refresh_delete_everyone(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    selected_ids: &Rc<RefCell<HashSet<String>>>,
    generation: &Rc<Cell<u64>>,
) {
    let ids = selected_ids_in_order(w, &selected_ids.borrow());
    let revision = generation.get();
    let conversation = ui_ctx.active_conversation();
    let auth = ui_ctx.auth_snapshot();
    let chrome = w.global::<crate::SelectionChrome>();
    chrome.set_can_delete_everyone(false);
    chrome.set_can_forward(false);
    if ids.is_empty() || ids.len() != selected_ids.borrow().len() {
        return;
    }
    let weak = w.as_weak();
    let ui_ctx = ui_ctx.clone();
    let selected_ids = selected_ids.clone();
    let generation = generation.clone();
    if let Err(error) = slint::spawn_local(async move {
        let snapshot = ui_ctx.selection_snapshot(conversation, ids.clone()).await;
        let Some(w) = weak.upgrade() else { return };
        let current_auth = ui_ctx.auth_snapshot();
        if generation.get() != revision
            || ui_ctx.active_conversation() != conversation
            || current_auth.current_user_id != auth.current_user_id
            || current_auth.session_revision != auth.session_revision
            || selected_ids.borrow().len() != ids.len()
            || !ids.iter().all(|id| selected_ids.borrow().contains(id))
        {
            return;
        }
        if let Some(snapshot) = snapshot {
            let chrome = w.global::<crate::SelectionChrome>();
            chrome.set_can_delete_everyone(snapshot.actions.can_delete_everyone);
            chrome.set_can_forward(snapshot.actions.can_forward);
        }
    }) {
        tracing::error!(%error, "Could not schedule selection snapshot");
    }
}

/// Leave selection mode and drop the marks.
fn exit(w: &MainWindow, selected_ids: &Rc<RefCell<HashSet<String>>>, generation: &Cell<u64>) {
    generation.set(generation.get().wrapping_add(1));
    selected_ids.borrow_mut().clear();
    sync_model(w, &selected_ids.borrow());
    w.global::<crate::ChatPane>().set_selection_mode(false);
    w.global::<crate::SelectionChrome>()
        .set_can_delete_everyone(false);
    w.global::<crate::SelectionChrome>().set_can_forward(false);
}

/// Snapshot the complete selection before any asynchronous deletion begins.
fn selected_ids_in_order(w: &MainWindow, ids: &HashSet<String>) -> Vec<String> {
    w.global::<crate::ChatPane>()
        .get_messages()
        .iter()
        .filter(|row| ids.contains(row.message_id.as_str()))
        .map(|row| row.message_id.to_string())
        .collect()
}

fn delete_selection(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    selected_ids: &Rc<RefCell<HashSet<String>>>,
    pending: &Rc<Cell<bool>>,
    generation: &Rc<Cell<u64>>,
    everyone: bool,
) {
    if pending.get() {
        return;
    }
    let ordered_ids = selected_ids_in_order(w, &selected_ids.borrow());
    let conversation = ui_ctx.active_conversation();
    // Preserve a missing selected row as an unavailable set. Core resolves
    // the complete selection against storage before making any changes.
    if ordered_ids.is_empty() || ordered_ids.len() != selected_ids.borrow().len() {
        refresh_delete_everyone(w, ui_ctx, selected_ids, generation);
        w.set_toast_message("The selected messages are no longer available.".into());
        w.set_toast_visible(true);
        return;
    }
    let submitted_ids = selected_ids.borrow().clone();
    let selection_revision = generation.get();
    let generation = generation.clone();
    let auth = ui_ctx.auth_snapshot();
    let Some(account) = auth.current_user_id else {
        return;
    };
    let session_revision = auth.session_revision;
    let selected_ids = selected_ids.clone();
    let pending_completion = pending.clone();
    let weak = w.as_weak();
    let ui_ctx = ui_ctx.clone();
    pending.set(true);
    // Keep the UI-owned selection on the event loop; network/storage work runs
    // on Tokio. Register completion before starting any destructive work.
    if let Err(error) = slint::spawn_local(async move {
        let snapshot = ui_ctx.selection_snapshot(conversation, ordered_ids.clone()).await;
        let current_auth = ui_ctx.auth_snapshot();
        if current_auth.current_user_id != Some(account)
            || current_auth.session_revision != session_revision
            || ui_ctx.active_conversation() != conversation
            || generation.get() != selection_revision
            || *selected_ids.borrow() != submitted_ids
        {
            pending_completion.set(false);
            return;
        }
        if snapshot.is_none() || (everyone && !snapshot.unwrap().actions.can_delete_everyone) {
            pending_completion.set(false);
            if let Some(w) = weak.upgrade() {
                w.set_toast_message("The selected messages are no longer available for deletion.".into());
                w.set_toast_visible(true);
                refresh_delete_everyone(&w, &ui_ctx, &selected_ids, &generation);
            }
            return;
        }
        let (completed, mut completions) = tokio::sync::mpsc::unbounded_channel();
        let task_client = ui_ctx.clone();
        let task = tokio::spawn(async move {
            task_client
                .delete_selected_messages(
                    conversation,
                    &ordered_ids,
                    if everyone {
                        SelectedDeleteScope::ForEveryone
                    } else {
                        SelectedDeleteScope::ForMe
                    },
                    account,
                    session_revision,
                    completed,
                )
                .await
        });
        let mut expected_generation = selection_revision;
        let mut expected_ids = submitted_ids;
        let mut selection_current = true;
        while let Some(completed_targets) = completions.recv().await {
            let auth = ui_ctx.auth_snapshot();
            if auth.current_user_id != Some(account) || auth.session_revision != session_revision {
                selection_current = false;
                continue;
            }
            // Every confirmed persistence change affects the sidebar, even if
            // another target later fails or the user has left this selection.
            crate::bin_ui::features::sidebar::history_changed(&weak, &ui_ctx);
            let Some(w) = weak.upgrade() else { continue };
            selection_current &= ui_ctx.active_conversation() == conversation
                && generation.get() == expected_generation
                && *selected_ids.borrow() == expected_ids;
            if !selection_current {
                continue;
            }
            super::delete::remove_selected_messages_from_ui(&w, &completed_targets);
            // The shared row-removal callback reconciles exact selected IDs
            // and advances the generation. Accept only our own synchronous
            // change; intervening user/background changes still fence us out.
            expected_generation = generation.get();
            expected_ids = selected_ids.borrow().clone();
        }
        let result = task.await;
        pending_completion.set(false);
        let Some(w) = weak.upgrade() else { return };
        let auth = ui_ctx.auth_snapshot();
        if auth.current_user_id != Some(account) || auth.session_revision != session_revision {
            return;
        }
        if !selection_current
            || ui_ctx.active_conversation() != conversation
            || generation.get() != expected_generation
            || *selected_ids.borrow() != expected_ids
        {
            return;
        }
        let failure = match result {
            Ok(result) => result
                .failed_ids
                .into_iter()
                .next()
                .map(|failure| failure.reason),
            Err(error) => Some(SelectedDeleteFailureReason::Operation(
                chatter_core::services::conversation::ConversationError::Other(error.to_string()),
            )),
        };
        if let Some(failure) = failure {
            let error = failure.to_string();
            tracing::warn!(%error, everyone, "Selected-message deletion failed");
            let message = match failure {
                SelectedDeleteFailureReason::Unavailable
                | SelectedDeleteFailureReason::NotEligible => error,
                _ => format!("Could not finish deleting selected messages: {error}"),
            };
            w.set_toast_message(message.into());
            w.set_toast_visible(true);
            refresh_delete_everyone(&w, &ui_ctx, &selected_ids, &generation);
        }
    }) {
        pending.set(false);
        tracing::error!(%error, "Could not schedule selected-message deletion");
        w.set_toast_message("Could not start deleting selected messages.".into());
        w.set_toast_visible(true);
    }
}

/// Wire the multi-select callbacks.
pub fn wire_selection(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let selected_ids: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));

    let delete_pending = Rc::new(Cell::new(false));
    let generation = Rc::new(Cell::new(0_u64));

    // Background recovery and incoming deletes use the same row-removal
    // helper as manual deletion. Reconcile its exact removed IDs on the UI
    // thread, never infer deletion from a partial/replaced message model.
    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let generation = generation.clone();
        let ui_ctx = ui_ctx.clone();
        window
            .global::<crate::SelectionChrome>()
            .on_messages_removed(move |removed| {
                let Some(w) = weak.upgrade() else { return };
                let changed = {
                    let mut set = selected_ids.borrow_mut();
                    let before = set.len();
                    for id in removed.iter() {
                        set.remove(id.as_str());
                    }
                    set.len() != before
                };
                if !changed {
                    return;
                }
                // Invalidate a pending manual completion's selection snapshot.
                generation.set(generation.get().wrapping_add(1));
                if selected_ids.borrow().is_empty() {
                    exit(&w, &selected_ids, &generation);
                } else {
                    sync_model(&w, &selected_ids.borrow());
                    refresh_delete_everyone(&w, &ui_ctx, &selected_ids, &generation);
                }
            });
    }

    // Toggle one message. Deselecting the last one leaves selection mode
    // entirely — a selection bar reading "0 selected" with a dead delete button
    // is a dead end the user has to think their way out of.
    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let generation = generation.clone();
        let ui_ctx = ui_ctx.clone();
        window
            .global::<crate::ChatPane>()
            .on_message_selection_toggled(move |message_id: SharedString| {
                let Some(w) = weak.upgrade() else { return };
                let id = message_id.to_string();
                if id.is_empty() {
                    return;
                }
                generation.set(generation.get().wrapping_add(1));
                {
                    let mut set = selected_ids.borrow_mut();
                    if !set.remove(&id) {
                        set.insert(id);
                    }
                }
                if selected_ids.borrow().is_empty() {
                    exit(&w, &selected_ids, &generation);
                    w.global::<crate::SelectionChrome>()
                        .set_can_delete_everyone(false);
                } else {
                    w.global::<crate::ChatPane>().set_selection_mode(true);
                    sync_model(&w, &selected_ids.borrow());
                    refresh_delete_everyone(&w, &ui_ctx, &selected_ids, &generation);
                }
            });
    }

    // Explicit exit (the bar's close button). Android Back routes here too.
    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let generation = generation.clone();
        window
            .global::<crate::ChatPane>()
            .on_selection_cleared(move || {
                let Some(w) = weak.upgrade() else { return };
                exit(&w, &selected_ids, &generation);
            });
    }

    // Bulk copy: the selected messages' text, in list (time) order, one
    // blank line apart. Media-only messages contribute nothing.
    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let generation = generation.clone();
        let ui_ctx = ui_ctx.clone();
        window
            .global::<crate::ChatPane>()
            .on_selection_copy_clicked(move || {
                let Some(w) = weak.upgrade() else { return };
                let ordered_ids = selected_ids_in_order(&w, &selected_ids.borrow());
                if ordered_ids.is_empty() || ordered_ids.len() != selected_ids.borrow().len() {
                    return;
                }
                let conversation = ui_ctx.active_conversation();
                let revision = generation.get();
                let auth = ui_ctx.auth_snapshot();
                let weak = w.as_weak();
                let selected_ids = selected_ids.clone();
                let generation = generation.clone();
                let ui_ctx = ui_ctx.clone();
                if let Err(error) = slint::spawn_local(async move {
                    let snapshot = ui_ctx.selection_snapshot(conversation, ordered_ids.clone()).await;
                    let Some(w) = weak.upgrade() else { return };
                    let current_auth = ui_ctx.auth_snapshot();
                    if generation.get() != revision
                        || ui_ctx.active_conversation() != conversation
                        || current_auth.current_user_id != auth.current_user_id
                        || current_auth.session_revision != auth.session_revision
                        || selected_ids.borrow().len() != ordered_ids.len()
                        || !ordered_ids.iter().all(|id| selected_ids.borrow().contains(id))
                    {
                        return;
                    }
                    let Some(snapshot) = snapshot else { return };
                    if !snapshot.copy_text.is_empty() {
                        std::thread::spawn(move || super::actions::copy_text(&snapshot.copy_text));
                        // Same confirmation as a single message's Copy.
                        w.set_toast_message("Copied".into());
                        w.set_toast_variant("success".into());
                        w.set_toast_visible(true);
                    }
                    exit(&w, &selected_ids, &generation);
                }) {
                    tracing::error!(%error, "Could not schedule selection copy");
                }
            });
    }

    // Bulk forward: the selected messages, in list (time) order, through the
    // same picker and confirm path as a single forward.
    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let generation = generation.clone();
        let ui_ctx = ui_ctx.clone();
        window
            .global::<crate::ChatPane>()
            .on_selection_forward_clicked(move || {
                let Some(w) = weak.upgrade() else { return };
                let ordered_ids = selected_ids_in_order(&w, &selected_ids.borrow());
                if ordered_ids.is_empty() || ordered_ids.len() != selected_ids.borrow().len() {
                    return;
                }
                let conversation = ui_ctx.active_conversation();
                let revision = generation.get();
                let auth = ui_ctx.auth_snapshot();
                let weak = w.as_weak();
                let selected_ids = selected_ids.clone();
                let generation = generation.clone();
                let ui_ctx = ui_ctx.clone();
                if let Err(error) = slint::spawn_local(async move {
                    let snapshot = ui_ctx.selection_snapshot(conversation, ordered_ids.clone()).await;
                    let Some(w) = weak.upgrade() else { return };
                    let current_auth = ui_ctx.auth_snapshot();
                    if generation.get() != revision
                        || ui_ctx.active_conversation() != conversation
                        || current_auth.current_user_id != auth.current_user_id
                        || current_auth.session_revision != auth.session_revision
                        || selected_ids.borrow().len() != ordered_ids.len()
                        || !ordered_ids.iter().all(|id| selected_ids.borrow().contains(id))
                    {
                        return;
                    }
                    let Some(snapshot) = snapshot else { return };
                    if !snapshot.actions.can_forward {
                        return;
                    }
                    let sources: Vec<String> = snapshot.targets.iter()
                        .map(|(sender, ts)| format!("{sender}:{ts}"))
                        .collect();
                    let preview = if sources.len() == 1 {
                        "1 message".to_string()
                    } else {
                        format!("{} messages", sources.len())
                    };
                    exit(&w, &selected_ids, &generation);
                    super::actions::open_forward_modal(&w, &ui_ctx, sources, preview);
                }) {
                    tracing::error!(%error, "Could not schedule selection forward");
                }
            });
    }

    // Delete-for-me drops each confirmed row while retaining failures for retry.
    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let generation = generation.clone();
        let ui_ctx = ui_ctx.clone();
        let pending = delete_pending.clone();
        window
            .global::<crate::ChatPane>()
            .on_selection_delete_clicked(move || {
                let Some(w) = weak.upgrade() else { return };
                delete_selection(&w, &ui_ctx, &selected_ids, &pending, &generation, false);
            });
    }

    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let generation = generation.clone();
        let ui_ctx = ui_ctx.clone();
        window
            .global::<crate::SelectionChrome>()
            .on_select_all(move || {
                let Some(w) = weak.upgrade() else { return };
                generation.set(generation.get().wrapping_add(1));
                {
                    let mut set = selected_ids.borrow_mut();
                    set.clear();
                    let model = w.global::<crate::ChatPane>().get_messages();
                    for row in model.iter() {
                        let id = row.message_id.to_string();
                        if !id.is_empty() && !row.is_deleted {
                            set.insert(id);
                        }
                    }
                }
                if selected_ids.borrow().is_empty() {
                    exit(&w, &selected_ids, &generation);
                    w.global::<crate::SelectionChrome>()
                        .set_can_delete_everyone(false);
                } else {
                    w.global::<crate::ChatPane>().set_selection_mode(true);
                    sync_model(&w, &selected_ids.borrow());
                    refresh_delete_everyone(&w, &ui_ctx, &selected_ids, &generation);
                }
            });
    }

    {
        let weak = window.as_weak();
        let selected_ids = selected_ids.clone();
        let generation = generation.clone();
        let ui_ctx = ui_ctx.clone();
        let pending = delete_pending.clone();
        window
            .global::<crate::SelectionChrome>()
            .on_delete_everyone(move || {
                let Some(w) = weak.upgrade() else { return };
                delete_selection(&w, &ui_ctx, &selected_ids, &pending, &generation, true);
            });
    }
}
