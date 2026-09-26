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
pub use private_group::add_private_group_to_sidebar;

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

    // Channels (regular server channels only — DMs live in a separate model).
    {
        let model = window.get_channels();
        let rows: Vec<_> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .map(|mut item| {
                item.is_selected = item
                    .channel_id
                    .parse()
                    .map(|id| active.matches_channel(&id))
                    .unwrap_or(false);
                item
            })
            .collect();
        window.set_channels(slint::ModelRc::new(slint::VecModel::from(rows)));
    }

    // DM conversations (regular + anon, disambiguated via is_anon).
    {
        let model = window.get_dm_conversations();
        let rows: Vec<_> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .map(|mut item| {
                item.is_selected = if item.is_anon {
                    item.channel_id
                        .parse()
                        .map(|id| active.matches_anon_dm(&id))
                        .unwrap_or(false)
                } else {
                    match (item.channel_id.parse(), item.user_id.parse()) {
                        (Ok(cid), Ok(pid)) => active.matches_dm(&cid, &pid),
                        _ => false,
                    }
                };
                item
            })
            .collect();
        window.set_dm_conversations(slint::ModelRc::new(slint::VecModel::from(rows)));
    }

    // Private (anonymous) groups.
    {
        let model = window.get_private_groups();
        let rows: Vec<_> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .map(|mut item| {
                item.is_selected = item
                    .group_id
                    .parse()
                    .map(|id| active.matches_anon_group(&id))
                    .unwrap_or(false);
                item
            })
            .collect();
        window.set_private_groups(slint::ModelRc::new(slint::VecModel::from(rows)));
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
    let updated: Vec<_> = (0..sections.row_count())
        .filter_map(|i| sections.row_data(i))
        .map(|mut section| {
            let rows = section.conversations.clone();
            let marked: Vec<_> = (0..rows.row_count())
                .filter_map(|j| rows.row_data(j))
                .map(|mut item| {
                    item.is_selected = section_row_is_active(
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

/// Does this folder row point at the currently open conversation?
///
/// `kind` is core's `ConversationType`, typed (`converters::conversation_kind`).
fn section_row_is_active(
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
            // Folder rows may carry an anon_dm_id, a channel_id, or a peer_id
            // in `conversation_id`. Match against all shapes.
            let anon = conv_id.parse().map(|id| active.matches_anon_dm(&id)).unwrap_or(false);
            let peer = conv_id.parse().map(|p| active.matches_dm_peer(&p)).unwrap_or(false);
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
pub(crate) fn apply_pinned_ui(
    weak: &slint::Weak<MainWindow>,
    chat_state: &chatter_core::state::ChatState,
    message_ids: Vec<chatter_core::domain::ids::MessageId>,
    preview: String,
) {
    if message_ids.is_empty() {
        chat_state.clear_pinned();
        run_ui!(weak, |w| {
            w.set_has_pinned_messages(false);
            w.set_pinned_message_preview("".into());
            w.set_pinned_total_count(0);
        });
    } else {
        let total_count = message_ids.len() as i32;
        chat_state.set_pinned_messages(message_ids);
        let should_show = chat_state.pinned_should_show();
        run_ui!(weak, |w| {
            w.set_has_pinned_messages(should_show);
            w.set_pinned_message_preview(preview.into());
            w.set_pinned_current_index(1);
            w.set_pinned_total_count(total_count);
        });
    }
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
pub(crate) fn apply_received_to_sidebar(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    conversation: chatter_core::state::ActiveConversation,
    preview: &str,
    timestamp: &str,
    badges: chatter_core::client::UnreadBadges,
) {
    update_sidebar_row(window, ui_ctx, conversation, preview, timestamp, Some(badges.row as i32));
    window.set_total_unread_count(badges.total as i32);
}

/// Move the row for `conversation` to the top with a new preview line, and
/// set its unread badge when `unread` is given.
fn update_sidebar_row(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    conversation: chatter_core::state::ActiveConversation,
    preview: &str,
    timestamp: &str,
    unread: Option<i32>,
) {
    use chatter_core::state::ActiveConversation;
    use slint::Model;

    // Every received and sent message funnels through here, so this is where
    // a foldered conversation's badge gets a chance to move.
    refresh_section_badges(window, ui_ctx);

    match conversation {
        ActiveConversation::ServerDm { peer_id } => {
            let peer_id_str = peer_id.to_string();
            let model = window.get_dm_conversations();
            let mut others = Vec::new();
            let mut updated = None;

            for i in 0..model.row_count() {
                if let Some(item) = model.row_data(i) {
                    if item.user_id.as_str() == peer_id_str {
                        let mut u = item.clone();
                        u.last_message = preview.into();
                        u.timestamp = timestamp.into();
                        if let Some(n) = unread { u.unread_count = n; }
                        updated = Some(u);
                    } else {
                        others.push(item);
                    }
                }
            }
            if let Some(item) = updated {
                others.insert(0, item);
                window.set_dm_conversations(slint::ModelRc::new(slint::VecModel::from(others)));
            }
        }
        ActiveConversation::Channel(channel_id) => {
            let channel_id_str = channel_id.to_string();
            let model = window.get_channels();
            let mut others = Vec::new();
            let mut updated = None;

            for i in 0..model.row_count() {
                if let Some(item) = model.row_data(i) {
                    if item.channel_id.as_str() == channel_id_str {
                        let mut u = item.clone();
                        u.last_message = preview.into();
                        u.timestamp = timestamp.into();
                        if let Some(n) = unread { u.unread_count = n; }
                        updated = Some(u);
                    } else {
                        others.push(item);
                    }
                }
            }
            if let Some(item) = updated {
                others.insert(0, item);
                window.set_channels(slint::ModelRc::new(slint::VecModel::from(others)));
            }
        }
        ActiveConversation::AnonGroup(group_id) => {
            let group_id_str = group_id.to_string();
            let model = window.get_private_groups();
            let mut others = Vec::new();
            let mut updated = None;

            for i in 0..model.row_count() {
                if let Some(item) = model.row_data(i) {
                    if item.group_id.as_str() == group_id_str {
                        let mut u = item.clone();
                        u.last_message = preview.into();
                        u.timestamp = timestamp.into();
                        if let Some(n) = unread { u.unread_count = n; }
                        updated = Some(u);
                    } else {
                        others.push(item);
                    }
                }
            }
            if let Some(item) = updated {
                others.insert(0, item);
                window.set_private_groups(slint::ModelRc::new(slint::VecModel::from(others)));
            }
        }
        ActiveConversation::AnonDm(anon_dm_id) => {
            let channel_id_str = anon_dm_id.to_string();
            let model = window.get_dm_conversations();
            let mut others = Vec::new();
            let mut updated = None;

            for i in 0..model.row_count() {
                if let Some(item) = model.row_data(i) {
                    if item.is_anon && item.channel_id.as_str() == channel_id_str {
                        let mut u = item.clone();
                        u.last_message = preview.into();
                        u.timestamp = timestamp.into();
                        if let Some(n) = unread { u.unread_count = n; }
                        updated = Some(u);
                    } else {
                        others.push(item);
                    }
                }
            }
            if let Some(item) = updated {
                others.insert(0, item);
                window.set_dm_conversations(slint::ModelRc::new(slint::VecModel::from(others)));
            }
        }
        ActiveConversation::None => {
            // None is a no-op
        }
    }
}

/// Check if a DM row for a peer is already in the sidebar model.
///
/// DMs are channel-free (keyed by peer user_id), so this checks the Slint
/// DM model directly rather than a local channel row.
pub(crate) fn sidebar_has_dm(
    window: &MainWindow,
    peer_id: &chatter_core::domain::UserId,
) -> bool {
    use slint::Model;
    let peer_id_str = peer_id.to_string();
    let model = window.get_dm_conversations();
    (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .any(|dm| dm.user_id == peer_id_str)
}

/// Create a new DM sidebar entry for a peer not yet in the list.
///
/// Used when receiving the first message from a new peer. Channel-free —
/// the row is keyed by the peer's user_id. Inserted at position 0 (MRU)
/// with core's absolute unread count.
pub(crate) fn create_dm_sidebar_entry(
    window: &MainWindow,
    ui_ctx: &std::sync::Arc<chatter_core::client::ChatterClient>,
    peer_id: &chatter_core::domain::UserId,
    sender_name: &str,
    preview: &str,
    timestamp: &str,
    badges: chatter_core::client::UnreadBadges,
) {
    use slint::Model;

    let peer_id_str = peer_id.to_string();
    let avatar = ui_ctx
        .get_profile_local(peer_id)
        .and_then(|p| p.avatar_bytes)
        .and_then(|bytes| crate::bin_ui::converters::avatar_bytes_to_image(&bytes));

    let new_dm = crate::DMListItemData {
        channel_id: peer_id_str.clone().into(), // peer_id is the conversation key
        user_id: peer_id_str.clone().into(),
        display_name: sender_name.into(),
        avatar_source: avatar.clone().unwrap_or_default(),
        has_avatar: avatar.is_some(),
        initials: crate::bin_ui::converters::get_initials(sender_name).into(),
        presence_status: "offline".into(),
        last_message: preview.into(),
        timestamp: timestamp.into(),
        unread_count: badges.row as i32,
        is_typing: false,
        is_anon: false,
        is_pinned: false,
        is_selected: false,
    };

    let model = window.get_dm_conversations();
    let mut items: Vec<_> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect();
    items.insert(0, new_dm);
    window.set_dm_conversations(slint::ModelRc::new(slint::VecModel::from(items)));
    window.set_total_unread_count(badges.total as i32);

    tracing::info!(
        peer_id = %peer_id_str,
        "Added missing DM conversation to sidebar (channel-free)"
    );
}

/// Stop any currently playing audio and clear state.
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
        if w.get_is_mobile_platform() {
            w.set_mobile_show_chat(true);
        }
        // The header's typing line belongs to the conversation being left.
        w.set_typing_indicator("".into());
    }
}

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
