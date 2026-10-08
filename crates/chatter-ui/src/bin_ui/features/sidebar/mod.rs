//! Sidebar callbacks.
//!
//! Handles channel/DM selection, settings, and profile.

mod anon_mode;
pub mod channel;
pub mod dm;
mod navigation;
pub mod private_group;
pub mod sections;

use std::sync::Arc;

use chatter_core::client::ChatterClient;

use crate::bin_ui::audio_state;
use crate::{MainWindow, run_ui};

// Re-export public functions
pub use channel::load_channel_into_ui;

/// Wire sidebar callbacks (channel/DM selection, settings, profile).
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    channel::wire_channel_selected(window, ui_ctx);
    dm::wire_dm_selected(window, ui_ctx);
    dm::wire_delete_conversation(window, ui_ctx);

    // On a phone, leaving the chat pane left its conversation "active": the
    // next message there was marked read (and receipted) unseen, and its row
    // got no unread badge. Clear it the moment the pane closes.
    let (weak, ctx) = (slint::ComponentHandle::as_weak(window), ui_ctx.clone());
    window.on_chat_pane_closed(move || {
        ctx.chat_state()
            .set_active_conversation(chatter_core::state::ActiveConversation::None);
        if let Some(w) = weak.upgrade() {
            refresh_selection(&w, &ctx);
        }
    });
    private_group::wire_private_group_selected(window, ui_ctx);
    navigation::wire_saved_messages(window, ui_ctx);
    navigation::wire_settings_clicked(window, ui_ctx);
    navigation::wire_profile_clicked(window, ui_ctx);
    navigation::wire_create_join_channel(window);
    navigation::wire_toggle_sidebar(window, ui_ctx);
    navigation::load_sidebar_collapsed_state(window, ui_ctx);
    navigation::wire_message_requests_clicked(window, ui_ctx);
    navigation::load_pending_message_requests_count(window, ui_ctx);
    navigation::wire_group_invites_clicked(window, ui_ctx);
    navigation::load_pending_group_invites_count(window, ui_ctx);
    sections::wire_section_callbacks(window, ui_ctx);
    sections::load_sections(window, ui_ctx);
    anon_mode::wire(window, ui_ctx);
}

/// Tell the message list where to open.
///
/// The anchor comes from `ConversationHandle::mark_as_read`, which captured it
/// on its way past — by the time the page below is loaded, every row is read
/// and the boundary no longer exists in the data. Core decides *which* row
/// (`UnreadAnchor::divider_index`); this only hands the number to the list.
///
/// `is_at_bottom` is reset here on purpose: it is sticky across conversations,
/// so without this an open lands wherever the previous conversation's scroll
/// left it.
pub(crate) fn apply_unread_anchor(
    window: &MainWindow,
    anchor: chatter_core::domain::message::UnreadAnchor,
    page: &[chatter_core::domain::message::Message],
) {
    let index = anchor.divider_index(page);
    window.set_unread_index(index.map_or(-1, |i| i as i32));
    window.set_unread_count(anchor.unread_count as i32);
    window.set_is_at_bottom(index.is_none());
}

/// Fire the sidebar-open callback for `conv` — the same entry point a row tap
/// uses, so the message list is (re)loaded from storage. False for the
/// variant no open callback exists for (`None`).
pub(crate) fn select_conversation(
    window: &MainWindow,
    conv: chatter_core::state::ActiveConversation,
) -> bool {
    use chatter_core::state::ActiveConversation;
    super::header::cancel_pending_jump_for(conv);
    match conv {
        ActiveConversation::Channel(id) => window.invoke_channel_selected(id.to_string().into()),
        ActiveConversation::ServerDm { peer_id } => {
            window.invoke_dm_selected(peer_id.to_string().into())
        }
        ActiveConversation::AnonDm(id) => window.invoke_dm_selected(id.to_string().into()),
        ActiveConversation::AnonGroup(id) => {
            window.invoke_private_group_selected(id.to_string().into())
        }
        ActiveConversation::None => return false,
    }
    true
}

/// Recompute `is_selected` on every sidebar model item from the typed
/// `ActiveConversation` held by the client. Call after any selection change
/// so highlighting stays in sync without stringly-typed window properties.
///
/// Match logic is delegated to `ActiveConversation::matches_*` in `chatter-core`;
/// this function only iterates Slint models and writes the flag.
pub(crate) fn refresh_selection(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    use slint::Model;

    let active = ui_ctx.active_conversation();

    // A server-backed conversation was selected, so whatever mesh conversation
    // was open no longer is. `ActiveConversation` has no mesh arm — mesh rows
    // are keyed by a transport scope the server never hears about — so the two
    // selections are cleared against each other here, in the one place every
    // selection path already funnels through.
    #[cfg(feature = "mesh")]
    if !matches!(active, chatter_core::state::ActiveConversation::None) {
        crate::bin_ui::features::mesh::clear_active(window);
    }

    // The chat list: one model, every kind. Rows are edited in place, so no
    // model swap and no scroll reset.
    {
        let model = window.get_chat_rows();
        for i in 0..model.row_count() {
            if let Some(mut item) = model.row_data(i) {
                let selected = row_is_active(
                    &active,
                    item.conversation_type,
                    item.conversation_id.as_str(),
                );
                if item.is_selected != selected {
                    item.is_selected = selected;
                    model.set_row_data(i, item);
                }
            }
        }
    }

    // Custom-section conversations mirror the above across all kinds.
    refresh_section_selection(window, ui_ctx);
}

/// Re-mark the open conversation inside the folder models.
///
/// Rows now hang off their own section (pre-grouped in Rust), so this walks a
/// model of models rather than one flat list. Split out because rebuilding the
/// folders always clears the flag, and the rebuild happens on paths that must
/// not pay for the whole `refresh_selection` sweep.
pub(crate) fn refresh_section_selection(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    use slint::Model;

    let active = ui_ctx.active_conversation();
    let sections = window.get_custom_sections();
    let updated: Vec<_> = sections
        .iter()
        .map(|mut section| {
            let rows = section.conversations.clone();
            let marked: Vec<_> = (0..rows.row_count())
                .filter_map(|j| rows.row_data(j))
                .map(|mut item| {
                    item.is_selected = row_is_active(
                        &active,
                        item.conversation_type,
                        item.conversation_id.as_str(),
                    );
                    item
                })
                .collect();
            section.conversations = slint::ModelRc::new(slint::VecModel::from(marked));
            section
        })
        .collect();
    window.set_custom_sections(slint::ModelRc::new(slint::VecModel::from(updated)));
}

/// Does this chat-list or folder row point at the currently open conversation?
///
/// `kind` is core's `ConversationType`, typed (`converters::conversation_kind`).
fn row_is_active(
    active: &chatter_core::state::ActiveConversation,
    kind: crate::ConversationKind,
    conv_id: &str,
) -> bool {
    use crate::ConversationKind as K;
    match kind {
        K::Channel | K::SavedMessages => conv_id
            .parse()
            .map(|id| active.matches_channel(&id))
            .unwrap_or(false),
        K::ServerDm | K::AnonDm => {
            // Rows may carry an anon_dm_id, a channel_id, or a peer_id in
            // `conversation_id`. Match against all shapes.
            let anon = conv_id
                .parse()
                .map(|id| active.matches_anon_dm(&id))
                .unwrap_or(false);
            let peer = conv_id
                .parse()
                .map(|p| active.matches_dm_peer(&p))
                .unwrap_or(false);
            let chan = match (conv_id.parse(), conv_id.parse()) {
                (Ok(cid), Ok(pid)) => active.matches_dm(&cid, &pid),
                _ => false,
            };
            anon || peer || chan
        }
        K::AnonGroup => conv_id
            .parse()
            .map(|id| active.matches_anon_group(&id))
            .unwrap_or(false),
        // Mesh conversations are never filed into folders.
        K::Mesh | K::MeshDm => false,
    }
}

/// Rebuild the folder models, but only when folders exist.
///
/// Folder badges (per-row and the header aggregate) used to be frozen from app
/// launch: `load_sections` ran once at startup and only a folder MUTATION
/// refreshed them, so no message arrival or read ever moved them. This is the
/// hook the message and read paths call.
///
/// The row-count guard keeps the common case free — a user with no folders
/// pays nothing. With folders, the queries run on the runtime, not here on the
/// UI thread: this is called for every received message, and a reconnect
/// burst used to stall the UI on one rebuild per message.
pub(crate) fn refresh_section_badges(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    use slint::Model;
    if window.get_custom_sections().row_count() == 0 {
        return;
    }
    let weak = slint::ComponentHandle::as_weak(window);
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move { sections::refresh_sections_ui(&weak, &ui_ctx) });
}

/// Apply pinned message state to the UI.
///
/// Shared helper used by channel, DM, and private group sidebar handlers
/// to update the pinned message bar after loading pinned messages.
/// Pins belong to the same history request as the model/header they accompany.
pub(crate) fn apply_pinned_ui_owned(
    weak: &slint::Weak<MainWindow>,
    chat: &chatter_core::state::ChatState,
    ids: Vec<chatter_core::domain::ids::MessageId>,
    preview: String,
    owner: &crate::bin_ui::features::history_owner::HistoryOwner,
) {
    let chat = chat.clone();
    crate::run_history_ui!(weak, owner, |w| {
        if ids.is_empty() {
            chat.clear_pinned();
            w.set_has_pinned_messages(false);
            w.set_pinned_message_preview("".into());
            w.set_pinned_total_count(0);
        } else {
            let count = ids.len() as i32;
            chat.set_pinned_messages(ids);
            w.set_has_pinned_messages(chat.pinned_should_show());
            w.set_pinned_message_preview(preview.into());
            w.set_pinned_current_index(1);
            w.set_pinned_total_count(count);
        }
    });
}

/// Update a sidebar item's preview text and timestamp, moving it to the top
/// (MRU). Its unread badge is left alone — for a received message use
/// [`apply_received_to_sidebar`].
///
/// Works for all conversation types: DM, Channel, and Private Group.
pub(crate) fn update_sidebar_preview(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    conversation: chatter_core::state::ActiveConversation,
    preview: &str,
    timestamp: &str,
) {
    update_sidebar_row(window, ui_ctx, conversation, preview, timestamp, None);
}

/// [`update_sidebar_preview`] for a received message, also setting the row's
/// unread badge and the total to the absolute values core computed
/// (`ChatterClient::unread_badges`, read off the UI thread). The UI never
/// counts: its old `+1` disagreed with every rebuild.
///
/// A conversation with no row yet (a first message from a new peer) is not
/// hand-built here: core decides whether it gets one — a DM filed into a
/// folder does not — so the list is rebuilt from core instead.
pub fn apply_received_to_sidebar(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    conversation: chatter_core::state::ActiveConversation,
    preview: &str,
    timestamp: &str,
    badges: chatter_core::client::UnreadBadges,
) {
    if !update_sidebar_row(
        window,
        ui_ctx,
        conversation,
        preview,
        timestamp,
        Some(badges.row as i32),
    ) {
        let (weak, ui_ctx) = (slint::ComponentHandle::as_weak(window), ui_ctx.clone());
        tokio::spawn(async move { rebuild_chat_list(&weak, &ui_ctx) });
    }
    window.set_total_unread_count(badges.total as i32);
}

/// Move the row for `conversation` to the top with a new preview line, and
/// set its unread badge when `unread` is given. "The top" is core's rule
/// (`chat_row_top_slot`): pinned rows stay first, so an unpinned row lands
/// just below them. False when the list has no row for it.
fn update_sidebar_row(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    conversation: chatter_core::state::ActiveConversation,
    preview: &str,
    timestamp: &str,
    unread: Option<i32>,
) -> bool {
    use chatter_core::state::ActiveConversation;
    use slint::Model;

    // Every received and sent message funnels through here, so this is where
    // a foldered conversation's badge gets a chance to move.
    refresh_section_badges(window, ui_ctx);

    let key = match conversation {
        ActiveConversation::ServerDm { peer_id } => peer_id.to_string(),
        ActiveConversation::Channel(id) => id.to_string(),
        ActiveConversation::AnonGroup(id) => id.to_string(),
        ActiveConversation::AnonDm(id) => id.to_string(),
        ActiveConversation::None => return true,
    };
    let model = window.get_chat_rows();
    let mut rows: Vec<_> = model.iter().collect();
    let Some(i) = rows.iter().position(|r| r.conversation_id.as_str() == key) else {
        return false;
    };
    let mut row = rows.remove(i);
    row.last_message = preview.into();
    row.timestamp = timestamp.into();
    if let Some(n) = unread {
        row.unread_count = n;
    }
    let slot = chatter_core::ui::sidebar_data::chat_row_top_slot(
        row.is_pinned,
        rows.iter().map(|r| r.is_pinned),
    );
    rows.insert(slot, row);
    window.set_chat_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
    true
}

/// Rebuild the chat list from core (`ChatterClient::sidebar_rows`) — the one
/// writer of the `chat-rows` model. Core's queries run on the calling
/// thread, so call this from the runtime, never the UI thread
/// ([`refresh_chat_list`] is the UI-thread entry). Every rebuild re-marks the
/// open conversation and recomputes the Chats-tab badge, which counts the
/// same rows.
pub(crate) fn rebuild_chat_list(weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    let mut rows = ui_ctx.sidebar_rows();
    for row in rows.iter_mut() {
        row.pre_decode_avatar();
    }
    let ui_ctx = ui_ctx.clone();
    run_ui!(weak, |w| {
        apply_chat_list(&w, &ui_ctx, rows);
    });
}

/// Apply a core sidebar snapshot on the UI thread after the caller's guards.
pub(crate) fn apply_chat_list(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    rows: Vec<chatter_core::ui::sidebar_data::ChatRowSendData>,
) {
    use crate::bin_ui::converters::ChatRowExt;
    let rows: Vec<_> = rows.into_iter().map(|r| r.to_slint()).collect();
    window.set_chat_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
    refresh_selection(window, ui_ctx);
    crate::bin_ui::features::auth::inbox::domain_handler::recompute_total_unread(window, ui_ctx);
}

/// A conversation's stored history changed without a new message arriving —
/// an edit, a delete for everyone or for me — so its row's preview (and, for
/// a deleted last message, its place in the list) may be stale. Rows are
/// rebuilt from core, which reads storage; core has applied the change before
/// announcing it. Runtime thread, like [`rebuild_chat_list`].
pub(crate) fn history_changed(weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    sections::refresh_sections_ui(weak, ui_ctx);
    rebuild_chat_list(weak, ui_ctx);
}

/// `conv` no longer exists on this device (a deleted chat, a left or
/// kicked-from group): empty the chat pane if it was open, then rebuild the
/// folders and the chat list. Runtime thread, like [`rebuild_chat_list`].
pub(crate) fn conversation_removed(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    conv: chatter_core::state::ActiveConversation,
) {
    let ui_ctx_ui = ui_ctx.clone();
    let session_revision = ui_ctx.auth_state().get().session_revision;
    run_ui!(weak, |w| {
        if ui_ctx_ui.composer_session_is_current(session_revision)
            && ui_ctx_ui.active_conversation() == conv
        {
            ui_ctx_ui
                .chat_state()
                .set_active_conversation(chatter_core::state::ActiveConversation::None);
            w.set_messages(slint::ModelRc::new(
                slint::VecModel::<crate::MessageItemData>::default(),
            ));
            // No name = nothing open: the pane shows "Select a conversation".
            w.set_channel_name("".into());
            // The empty detail pane is useful on desktop; on a phone the
            // removed conversation must reveal the list without another Back.
            // Keep pushed screens intact: their owners close them separately.
            if w.get_is_mobile_platform() {
                w.set_mobile_show_chat(false);
                w.set_active_tab("chats".into());
            }
        }
    });
    sections::refresh_sections_ui(weak, ui_ctx);
    rebuild_chat_list(weak, ui_ctx);
}

/// [`rebuild_chat_list`] from the UI thread. Friends are synced from the
/// server first — `list_friends_with_profiles` persists each accepted friend
/// locally — so a friendship accepted on another device shows up too.
pub fn refresh_chat_list(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = slint::ComponentHandle::as_weak(window);
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let _ = ui_ctx.list_friends_with_profiles().await;
        rebuild_chat_list(&weak, &ui_ctx);
    });
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
        // Clear the old header synchronously, before revealing the pane or
        // waiting for the next conversation's asynchronous load.
        w.set_loading_messages(true);
        w.set_channel_name("".into());
        w.set_topic("".into());
        w.set_member_count(0);
        w.set_header_presence_status("offline".into());
        w.set_channel_avatar_source(slint::Image::default());
        w.set_channel_has_avatar(false);
        // The header's typing line belongs to the conversation being left.
        w.set_typing_indicator("".into());
        if w.get_is_mobile_platform() {
            w.set_mobile_show_chat(true);
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
