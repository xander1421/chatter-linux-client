//! Jump-to-message: scroll to and flash a row, loading history back to it,
//! and jumps queued until another conversation has opened.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::MessageId;
use chatter_core::state::ActiveConversation;

use crate::MainWindow;
use super::history::{HistoryLoad, load_history};

thread_local! {
    static PENDING_JUMP: std::cell::RefCell<Option<(ActiveConversation, String)>> = const { std::cell::RefCell::new(None) };
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
    crate::bin_ui::features::chat::open::select_conversation(w, conversation);
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
pub(super) fn report_jump_unavailable_later(
    weak: slint::Weak<MainWindow>,
    owner: crate::bin_ui::features::history_owner::HistoryOwner,
) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            if owner.is_current() {
                report_jump_unavailable(&w);
            }
        }
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
pub(super) fn install_and_jump(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    owner: crate::bin_ui::features::history_owner::HistoryOwner,
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
