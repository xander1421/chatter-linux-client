//! Unified search wiring — top-bar palette merging conversation navigation
//! ("Jump to") with message-content search ("Messages").
//!
//! Subsumes the former `quick_switcher.rs` and the search half of `header.rs`.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;

use crate::{MainWindow, QuickSwitcherItem, SearchResult, run_ui};

/// Wire all unified-search callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_opened(window, ui_ctx);
    wire_changed(window, ui_ctx);
    wire_conversation_selected(window);
    wire_message_selected(window, ui_ctx);
    wire_resolve(window, ui_ctx);
    wire_person_selected(window, ui_ctx);
}

/// The person-lookup form of a query: trimmed, one leading `@` dropped.
/// People type handles the way the app shows them (`@nick.123`); the `@` is
/// display decoration, not part of the handle the lookup hashes.
fn person_query(q: &str) -> &str {
    let q = q.trim();
    q.strip_prefix('@').unwrap_or(q)
}

/// True when `q` is resolvable to a person: a full handle OR a pasted
/// `chatter://u/…` username link (shape check lives in core).
fn resolvable_person_query(q: &str) -> bool {
    chatter_core::services::user::is_lookup_handle(q)
        || chatter_core::domain::UsernameLink::is_username_link(q)
}

/// On open: populate conversations (empty query → recent), clear messages.
fn wire_opened(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_opened(move || {
        if let Some(w) = weak.upgrade() {
            let convs = conversation_items(&ui_ctx, "");
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

        // People: flag handle-shaped queries (enables the resolve-on-Enter
        // prompt) and invalidate any prior resolution for the new query.
        if let Some(w) = weak.upgrade() {
            let person = person_query(&query_str);
            w.set_unified_search_query_is_handle(resolvable_person_query(person));
            w.set_unified_search_person_handle(person.into());
            w.set_unified_search_person_status("".into());
        }

        // Conversations: synchronous, core-built and core-ranked.
        if let Some(w) = weak.upgrade() {
            let convs = conversation_items(&ui_ctx, &query_str);
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

/// On Enter over a `nickname.discriminator` query or a pasted
/// `chatter://u/…` username link: exact resolution to a single account.
/// Privacy: exact-match only (no enumeration); a username link decrypts
/// LOCALLY (core `resolve_username_link`) and names its owner directly.
fn wire_resolve(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_resolve(move |query: SharedString| {
        let handle = person_query(&query).to_string();
        if !resolvable_person_query(&handle) {
            return;
        }
        if let Some(w) = weak.upgrade() {
            w.set_unified_search_person_handle(handle.clone().into());
            w.set_unified_search_person_status("searching".into());
        }
        let weak_bg = weak.clone();
        let ui_ctx = ui_ctx.clone();
        tokio::spawn(async move {
            // Username link → (decrypted handle, owner account); plain handle
            // → hashed lookup, which a Nobody account answers as "no match".
            let is_link = chatter_core::domain::UsernameLink::is_username_link(&handle);
            let (shown_handle, looked_up) = if is_link {
                match ui_ctx.resolve_username_link(&handle).await {
                    Ok((username, uid)) => (username, Ok(Some(uid))),
                    Err(e) => (handle.clone(), Err(e)),
                }
            } else {
                (handle.clone(), ui_ctx.lookup_by_username(&handle).await)
            };
            let mut error = String::new();
            let (status, id, name) = match looked_up {
                Ok(Some(uid)) => {
                    let display = ui_ctx
                        .get_user_profile(&uid)
                        .await
                        .map(|p| p.display_name)
                        .unwrap_or_default();
                    let name = if display.is_empty() { shown_handle.clone() } else { display };
                    ("found".to_string(), uid.to_string(), name)
                }
                Ok(None) => ("notfound".to_string(), String::new(), String::new()),
                // Not "No user": a refusal (Shield) or a failure is not an
                // answer about the handle. Core words it.
                Err(e) => {
                    error = e.user_message();
                    ("error".to_string(), String::new(), String::new())
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_bg.upgrade() {
                    w.set_unified_search_person_id(id.into());
                    w.set_unified_search_person_name(name.into());
                    w.set_unified_search_person_error(error.into());
                    w.set_unified_search_person_status(status.into());
                }
            });
        });
    });
}

/// On selecting a resolved person: open their profile modal (where the
/// add-friend / safety-number actions live).
fn wire_person_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_person_selected(move |uid: SharedString| {
        let user_id = match uid.to_string().parse::<chatter_core::domain::ids::UserId>() {
            Ok(id) => id,
            Err(_) => return,
        };
        if let Some(w) = weak.upgrade() {
            crate::bin_ui::features::modals::open_user_profile(
                &w,
                &user_id,
                ui_ctx.auth_state(),
                ui_ctx.user_state(),
                Some(&ui_ctx),
            );
            crate::bin_ui::features::back::push(&w, "user-profile");
        }
    });
}

/// On conversation pick: navigate (channel/dm/private group).
fn wire_conversation_selected(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_unified_search_conversation_selected(move |id: SharedString, conv_type: SharedString| {
        if let Some(w) = weak.upgrade() {
            open_conversation(&w, id, conv_type.as_str());
        }
    });
}

/// Open a conversation by its palette `(id, conv_type)` — the one way a
/// jump-to row and a message hit (core `search_hit_target`) navigate.
fn open_conversation(w: &MainWindow, id: SharedString, conv_type: &str) {
    match conv_type {
        "channel" => w.invoke_channel_selected(id),
        "dm" | "anon_dm" => w.invoke_dm_selected(id),
        "private_group" => w.invoke_private_group_selected(id),
        "saved_messages" => w.invoke_saved_messages_clicked(),
        // Mesh rows open through the same global callback the sidebar
        // uses, so there is one way into a mesh conversation.
        #[cfg(feature = "mesh")]
        "mesh" => w.global::<crate::MeshGlobal>().invoke_conversation_selected(id),
        // No mesh in this build: the row cannot exist, but a stored message
        // from a mesh-enabled run can. Clearing the spinner is the honest
        // outcome — any view left open would be the wrong conversation.
        _ => w.set_loading_messages(false),
    }
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
                Err(e) => {
                    tracing::error!("Invalid message ID: {}", e);
                    run_ui!(weak, |w| w.set_loading_messages(false));
                    return;
                }
            };
            let message = match ui_ctx.get_message_local(&msg_id) {
                Some(m) => m,
                None => {
                    tracing::error!("Message {} not found locally", msg_id);
                    run_ui!(weak, |w| w.set_loading_messages(false));
                    return;
                }
            };
            let (conv_type, conv_id) = ui_ctx.search_hit_target(&message);
            run_ui!(weak, |w| open_conversation(&w, conv_id.into(), conv_type));
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

/// Jump-to rows for `query`: core builds them from the mode-filtered sidebar
/// sources (folders included) and ranks them; this only converts.
fn conversation_items(ui_ctx: &ChatterClient, query: &str) -> Vec<QuickSwitcherItem> {
    ui_ctx
        .quick_switcher_entries(query)
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

/// Port of header.rs message-search shaping. Searches every conversation the
/// current mode shows (core filters; the palette has no per-channel scope).
fn search_messages(ui_ctx: &Arc<ChatterClient>, query: &str) -> Vec<SearchResult> {
    let results = ui_ctx.search_messages_visible(query, 50);
    let mut out: Vec<SearchResult> = Vec::with_capacity(results.len());
    for msg in &results {
        // Conversation name + sender name resolution both live in core.
        let channel_name =
            ui_ctx.conversation_display_name(&msg.conversation_id, &msg.conversation_type);
        let sender_name = match msg.sender_id {
            Some(_) => ui_ctx.message_sender_display_name(&msg),
            None => "Unknown".to_string(),
        };
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
            message_id: msg.id.to_string().into(),
            channel_id: msg.conversation_id.to_string().into(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn person_query_drops_one_leading_at() {
        assert_eq!(person_query(" @dev13883.75 "), "dev13883.75");
        assert_eq!(person_query("dev13883.75"), "dev13883.75");
        assert!(resolvable_person_query(person_query("@dev13883.75")));
    }
}
