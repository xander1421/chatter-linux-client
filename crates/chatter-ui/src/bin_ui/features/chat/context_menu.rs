//! Context menu callback for messages.

use std::sync::Arc;

use slint::{ComponentHandle, Model, SharedString};

use chatter_core::client::ChatterClient;
use chatter_core::domain::UserId;
use chatter_core::domain::message::MessageMenu;
use chatter_core::state::ActiveConversation;

use crate::MainWindow;

/// Wire more-clicked callback to show context menu.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_more_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_message_info(window, ui_ctx);
    let ui_ctx = Arc::clone(ui_ctx);
    let weak_window = window.as_weak();

    window.global::<crate::ChatPane>().on_more_clicked(move |sender_id: SharedString, timestamp_ns: SharedString, click_x: f32, click_y: f32| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let ui_ctx = Arc::clone(&ui_ctx);
        let weak = weak_window.clone();
        let menu_x = click_x;
        let menu_y = click_y;

        let conversation = ui_ctx.active_conversation();
        if matches!(conversation, ActiveConversation::None) {
            return;
        }
        // The menu is for this session and conversation: a lookup that
        // finishes after a sign-out or another conversation opened must not
        // publish a menu (whose Edit would then key the new conversation
        // with the old author/clock).
        let owner =
            crate::bin_ui::features::history_owner::HistoryOwner::capture(&ui_ctx, conversation);

        let handle = ui_ctx.conversation(conversation);

        tokio::spawn(async move {
            if !owner.is_current() {
                return;
            }
            let target_sender_id: UserId = match sender_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for context menu");
                    return;
                }
            };
            let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                Ok(ts) => ts,
                Err(e) => {
                    tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for context menu");
                    return;
                }
            };

            let menu = match handle.message_menu(&target_sender_id, target_sent_timestamp) {
                Ok(menu) => menu,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to look up message for context menu");
                    return;
                }
            };

            crate::run_history_ui!(weak, owner, |w| {
                set_context_menu(&w, sender_id_str, timestamp_ns_str, menu, menu_x, menu_y);
            });
        });
    });
}

/// Resolve the message id for a (sender_id, timestamp_ns) pair from the UI
/// model. The menu is keyed Signal-style, but "Reply in Thread" and "Select"
/// are keyed by message id, so the menu has to carry both.
fn find_message_id(w: &MainWindow, sender_id: &str, timestamp_ns: &str) -> String {
    let model = w.global::<crate::ChatPane>().get_messages();
    for i in 0..model.row_count() {
        if let Some(m) = model.row_data(i) {
            if m.sender_id.as_str() == sender_id && m.timestamp_ns.as_str() == timestamp_ns {
                return m.message_id.to_string();
            }
        }
    }
    String::new()
}

/// Set all context menu properties on the window.
///
/// Flags come from [`MessageMenu`] and are written before the menu is shown.
/// The window bools stay in sync for anything that still reads them; the menu
/// itself renders `MessageMenuActions`.
fn set_context_menu(
    w: &MainWindow,
    sender_id: String,
    timestamp_ns: String,
    menu: MessageMenu,
    menu_x: f32,
    menu_y: f32,
) {
    let actions = menu.actions;
    let flags = w.global::<crate::MessageMenuActions>();
    flags.set_resend(actions.resend);
    flags.set_react(actions.react);
    flags.set_reply(actions.reply);
    flags.set_edit(actions.edit);
    flags.set_pin(actions.pin);
    flags.set_pinned(menu.pinned);
    flags.set_delete_for_everyone(actions.delete_for_everyone);
    flags.set_delete_for_me(actions.delete_for_me);
    flags.set_forward(actions.forward);
    flags.set_copy(actions.copy);
    flags.set_download(actions.download);
    flags.set_info(actions.info);
    flags.set_select(actions.select);

    w.global::<crate::ChatPane>()
        .set_context_menu_message_id(find_message_id(w, &sender_id, &timestamp_ns).into());
    w.global::<crate::ChatPane>()
        .set_context_menu_sender_id(sender_id.into());
    w.global::<crate::ChatPane>()
        .set_context_menu_timestamp_ns(timestamp_ns.into());
    w.global::<crate::ChatPane>().set_context_menu_x(menu_x);
    w.global::<crate::ChatPane>().set_context_menu_y(menu_y);
    w.global::<crate::ChatPane>().set_context_menu_visible(true);
}

fn wire_message_info(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window.global::<crate::MessageInfo>().on_open(move |id| {
        let Ok(message_id) = id.parse() else { return };
        let details = client
            .message_receipt_info(message_id)
            .unwrap_or_else(|error| format!("Could not load delivery details: {error}"));
        if let Some(window) = weak.upgrade() {
            let info = window.global::<crate::MessageInfo>();
            info.set_message_id(id);
            info.set_details(details.into());
            super::super::back::push(&window, "message-info");
        }
    });
}
