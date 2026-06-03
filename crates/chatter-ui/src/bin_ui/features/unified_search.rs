//! Unified search wiring — top-bar palette merging conversation navigation
//! ("Jump to") with message-content search ("Messages").
//!
//! Subsumes the former `quick_switcher.rs` and the search half of `header.rs`.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::quick_switcher::QuickSwitcherEntry;
use chatter_core::services::search::filter_conversations;

use crate::{MainWindow, QuickSwitcherItem, SearchResult};

/// Wire all unified-search callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_opened(window);
    wire_changed(window, ui_ctx);
    wire_conversation_selected(window);
    wire_message_selected(window, ui_ctx);
}

/// On open: populate conversations (empty query → recent), clear messages.
fn wire_opened(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_unified_search_opened(move || {
        if let Some(w) = weak.upgrade() {
            let convs = filter_to_items("", &w);
            set_conversations(&w, convs);
            w.set_unified_search_messages(ModelRc::new(VecModel::default()));
        }
    });
}

/// On query change: filter conversations (sync, core) + search messages (bg).
fn wire_changed(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_changed(move |query: SharedString| {
        let query_str = query.to_string();

        // Conversations: synchronous, core-ranked.
        if let Some(w) = weak.upgrade() {
            let convs = filter_to_items(&query_str, &w);
            set_conversations(&w, convs);
        }

        // Messages: empty query clears; otherwise search in background.
        if query_str.trim().is_empty() {
            if let Some(w) = weak.upgrade() {
                w.set_unified_search_messages(ModelRc::new(VecModel::default()));
                w.set_unified_search_searching(false);
            }
            return;
        }

        if let Some(w) = weak.upgrade() {
            w.set_unified_search_searching(true);
        }
        let weak_bg = weak.clone();
        let ui_ctx = ui_ctx.clone();
        std::thread::spawn(move || {
            let results = search_messages(&ui_ctx, &query_str);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_bg.upgrade() {
                    w.set_unified_search_messages(ModelRc::new(VecModel::from(results)));
                    w.set_unified_search_searching(false);
                }
            });
        });
    });
}

/// On conversation pick: navigate (channel/dm/private group).
fn wire_conversation_selected(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_unified_search_conversation_selected(move |id: SharedString, conv_type: SharedString| {
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

/// On message pick: navigate to the message (port of header.rs search_result_clicked).
fn wire_message_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_message_selected(move |message_id: SharedString| {
        let message_id_str = message_id.to_string();
        let weak = weak.clone();
        let ui_ctx = ui_ctx.clone();
        if let Some(w) = weak.upgrade() {
            w.set_loading_messages(true);
        }
        tokio::spawn(async move {
            let msg_id = match message_id_str.parse::<chatter_core::domain::ids::MessageId>() {
                Ok(id) => id,
                Err(e) => { tracing::error!("Invalid message ID: {}", e); return; }
            };
            let message = match ui_ctx.get_message_local(&msg_id) {
                Some(m) => m,
                None => { tracing::error!("Message {} not found locally", msg_id); return; }
            };
            let conv_id_str = message.conversation_id.to_string();
            let conv_type = message.conversation_type;
            let weak_dispatch = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_dispatch.upgrade() {
                    let id_slint: SharedString = conv_id_str.into();
                    use chatter_core::domain::message::ConversationType as CT;
                    match conv_type {
                        CT::Channel => w.invoke_channel_selected(id_slint),
                        CT::ServerDm | CT::MailboxDm | CT::AnonDm => w.invoke_dm_selected(id_slint),
                        CT::AnonGroup => w.invoke_private_group_selected(id_slint),
                        CT::SavedMessages => w.set_loading_messages(false),
                    }
                }
            });
            // Scroll-to-message once the loader populates the model.
            let weak_scroll = weak.clone();
            let target_id = message_id_str.clone();
            tokio::spawn(async move {
                for _ in 0..20 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let target_id = target_id.clone();
                    let weak_inner = weak_scroll.clone();
                    let found = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let found_set = found.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_inner.upgrade() {
                            let messages = w.get_messages();
                            for i in 0..messages.row_count() {
                                if let Some(m) = messages.row_data(i) {
                                    if m.message_id == target_id {
                                        w.set_scroll_to_message_index(i as i32);
                                        found_set.store(true, std::sync::atomic::Ordering::SeqCst);
                                        break;
                                    }
                                }
                            }
                        }
                    });
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    if found.load(std::sync::atomic::Ordering::SeqCst) { break; }
                }
            });
        });
    });
}

// ---- helpers ----

/// Collect conversations from sidebar models, filter+arrange via core.
fn filter_to_items(query: &str, window: &MainWindow) -> Vec<QuickSwitcherItem> {
    let mut entries: Vec<QuickSwitcherEntry> = Vec::new();

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

    filter_conversations(query, entries)
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

fn set_conversations(window: &MainWindow, items: Vec<QuickSwitcherItem>) {
    window.set_unified_search_conversations(ModelRc::new(VecModel::from(items)));
}

/// Port of header.rs message-search shaping. Searches all conversations
/// (unified palette has no per-channel scope toggle).
fn search_messages(ui_ctx: &Arc<ChatterClient>, query: &str) -> Vec<SearchResult> {
    let results = match ui_ctx.search_messages_local(query, None, 50) {
        Ok(r) => r,
        Err(e) => { tracing::error!("Search failed: {}", e); return Vec::new(); }
    };
    let mut out: Vec<SearchResult> = Vec::with_capacity(results.len());
    for result in &results {
        let msg = match ui_ctx.get_message_local(&result.message_id) {
            Some(m) => m,
            None => continue,
        };
        use chatter_core::domain::message::ConversationType as CT;
        let channel_name = match msg.conversation_type {
            CT::Channel | CT::ServerDm | CT::MailboxDm => ui_ctx
                .get_channel_local(&result.conversation_id)
                .map(|c| c.name)
                .unwrap_or_else(|| "Unknown".to_string()),
            CT::AnonDm => result
                .conversation_id
                .to_string()
                .parse::<chatter_core::domain::anon_dm::AnonDmId>()
                .ok()
                .and_then(|id| ui_ctx.get_anon_dm_conversation(&id))
                .and_then(|c| c.peer_display_name)
                .unwrap_or_else(|| "Private Chat".to_string()),
            CT::AnonGroup => result
                .conversation_id
                .to_string()
                .parse::<chatter_core::domain::anon_group::AnonGroupId>()
                .ok()
                .and_then(|id| ui_ctx.get_anon_group(&id))
                .map(|g| g.name)
                .unwrap_or_else(|| "Private Group".to_string()),
            CT::SavedMessages => "Saved Messages".to_string(),
        };
        let sender_name = msg
            .sender_id
            .as_ref()
            .map(|id| ui_ctx.display_name(id))
            .unwrap_or_else(|| "Unknown".to_string());
        let timestamp = msg
            .created_at
            .with_timezone(&chrono::Local)
            .format("%b %d, %H:%M")
            .to_string();
        let content_preview = msg
            .as_text()
            .map(|t| ui_ctx.truncate_message_preview(&t))
            .unwrap_or_else(|| "[Attachment]".to_string());
        out.push(SearchResult {
            message_id: result.message_id.to_string().into(),
            channel_id: result.conversation_id.to_string().into(),
            channel_name: channel_name.into(),
            sender_name: sender_name.into(),
            content_preview: content_preview.into(),
            timestamp: timestamp.into(),
            highlight_start: 0,
            highlight_end: 0,
        });
    }
    out
}
