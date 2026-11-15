//! Opening a conversation: the channel, DM, private-group and Saved
//! Messages open paths, and the chat-pane state they share.

pub mod channel;
pub mod dm;
pub mod private_group;
mod saved_messages;

use std::sync::Arc;

use slint::ComponentHandle;

use chatter_core::client::ChatterClient;

use crate::MainWindow;
use crate::bin_ui::audio_state;

/// Wire the conversation-open callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    channel::wire_channel_selected(window, ui_ctx);
    dm::wire_dm_selected(window, ui_ctx);
    dm::wire_delete_conversation(window, ui_ctx);
    dm::wire_secret_request_answer(window, ui_ctx);
    dm::wire_start_secret_chat(window, ui_ctx);
    private_group::wire_private_group_selected(window, ui_ctx);
    saved_messages::wire_saved_messages(window, ui_ctx);
}

/// Tell the message list where to open.
///
/// Core picked the row (`ChatterClient::open_conversation`, from the anchor
/// `mark_as_read` captured before the mark destroyed it); this only hands the
/// number to the list.
///
/// `is_at_bottom` is reset here on purpose: it is sticky across conversations,
/// so without this an open lands wherever the previous conversation's scroll
/// left it.
pub(crate) fn apply_unread_anchor(
    window: &MainWindow,
    page: &chatter_core::client::ConversationPage,
) {
    let pane = window.global::<crate::ChatPane>();
    pane.set_unread_index(page.unread_index.map_or(-1, |i| i as i32));
    pane.set_unread_count(page.unread_count as i32);
    pane.set_is_at_bottom(page.unread_index.is_none());
}

/// Fire the sidebar-open callback for `conv` — the same entry point a row tap
/// uses, so the message list is (re)loaded from storage. False for the
/// variant no open callback exists for (`None`).
pub(crate) fn select_conversation(
    window: &MainWindow,
    conv: chatter_core::state::ActiveConversation,
) -> bool {
    use chatter_core::state::ActiveConversation;
    crate::bin_ui::features::message_list::jump::cancel_pending_jump_for(conv);
    match conv {
        ActiveConversation::Channel(id) => window
            .global::<crate::SidebarPane>()
            .invoke_channel_selected(id.to_string().into()),
        ActiveConversation::ServerDm { peer_id } => window
            .global::<crate::SidebarPane>()
            .invoke_dm_selected(peer_id.to_string().into()),
        ActiveConversation::AnonDm(id) => window
            .global::<crate::SidebarPane>()
            .invoke_dm_selected(id.to_string().into()),
        ActiveConversation::AnonGroup(id) => window
            .global::<crate::SidebarPane>()
            .invoke_private_group_selected(id.to_string().into()),
        ActiveConversation::None => return false,
    }
    true
}

/// Apply pinned message state to the UI.
///
/// Shared helper used by channel, DM, and private group sidebar handlers
/// to update the pinned message bar after loading pinned messages.
/// Pins belong to the same history request as the model/header they accompany.
pub(crate) fn apply_pinned_ui_owned(
    weak: &slint::Weak<MainWindow>,
    client: &Arc<ChatterClient>,
    ids: Vec<chatter_core::domain::ids::MessageId>,
    preview: String,
    owner: &crate::bin_ui::features::history_owner::HistoryOwner,
) {
    let client = client.clone();
    crate::run_history_ui!(weak, owner, |w| {
        if ids.is_empty() {
            client.clear_pinned();
            w.global::<crate::ChatPane>().set_has_pinned_messages(false);
            w.global::<crate::ChatPane>()
                .set_pinned_message_preview("".into());
            w.global::<crate::ChatPane>().set_pinned_total_count(0);
        } else {
            let count = ids.len() as i32;
            client.set_pinned_messages(ids);
            w.global::<crate::ChatPane>()
                .set_has_pinned_messages(client.pinned_should_show());
            w.global::<crate::ChatPane>()
                .set_pinned_message_preview(preview.into());
            w.global::<crate::ChatPane>().set_pinned_current_index(1);
            w.global::<crate::ChatPane>().set_pinned_total_count(count);
        }
    });
}

/// Close whatever conversation is open: the pane shows "Select a
/// conversation" on desktop, and a phone returns to the list. UI thread.
/// Pushed screens are left alone: their owners close them separately.
pub(crate) fn close_open_conversation(w: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // The thread panel hangs off the conversation; it stayed open beside the
    // empty pane. Keep its draft, as its own close button does.
    if w.global::<crate::ChatPane>().get_thread_panel_visible() {
        crate::bin_ui::features::composer::autosave_thread_draft(w, ui_ctx);
        w.global::<crate::ChatPane>()
            .set_thread_root_message_id("".into());
        w.global::<crate::ChatPane>()
            .set_thread_composer_text("".into());
        w.global::<crate::ChatPane>().set_thread_panel_visible(false);
        w.global::<crate::ChatPane>().set_thread_pinned_mode(false);
        w.global::<crate::ChatPane>()
            .set_thread_replies(slint::ModelRc::new(slint::VecModel::default()));
    }
    ui_ctx.select_conversation(chatter_core::state::ActiveConversation::None);
    w.invoke_refresh_screen_protection();
    w.global::<crate::ChatPane>()
        .set_messages(slint::ModelRc::new(
            crate::bin_ui::message_model::MessageModel::default(),
        ));
    // No name = nothing open: the pane shows "Select a conversation".
    w.global::<crate::ChatPane>().set_channel_name("".into());
    w.global::<crate::ChatPane>().set_header_can_call(false);
    w.global::<crate::ChatPane>().set_header_can_invite(false);
    w.global::<crate::ChatPane>()
        .set_header_can_open_settings(false);
    w.global::<crate::ChatPane>()
        .set_header_can_open_pinned_list(false);
    // The empty detail pane is useful on desktop; on a phone the closed
    // conversation must reveal the list without another Back.
    if w.get_is_mobile_platform() {
        w.global::<crate::SidebarPane>().set_mobile_show_chat(false);
        w.global::<crate::SidebarPane>()
            .set_active_tab("chats".into());
    }
}

/// Opening a conversation shows it. On a phone the chat pane is a separate
/// screen, and only the sidebar's own row taps used to raise it: every other
/// opener (Friends and Profile "Message", search, call history, sections)
/// selected the conversation behind the list and looked like it did nothing.
/// Runs on the UI thread, from the selection callbacks.
pub(crate) fn show_chat_pane(weak: &slint::Weak<MainWindow>) {
    if let Some(w) = weak.upgrade() {
        // A conversation lives on the Chats tab: opened from Contacts, Calls
        // or a pushed profile, it must not stay hidden behind them.
        crate::bin_ui::features::back::reset_to_chats(&w);
        // Picker visibility belongs to the composer being left. Reset at the
        // synchronous entry point as well as on phone Back: a two-pane switch
        // does not change mobile-show-chat, and the chat child is recreated.
        w.global::<crate::ChatPane>()
            .set_composer_emoji_picker_visible(false);
        w.global::<crate::ChatPane>()
            .set_composer_gif_picker_visible(false);
        // Clear the old header synchronously, before revealing the pane or
        // waiting for the next conversation's asynchronous load.
        w.global::<crate::ChatPane>().set_loading_messages(true);
        w.global::<crate::ChatPane>().set_channel_id("".into());
        w.global::<crate::ChatPane>().set_header_last_seen("".into());
        w.global::<crate::ChatPane>().set_channel_name("".into());
        w.global::<crate::ChatPane>().set_header_can_call(false);
        w.global::<crate::ChatPane>().set_header_can_invite(false);
        w.global::<crate::ChatPane>()
            .set_header_can_open_settings(false);
        w.global::<crate::ChatPane>()
            .set_header_can_open_pinned_list(false);
        w.global::<crate::ChatPane>().set_topic("".into());
        w.global::<crate::ChatPane>().set_member_count(0);
        w.global::<crate::ChatPane>()
            .set_header_presence_status("offline".into());
        w.global::<crate::ChatPane>()
            .set_channel_avatar_source(slint::Image::default());
        w.global::<crate::ChatPane>().set_channel_has_avatar(false);
        // The header's typing line belongs to the conversation being left.
        w.global::<crate::ChatPane>()
            .set_typing_indicator("".into());
        if w.get_is_mobile_platform() {
            w.global::<crate::SidebarPane>().set_mobile_show_chat(true);
        }
    }
}

/// Stop any currently playing audio and clear state.
pub(crate) fn stop_audio_playback() {
    // Stop the audio player
    if let Ok(mut guard) = audio_state::get_audio_player().lock() {
        if let Some(ref mut player) = *guard {
            player.stop();
        }
    }
    // Clear the audio state
    audio_state::clear_state();
}
