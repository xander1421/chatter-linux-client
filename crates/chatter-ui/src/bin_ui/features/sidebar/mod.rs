//! Sidebar callbacks.
//!
//! Handles channel/DM selection, settings, and profile.

use slint::ComponentHandle;
mod anon_mode;
pub mod channel;
pub mod dm;
pub(crate) mod navigation;
pub mod private_group;
pub mod sections;

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use chatter_core::client::ChatterClient;

use crate::bin_ui::audio_state;
use crate::{MainWindow, run_ui};

static NEXT_ARRIVAL_SERIAL: AtomicI32 = AtomicI32::new(1);

fn next_arrival_serial() -> i32 {
    NEXT_ARRIVAL_SERIAL.fetch_add(1, Ordering::Relaxed)
}

/// Project the two retained source models into the one virtualized sidebar
/// model. Core owns the ordering and expanded-folder policy; this adapter
/// only attaches Slint values to those slots.
pub(crate) fn refresh_flat_rows(window: &MainWindow) {
    use chatter_core::ui::sidebar_data::{sidebar_slots, SidebarSlot};
    use slint::{Model, ModelRc, VecModel};

    let pane = window.global::<crate::SidebarPane>();
    let chats = pane.get_chat_rows();
    let sections = pane.get_custom_sections();
    let sections: Vec<_> = sections.iter().collect();
    let shape: Vec<_> = sections.iter().map(|s| (s.is_expanded, s.conversations.row_count())).collect();
    let mut rows: Vec<crate::SidebarRowData> = sidebar_slots(chats.row_count(), &shape)
        .into_iter()
        .map(|slot| {
            let (kind, chat, section) = match slot {
                SidebarSlot::Search => ("search", None, None),
                SidebarSlot::Saved => ("saved", None, None),
                SidebarSlot::Friends => ("friends", None, None),
                SidebarSlot::Calls => ("calls", None, None),
                SidebarSlot::Requests => ("requests", None, None),
                SidebarSlot::ChannelActions => ("channel-actions", None, None),
                SidebarSlot::Empty => ("empty", None, None),
                SidebarSlot::Loading => ("loading", None, None),
                SidebarSlot::Chat(index) => ("chat", chats.row_data(index), None),
                SidebarSlot::SectionHeader(index) => ("section-header", None, sections.get(index).cloned()),
                SidebarSlot::SectionChat(index, row) => ("section-chat", sections[index].conversations.row_data(row), sections.get(index).cloned()),
                SidebarSlot::Bottom => ("bottom", None, None),
            };
            let key = match slot {
                SidebarSlot::Chat(_) => format!("chat:{}", chat.as_ref().map_or("", |r| r.conversation_id.as_str())),
                SidebarSlot::SectionHeader(_) => format!("section:{}", section.as_ref().map_or("", |s| s.section_id.as_str())),
                SidebarSlot::SectionChat(_, _) => format!("section-chat:{}:{}",
                    section.as_ref().map_or("", |s| s.section_id.as_str()),
                    chat.as_ref().map_or("", |r| r.conversation_id.as_str())),
                _ => kind.to_string(),
            };
            crate::SidebarRowData {
                key: key.into(),
                kind: kind.into(),
                chat: chat.unwrap_or_default(),
                section: section.unwrap_or_default(),
            }
        })
        .collect();
    let archived = pane.get_archived_rows();
    let empty = |kind: &str| crate::SidebarRowData {
        key: kind.into(),
        kind: kind.into(),
        chat: Default::default(),
        section: Default::default(),
    };
    if pane.get_archive_open() {
        rows.clear();
        rows.push(empty("archive-back"));
        for chat in archived.iter() {
            rows.push(crate::SidebarRowData {
                key: format!("archived-chat:{}", chat.conversation_id).into(),
                kind: "archived-chat".into(),
                chat,
                section: Default::default(),
            });
        }
        if archived.row_count() == 0 {
            rows.push(empty("archive-empty"));
        }
        rows.push(empty("bottom"));
    } else if archived.row_count() > 0 {
        rows.insert(1, empty("archive-entry"));
    }
    let model = pane.get_flat_rows();
    if let Some(vec) = model.as_any().downcast_ref::<VecModel<crate::SidebarRowData>>() {
        let target_len = rows.len();
        let mut keys = (0..vec.row_count()).filter_map(|i| vec.row_data(i)
            .map(|row| row.key.to_string())).collect::<Vec<_>>();
        let mut positions = keys.iter().enumerate()
            .map(|(i, key)| (key.clone(), i)).collect::<std::collections::HashMap<_, _>>();
        for (target, row) in rows.into_iter().enumerate() {
            let key = row.key.to_string();
            if positions.get(&key).copied() == Some(target) {
                if vec.row_data(target).is_some_and(|old| old != row) {
                    vec.set_row_data(target, row);
                }
                continue;
            }
            if let Some(from) = positions.get(&key).copied().filter(|&from| from > target) {
                vec.remove(from);
                keys.remove(from);
            }
            vec.insert(target, row);
            keys.insert(target, key);
            for i in target..keys.len() {
                positions.insert(keys[i].clone(), i);
            }
        }
        while vec.row_count() > target_len {
            vec.remove(vec.row_count() - 1);
        }
    } else {
        pane.set_flat_rows(ModelRc::new(VecModel::from(rows)));
    }
}

/// Clear only the marked incarnation: a newer message may promote the same row
/// before the prior animation's first frame has rendered.
fn settle_arrivals(
    weak: slint::Weak<MainWindow>,
    model: slint::ModelRc<crate::ChatRowData>,
    arrivals: Vec<(slint::SharedString, i32)>,
) {
    if arrivals.is_empty() {
        return;
    }
    slint::Timer::single_shot(Duration::from_millis(16), move || {
        use slint::Model;
        for (id, serial) in arrivals {
            if let Some(index) = (0..model.row_count()).find(|&i| {
                model
                    .row_data(i)
                    .is_some_and(|row| row.conversation_id == id && row.arrival_serial == serial)
            }) {
                if let Some(mut row) = model.row_data(index) {
                    row.arrival_serial = 0;
                    model.set_row_data(index, row);
                }
            }
        }
        if let Some(window) = weak.upgrade() {
            refresh_flat_rows(&window);
        }
    });
}

// Re-export public functions
pub use channel::load_channel_into_ui;

/// Wire sidebar callbacks (channel/DM selection, settings, profile).
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let focus_window = window.as_weak();
    window.global::<crate::SidebarPane>().on_sidebar_focus_target(move |current, move_by, mobile, shield, requests| {
        use slint::Model;
        let Some(window) = focus_window.upgrade() else { return current; };
        let rows = window.global::<crate::SidebarPane>().get_flat_rows();
        let visible: Vec<_> = (0..rows.row_count()).filter(|&index| {
            rows.row_data(index).is_some_and(|row| match row.kind.as_str() {
                "saved" => !shield,
                "friends" | "calls" => !mobile,
                "requests" => requests,
                "chat" | "section-chat" | "archived-chat" | "archive-entry" | "archive-back" | "section-header" => true,
                _ => false,
            })
        }).collect();
        chatter_core::ui::sidebar_data::sidebar_focus_target(&visible, current.max(0) as usize, move_by)
            .map_or(current, |index| index as i32)
    });
    let navigation_client = ui_ctx.clone();
    let navigation_window = window.as_weak();
    window.global::<crate::SidebarPane>().on_navigate_chat(move |direction| {
        let Some(window) = navigation_window.upgrade() else { return; };
        let pane = window.global::<crate::SidebarPane>();
        if let Some(target) = navigation_client.adjacent_sidebar_conversation(
            direction > 0,
            pane.get_archive_open(),
        ) {
            pane.set_active_tab("chats".into());
            pane.set_mobile_show_chat(true);
            select_conversation(&window, target);
        }
    });
    wire_context_menu_mute(window, ui_ctx);
    wire_context_menu_mark_read(window, ui_ctx);
    wire_context_menu_mark_unread(window, ui_ctx);
    wire_context_menu_mute_for(window, ui_ctx);
    channel::wire_channel_selected(window, ui_ctx);
    dm::wire_dm_selected(window, ui_ctx);
    dm::wire_delete_conversation(window, ui_ctx);
    dm::wire_secret_request_answer(window, ui_ctx);
    dm::wire_start_secret_chat(window, ui_ctx);

    // On a phone, leaving the chat pane left its conversation "active": the
    // next message there was marked read (and receipted) unseen, and its row
    // got no unread badge. Clear it the moment the pane closes.
    let (weak, ctx) = (slint::ComponentHandle::as_weak(window), ui_ctx.clone());
    window.on_chat_pane_closed(move || {
        ctx.select_conversation(chatter_core::state::ActiveConversation::None);
        if let Some(w) = weak.upgrade() {
            w.invoke_refresh_screen_protection();
            refresh_selection(&w, &ctx);
        }
    });
    private_group::wire_private_group_selected(window, ui_ctx);
    navigation::wire_saved_messages(window, ui_ctx);
    navigation::wire_settings_clicked(window, ui_ctx);
    navigation::wire_profile_clicked(window, ui_ctx);
    navigation::wire_toggle_sidebar(window, ui_ctx);
    navigation::load_sidebar_collapsed_state(window, ui_ctx);
    navigation::wire_message_requests_clicked(window, ui_ctx);
    navigation::load_pending_message_requests_count(window, ui_ctx);
    navigation::wire_group_invites_clicked(window, ui_ctx);
    navigation::load_pending_group_invites_count(window, ui_ctx);
    sections::wire_section_callbacks(window, ui_ctx);
    sections::load_sections(window, ui_ctx);
    wire_archive(window, ui_ctx);
    anon_mode::wire(window, ui_ctx);
    refresh_flat_rows(window);
}

fn wire_context_menu_mute(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ctx = ui_ctx.clone();
    let weak = window.as_weak();
    window.global::<crate::SidebarPane>().on_row_context_menu_opened(move |row_id, kind| {
        let Some(w) = weak.upgrade() else { return };
        let kind = crate::bin_ui::converters::conversation_type(kind);
        let muted = ctx.resolve_row_conversation(row_id.as_str(), &kind)
            .and_then(|conv| ctx.is_conversation_muted(conv).ok())
            .unwrap_or(false);
        w.global::<crate::SidebarPane>().set_row_context_menu_muted(muted);
    });

    let ctx = ui_ctx.clone();
    let weak = window.as_weak();
    window.global::<crate::SidebarPane>().on_set_conversation_muted(move |row_id, kind, muted| {
        let Some(w) = weak.upgrade() else { return };
        let kind = crate::bin_ui::converters::conversation_type(kind);
        let Some(conv) = ctx.resolve_row_conversation(row_id.as_str(), &kind) else { return };
        match ctx.mute_conversation(conv, muted) {
            Ok(()) => {
                w.global::<crate::SidebarPane>().set_row_context_menu_muted(muted);
                refresh_mute_rows(&w, &ctx);
            }
            Err(error) => {
                tracing::error!(%error, "Could not update conversation mute");
                w.set_toast_message("Couldn't update mute setting".into());
                w.set_toast_variant("error".into());
                w.set_toast_visible(true);
            }
        }
    });
}

/// Row menu "Mute…" durations: 1 hour / 8 hours / 1 day, or 0 = always.
fn wire_context_menu_mute_for(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ctx = ui_ctx.clone();
    let weak = window.as_weak();
    window.global::<crate::SidebarPane>().on_set_conversation_muted_for(move |row_id, kind, seconds| {
        let Some(w) = weak.upgrade() else { return };
        let kind = crate::bin_ui::converters::conversation_type(kind);
        let Some(conv) = ctx.resolve_row_conversation(row_id.as_str(), &kind) else { return };
        let seconds = u64::try_from(seconds).ok().filter(|s| *s > 0);
        match ctx.mute_conversation_for(conv, seconds) {
            Ok(()) => {
                w.global::<crate::SidebarPane>().set_row_context_menu_muted(true);
                refresh_mute_rows(&w, &ctx);
                // Redraw the row when the mute runs out; a restart before then
                // rebuilds the list from the lapsed state anyway.
                if let Some(seconds) = seconds {
                    let (weak, ctx) = (weak.clone(), ctx.clone());
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(seconds + 1)).await;
                        history_changed(&weak, &ctx);
                    });
                }
            }
            Err(error) => {
                tracing::error!(%error, "Could not update conversation mute");
                w.set_toast_message("Couldn't update mute setting".into());
                w.set_toast_variant("error".into());
                w.set_toast_visible(true);
            }
        }
    });
}

/// Row menu "Mark as unread": badges the chat again without opening it.
fn wire_context_menu_mark_unread(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ctx = ui_ctx.clone();
    let weak = window.as_weak();
    window.global::<crate::SidebarPane>().on_mark_conversation_unread(move |row_id, kind| {
        let kind = crate::bin_ui::converters::conversation_type(kind);
        let Some(conv) = ctx.resolve_row_conversation(row_id.as_str(), &kind) else { return };
        if let Err(error) = ctx.conversation(conv).mark_as_unread() {
            tracing::warn!(%error, "Could not mark conversation unread");
        }
        history_changed(&weak, &ctx);
    });
}

/// Row menu "Mark as read": clears the chat's unread without opening it.
fn wire_context_menu_mark_read(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ctx = ui_ctx.clone();
    let weak = window.as_weak();
    window.global::<crate::SidebarPane>().on_mark_conversation_read(move |row_id, kind| {
        let kind = crate::bin_ui::converters::conversation_type(kind);
        let Some(conv) = ctx.resolve_row_conversation(row_id.as_str(), &kind) else { return };
        let ctx = ctx.clone();
        let weak = weak.clone();
        tokio::spawn(async move {
            if let Err(error) = ctx.conversation(conv).mark_as_read().await {
                tracing::warn!(%error, "Could not mark conversation read");
            }
            history_changed(&weak, &ctx);
        });
    });
}

/// Refresh loose, archived and section rows after a persisted mute change.
pub(crate) fn refresh_mute_rows(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move { history_changed(&weak, &ui_ctx) });
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
    window
        .global::<crate::ChatPane>()
        .set_unread_index(index.map_or(-1, |i| i as i32));
    window
        .global::<crate::ChatPane>()
        .set_unread_count(anchor.unread_count as i32);
    window
        .global::<crate::ChatPane>()
        .set_is_at_bottom(index.is_none());
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
        let model = window.global::<crate::SidebarPane>().get_chat_rows();
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

    {
        let model = window.global::<crate::SidebarPane>().get_archived_rows();
        for i in 0..model.row_count() {
            if let Some(mut item) = model.row_data(i) {
                let selected = row_is_active(&active, item.conversation_type, item.conversation_id.as_str());
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
    let sections = window.global::<crate::SidebarPane>().get_custom_sections();
    for section in sections.iter() {
        let rows = section.conversations;
        for j in 0..rows.row_count() {
            if let Some(mut item) = rows.row_data(j) {
                let selected = row_is_active(
                    &active,
                    item.conversation_type,
                    item.conversation_id.as_str(),
                );
                if item.is_selected != selected {
                    item.is_selected = selected;
                    rows.set_row_data(j, item);
                }
            }
        }
    }
    refresh_flat_rows(window);
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
    if window
        .global::<crate::SidebarPane>()
        .get_custom_sections()
        .row_count()
        == 0
    {
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
    update_sidebar_row(window, ui_ctx, conversation, preview, timestamp, None, None);
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
    mention_count: u32,
) {
    if !update_sidebar_row(
        window,
        ui_ctx,
        conversation,
        preview,
        timestamp,
        Some(badges.row as i32),
        Some(mention_count as i32),
    ) {
        let (weak, ui_ctx) = (slint::ComponentHandle::as_weak(window), ui_ctx.clone());
        tokio::spawn(async move { rebuild_chat_list(&weak, &ui_ctx) });
    }
    window
        .global::<crate::SidebarPane>()
        .set_total_unread_count(badges.total as i32);
}

/// Thread replies count as unread in their channel without changing the main
/// chat's preview, timestamp, or position.
pub fn apply_thread_reply_badges(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    conversation: chatter_core::state::ActiveConversation,
    badges: chatter_core::client::UnreadBadges,
    mention_count: u32,
) {
    use chatter_core::state::ActiveConversation;
    refresh_section_badges(window, ui_ctx);
    let key = match conversation {
        ActiveConversation::ServerDm { peer_id } => peer_id.to_string(),
        ActiveConversation::Channel(id) => id.to_string(),
        ActiveConversation::AnonGroup(id) => id.to_string(),
        ActiveConversation::AnonDm(id) => id.to_string(),
        ActiveConversation::None => String::new(),
    };
    let model = window.global::<crate::SidebarPane>().get_chat_rows();
    let mut found = false;
    use slint::Model as _;
    if let Some(vec) = model.as_any().downcast_ref::<slint::VecModel<crate::ChatRowData>>() {
        if let Some(i) = (0..vec.row_count()).find(|&i| {
            vec.row_data(i).is_some_and(|row| row.conversation_id.as_str() == key.as_str())
        }) {
            if let Some(mut row) = vec.row_data(i) {
                found = true;
                row.unread_count = badges.row as i32;
                row.mention_count = mention_count as i32;
                crate::bin_ui::converters::channel::refresh_row_accessible_label(&mut row);
                vec.set_row_data(i, row);
                refresh_flat_rows(window);
            }
        }
    }
    if !found {
        let (weak, ui_ctx) = (window.as_weak(), ui_ctx.clone());
        tokio::spawn(async move { rebuild_chat_list(&weak, &ui_ctx) });
    }
    window.global::<crate::SidebarPane>().set_total_unread_count(badges.total as i32);
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
    mentions: Option<i32>,
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
    let model = window.global::<crate::SidebarPane>().get_chat_rows();
    let Some(vec) = model
        .as_any()
        .downcast_ref::<slint::VecModel<crate::ChatRowData>>()
    else {
        return false;
    };
    let Some(i) = (0..vec.row_count()).find(|&i| {
        vec.row_data(i)
            .is_some_and(|r| r.conversation_id.as_str() == key)
    }) else {
        return false;
    };
    let mut row = vec.row_data(i).expect("located sidebar row");
    row.last_message = preview.into();
    row.timestamp = timestamp.into();
    if let Some(n) = unread {
        row.unread_count = n;
    }
    if let Some(n) = mentions {
        row.mention_count = n;
    }
    crate::bin_ui::converters::channel::refresh_row_accessible_label(&mut row);
    let slot = chatter_core::ui::sidebar_data::chat_row_top_slot(
        row.is_pinned,
        (0..vec.row_count())
            .filter(|&index| index != i)
            .filter_map(|index| vec.row_data(index).map(|r| r.is_pinned)),
    );
    if i == slot {
        vec.set_row_data(i, row);
    } else {
        row.arrival_serial = next_arrival_serial();
        let arrival = (row.conversation_id.clone(), row.arrival_serial);
        vec.remove(i);
        vec.insert(slot, row);
        settle_arrivals(window.as_weak(), model.clone(), vec![arrival]);
    }
    refresh_flat_rows(window);
    true
}

/// Rebuild the chat list from core (`ChatterClient::sidebar_rows`) — the one
/// writer of the `chat-rows` model. Core's queries run on the calling
/// thread, so call this from the runtime, never the UI thread
/// ([`refresh_chat_list`] is the UI-thread entry). Every rebuild re-marks the
/// open conversation and recomputes the Chats-tab badge, which counts the
/// same rows.
pub(crate) fn rebuild_chat_list(weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    let rows = ui_ctx.sidebar_rows();
    let ui_ctx = ui_ctx.clone();
    run_ui!(weak, |w| {
        apply_chat_list(&w, &ui_ctx, rows);
    });
}

/// Rebuild local row previews after a composer draft is parked or restored.
pub(crate) fn refresh_draft_rows(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        rebuild_chat_list(&weak, &ui_ctx);
        sections::refresh_sections_ui(&weak, &ui_ctx);
        rebuild_archived_list(&weak, &ui_ctx);
    });
}

fn rebuild_archived_list(weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    let rows = ui_ctx.archived_sidebar_rows();
    let ui_ctx = ui_ctx.clone();
    run_ui!(weak, |w| {
        use crate::bin_ui::converters::ChatRowExt;
        use slint::Model;
        let active = ui_ctx.active_conversation();
        let rows = rows.into_iter().map(|row| {
            let mut row = row.to_slint();
            row.is_selected = row_is_active(&active, row.conversation_type, row.conversation_id.as_str());
            row
        }).collect::<Vec<_>>();
        let pane = w.global::<crate::SidebarPane>();
        let model = pane.get_archived_rows();
        if let Some(vec) = model.as_any().downcast_ref::<slint::VecModel<crate::ChatRowData>>() {
            reconcile_chat_rows(vec, rows, false);
        } else {
            pane.set_archived_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
        }
        refresh_selection(&w, &ui_ctx);
    });
}

fn wire_archive(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    window.global::<crate::SidebarPane>().on_archive_view_changed(move || {
        if let Some(w) = weak.upgrade() {
            refresh_flat_rows(&w);
        }
    });
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.global::<crate::SidebarPane>().on_set_conversation_archived(move |id, archived| {
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        tokio::spawn(async move {
            if let Err(error) = ui_ctx.set_conversation_archived(id.as_str(), archived) {
                tracing::error!(%error, "Failed to change chat archive state");
                let message = format!("Couldn't change archive: {error}");
                run_ui!(weak, |w| {
                    w.set_toast_message(message.into());
                    w.set_toast_variant("error".into());
                    w.set_toast_visible(true);
                });
                return;
            }
            rebuild_chat_list(&weak, &ui_ctx);
            sections::refresh_sections_ui(&weak, &ui_ctx);
        });
    });
}

/// Apply a core sidebar snapshot on the UI thread after the caller's guards.
pub(crate) fn apply_chat_list(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    rows: Vec<chatter_core::ui::sidebar_data::ChatRowSendData>,
) {
    use crate::bin_ui::converters::ChatRowExt;
    use slint::Model;
    let active = ui_ctx.active_conversation();
    let rows: Vec<_> = rows.into_iter().map(|r| {
        let mut row = r.to_slint();
        row.is_selected = row_is_active(&active, row.conversation_type, row.conversation_id.as_str());
        row
    }).collect();
    let model = window.global::<crate::SidebarPane>().get_chat_rows();
    let arrivals;
    if let Some(vec) = model
        .as_any()
        .downcast_ref::<slint::VecModel<crate::ChatRowData>>()
    {
        // Apply the core snapshot by identity. Replacing the ModelRc made
        // every row disappear and reappear for one changed preview.
        arrivals = reconcile_chat_rows(vec, rows, true);
    } else {
        arrivals = Vec::new();
        window
            .global::<crate::SidebarPane>()
            .set_chat_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
    }
    settle_arrivals(
        window.as_weak(),
        window.global::<crate::SidebarPane>().get_chat_rows(),
        arrivals,
    );
    refresh_selection(window, ui_ctx);
    let (weak, archive_ctx) = (window.as_weak(), ui_ctx.clone());
    tokio::spawn(async move { rebuild_archived_list(&weak, &archive_ctx) });
    crate::bin_ui::features::auth::inbox::domain_handler::recompute_total_unread(window, ui_ctx);
    #[cfg(target_os = "android")]
    crate::platform::android::notification_route::open_pending(window);
}

/// Apply a keyed snapshot while preserving delegates for unchanged rows.
pub(super) fn reconcile_chat_rows(
    vec: &slint::VecModel<crate::ChatRowData>,
    rows: Vec<crate::ChatRowData>,
    animate: bool,
) -> Vec<(slint::SharedString, i32)> {
    use slint::Model;
    let had_rows = vec.row_count() > 0;
    let mut keys = (0..vec.row_count()).filter_map(|i| vec.row_data(i)
        .map(|row| row.conversation_id.to_string())).collect::<Vec<_>>();
    let mut positions = keys.iter().enumerate()
        .map(|(i, key)| (key.clone(), i)).collect::<std::collections::HashMap<_, _>>();
    let mut arrivals = Vec::new();
    let target_len = rows.len();
    for (target, mut row) in rows.into_iter().enumerate() {
        let key = row.conversation_id.to_string();
        if positions.get(&key).copied() == Some(target) {
            if vec.row_data(target).is_some_and(|old| old != row) {
                vec.set_row_data(target, row);
            }
            continue;
        }
        let from = positions.get(&key).copied().filter(|&from| from > target);
        if animate && (from.is_some() || had_rows) {
            row.arrival_serial = next_arrival_serial();
            arrivals.push((row.conversation_id.clone(), row.arrival_serial));
        }
        if let Some(from) = from {
            vec.remove(from);
            keys.remove(from);
        }
        vec.insert(target, row);
        keys.insert(target, key);
        for i in target..keys.len() {
            positions.insert(keys[i].clone(), i);
        }
    }
    while vec.row_count() > target_len {
        vec.remove(vec.row_count() - 1);
    }
    arrivals
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
    let session_revision = ui_ctx.auth_snapshot().session_revision;
    run_ui!(weak, |w| {
        if ui_ctx_ui.composer_session_is_current(session_revision)
            && ui_ctx_ui.active_conversation() == conv
        {
            close_open_conversation(&w, &ui_ctx_ui);
        }
    });
    sections::refresh_sections_ui(weak, ui_ctx);
    rebuild_chat_list(weak, ui_ctx);
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
        w.global::<crate::ChatPane>()
            .set_thread_replies(slint::ModelRc::new(slint::VecModel::default()));
    }
    ui_ctx.select_conversation(chatter_core::state::ActiveConversation::None);
    w.invoke_refresh_screen_protection();
    w.global::<crate::ChatPane>()
        .set_messages(slint::ModelRc::new(
            slint::VecModel::<crate::MessageItemData>::default(),
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

/// [`rebuild_chat_list`] from the UI thread. Rows are rebuilt from local
/// state at once, then again after friends are synced from the server —
/// `list_friends_with_profiles` persists each accepted friend locally — so a
/// friendship accepted on another device shows up too. The first rebuild must
/// not wait on that sync: after entering Shield it runs over a cold Tor link,
/// and the channel rows Shield hides stayed listed until Tor connected.
pub fn refresh_chat_list(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = slint::ComponentHandle::as_weak(window);
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        rebuild_chat_list(&weak, &ui_ctx);
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
