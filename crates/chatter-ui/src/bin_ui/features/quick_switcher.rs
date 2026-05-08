//! Quick switcher (Ctrl+K) — fast conversation navigation.
//!
//! Aggregates channels, DMs, and private groups from the sidebar models,
//! filters by query, and navigates on selection.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::quick_switcher::{arrange_entries, QuickSwitcherEntry};

use crate::{MainWindow, QuickSwitcherItem};

/// Wire quick switcher callbacks.
pub fn wire(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    wire_opened(window);
    wire_search(window);
    wire_select(window);
}

/// On open: populate with recent conversations (no filter).
fn wire_opened(window: &MainWindow) {
    let weak = window.as_weak();

    window.on_quick_switcher_opened(move || {
        if let Some(w) = weak.upgrade() {
            let items = collect_all_items(&w);
            set_items(&w, items);
        }
    });
}

/// On search: filter items by query substring.
fn wire_search(window: &MainWindow) {
    let weak = window.as_weak();

    window.on_quick_switcher_search(move |query: SharedString| {
        if let Some(w) = weak.upgrade() {
            let query_lower = query.to_string().to_lowercase();
            let all = collect_all_items(&w);

            let filtered: Vec<QuickSwitcherItem> = if query_lower.is_empty() {
                all
            } else {
                all.into_iter()
                    .filter(|item| item.name.to_lowercase().contains(&query_lower))
                    .collect()
            };

            set_items(&w, filtered);
        }
    });
}

/// On select: navigate to the selected conversation.
fn wire_select(window: &MainWindow) {
    let weak = window.as_weak();

    window.on_quick_switcher_select(move |id: SharedString, conv_type: SharedString| {
        if let Some(w) = weak.upgrade() {
            match conv_type.as_str() {
                "channel" => w.invoke_channel_selected(id),
                "dm" | "anon_dm" => w.invoke_dm_selected(id),
                "private_group" => w.invoke_private_group_selected(id),
                _ => {}
            }
        }
    });
}

/// Collect all conversations from sidebar models into quick switcher items.
///
/// Aggregates channels/DMs/private-groups from the slint sidebar models into
/// `chatter-core` domain entries, delegates ordering + capping to
/// [`arrange_entries`], then converts to the slint row type.
fn collect_all_items(window: &MainWindow) -> Vec<QuickSwitcherItem> {
    let mut entries: Vec<QuickSwitcherEntry> = Vec::new();

    // Channels
    let channels = window.get_channels();
    for i in 0..channels.row_count() {
        if let Some(ch) = channels.row_data(i) {
            entries.push(QuickSwitcherEntry {
                id: ch.channel_id.to_string(),
                name: ch.name.to_string(),
                conv_type: "channel".into(),
                presence_status: String::new(),
                unread_count: ch.unread_count,
            });
        }
    }

    // DMs
    let dms = window.get_dm_conversations();
    for i in 0..dms.row_count() {
        if let Some(dm) = dms.row_data(i) {
            entries.push(QuickSwitcherEntry {
                id: dm.channel_id.to_string(),
                name: dm.display_name.to_string(),
                conv_type: "dm".into(),
                presence_status: dm.presence_status.to_string(),
                unread_count: dm.unread_count,
            });
        }
    }

    // Private groups
    let groups = window.get_private_groups();
    for i in 0..groups.row_count() {
        if let Some(g) = groups.row_data(i) {
            entries.push(QuickSwitcherEntry {
                id: g.group_id.to_string(),
                name: g.name.to_string(),
                conv_type: "private_group".into(),
                presence_status: String::new(),
                unread_count: g.unread_count,
            });
        }
    }

    // Ordering + cap live in chatter-core (unread-first, then alphabetical).
    arrange_entries(entries)
        .into_iter()
        .map(|e| QuickSwitcherItem {
            id: e.id.into(),
            name: e.name.into(),
            conv_type: e.conv_type.into(),
            presence_status: e.presence_status.into(),
            unread_count: e.unread_count,
        })
        .collect()
}

/// Set the quick switcher items on the window.
fn set_items(window: &MainWindow, items: Vec<QuickSwitcherItem>) {
    let model = VecModel::from(items);
    window.set_quick_switcher_items(ModelRc::new(model));
}
