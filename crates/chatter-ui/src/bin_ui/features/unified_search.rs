//! Unified search wiring — top-bar palette merging conversation navigation
//! ("Jump to") with message-content search ("Messages").
//!
//! Subsumes the former `quick_switcher.rs` and the search half of `header.rs`.

use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::search::{SearchCategory, SearchDocument, SearchQuery};

use crate::{MainWindow, QuickSwitcherItem, SearchResult};

/// Wire all unified-search callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let requests = Arc::new(Mutex::new(SearchRequests::default()));
    let recent_mode = Arc::new(Mutex::new(None));
    wire_session(window, ui_ctx, requests.clone());
    wire_opened(window, ui_ctx, requests.clone(), recent_mode);
    wire_closed(window, ui_ctx, requests.clone());
    wire_changed(window, ui_ctx, requests.clone());
    wire_conversation_selected(window, ui_ctx, requests.clone());
    wire_message_selected(window, ui_ctx, requests.clone());
    wire_resolve(window, ui_ctx, requests.clone());
    wire_person_selected(window, ui_ctx, requests);
    wire_filters(window);
    wire_recents(window);
}

fn wire_filters(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_unified_search_filters_changed(move |_, _, _, _, _| {
        if let Some(w) = weak.upgrade() {
            w.invoke_unified_search_changed(w.get_unified_search_query());
        }
    });
}

fn wire_recents(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_unified_search_recent_selected(move |query| {
        if let Some(w) = weak.upgrade() {
            w.set_unified_search_query(query.clone());
            w.invoke_unified_search_changed(query);
        }
    });
    let weak = window.as_weak();
    window.on_unified_search_recent_clear(move || {
        if let Some(w) = weak.upgrade() {
            w.set_unified_search_recents(ModelRc::new(VecModel::default()));
        }
    });
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
fn wire_opened(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    requests: Arc<Mutex<SearchRequests>>,
    recent_mode: Arc<Mutex<Option<bool>>>,
) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_opened(move || {
        if let Some(w) = weak.upgrade() {
            let mode = ui_ctx.anon_mode_enabled();
            let mut previous_mode = recent_mode.lock().unwrap();
            if previous_mode.is_some_and(|previous| previous != mode) {
                w.set_unified_search_recents(ModelRc::new(VecModel::default()));
            }
            *previous_mode = Some(mode);
            requests
                .lock()
                .unwrap()
                .invalidate(ui_ctx.auth_state().get().session_revision, "");
            clear_search_rows(&w);
            let convs = conversation_items(&ui_ctx, w.get_unified_search_query().as_str());
            set_conversations(&w, convs);
            let count = w.get_unified_search_conversations().row_count() as i32;
            w.set_unified_search_counts(ModelRc::new(VecModel::from(vec![
                count, count, 0, 0, 0, 0, 0,
            ])));
            if !w.get_unified_search_query().trim().is_empty()
                || !matches!(
                    w.get_unified_search_category().as_str(),
                    "all" | "chats" | "messages"
                )
            {
                w.invoke_unified_search_changed(w.get_unified_search_query());
            }
        }
    });
}

/// Account transitions retire both pending requests and already painted rows.
/// Subscribe before spawning so an immediate logout cannot be missed.
fn wire_session(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    requests: Arc<Mutex<SearchRequests>>,
) {
    let weak = window.as_weak();
    let auth = ui_ctx.auth_state().clone();
    let mut events = auth.subscribe();
    let mut revision = auth.get().session_revision;
    tokio::spawn(async move {
        loop {
            if matches!(
                events.recv().await,
                Err(tokio::sync::broadcast::error::RecvError::Closed)
            ) {
                break;
            }
            let next = auth.get().session_revision;
            if revision == next {
                continue;
            }
            revision = next;
            // Invalidate immediately, even before the UI processes the reset.
            requests.lock().unwrap().invalidate(next, "");
            let requests = requests.clone();
            let weak = weak.clone();
            let auth = auth.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if auth.get().session_revision != next {
                    return;
                }
                if let Some(w) = weak.upgrade() {
                    requests.lock().unwrap().invalidate(next, "");
                    clear_search_rows(&w);
                    w.set_unified_search_conversations(ModelRc::new(VecModel::default()));
                    w.set_unified_search_recents(ModelRc::new(VecModel::default()));
                    w.set_unified_search_query("".into());
                    w.set_unified_search_open(false);
                }
            });
        }
    });
}

/// Closing invalidates requests even if the same query is reopened later.
fn wire_closed(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    requests: Arc<Mutex<SearchRequests>>,
) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_closed(move || {
        requests
            .lock()
            .unwrap()
            .invalidate(ui_ctx.auth_state().get().session_revision, "");
        if let Some(w) = weak.upgrade() {
            let query = w.get_unified_search_query().trim().to_string();
            if !query.is_empty() {
                let mut recents = vec![query];
                recents.extend(w.get_unified_search_recents().iter().map(|s| s.to_string()));
                let mut seen = std::collections::HashSet::new();
                recents.retain(|item| seen.insert(item.clone()));
                recents.truncate(6);
                w.set_unified_search_recents(ModelRc::new(VecModel::from(
                    recents
                        .into_iter()
                        .map(SharedString::from)
                        .collect::<Vec<_>>(),
                )));
            }
            clear_search_rows(&w);
        }
    });
}

fn clear_search_rows(w: &MainWindow) {
    w.set_unified_search_messages(ModelRc::new(VecModel::default()));
    w.set_unified_search_counts(ModelRc::new(VecModel::from(vec![0; 7])));
    w.set_unified_search_thumbnails(ModelRc::new(VecModel::default()));
    w.set_unified_search_has_thumbnails(ModelRc::new(VecModel::default()));
    w.set_unified_search_message_kinds(ModelRc::new(VecModel::default()));
    w.set_unified_search_searching(false);
    w.set_unified_search_selected_index(0);
    w.set_unified_search_person_status("".into());
    w.set_unified_search_person_id("".into());
    w.set_unified_search_person_name("".into());
    w.set_unified_search_person_handle("".into());
    w.set_unified_search_person_error("".into());
    w.set_unified_search_query_is_handle(false);
}

/// On query change: filter conversations (sync, core) + search messages (bg).
fn wire_changed(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    requests: Arc<Mutex<SearchRequests>>,
) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_changed(move |query: SharedString| {
        let query_str = query.to_string();
        let ticket = requests
            .lock()
            .unwrap()
            .invalidate(ui_ctx.auth_state().get().session_revision, &query_str);

        // People: flag handle-shaped queries (enables the resolve-on-Enter
        // prompt) and invalidate any prior resolution for the new query.
        if let Some(w) = weak.upgrade() {
            clear_search_rows(&w);
            let person = person_query(&query_str);
            w.set_unified_search_query_is_handle(resolvable_person_query(person));
            w.set_unified_search_person_handle(person.into());
            w.set_unified_search_person_status("".into());
        }

        // Conversations: synchronous, core-built and core-ranked.
        let chat_count = if let Some(w) = weak.upgrade() {
            let all_convs = conversation_items(&ui_ctx, &query_str);
            let count = all_convs.len() as i32;
            let convs = if w.get_unified_search_category().as_str() == "all"
                || w.get_unified_search_category().as_str() == "chats"
            {
                all_convs
            } else {
                Vec::new()
            };
            set_conversations(&w, convs);
            w.set_unified_search_counts(ModelRc::new(VecModel::from(vec![
                count, count, 0, 0, 0, 0, 0,
            ])));
            count
        } else {
            return;
        };

        // Messages: empty query clears; otherwise search in background.
        if weak
            .upgrade()
            .is_some_and(|w| w.get_unified_search_category().as_str() == "chats")
            || (query_str.trim().is_empty()
                && weak.upgrade().is_some_and(|w| {
                    matches!(w.get_unified_search_category().as_str(), "all" | "messages")
                }))
        {
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
        let requests = requests.clone();
        let (category, sort, date) = if let Some(w) = weak.upgrade() {
            (
                w.get_unified_search_category().to_string(),
                w.get_unified_search_sort().to_string(),
                w.get_unified_search_date().to_string(),
            )
        } else {
            return;
        };
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(160));
            if !requests
                .lock()
                .unwrap()
                .pending(ticket, ui_ctx.auth_state().get().session_revision)
            {
                return;
            }
            let (results, mut counts, thumbnails, kinds) =
                search_messages(&ui_ctx, &query_str, &category, &sort, &date);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_bg.upgrade() {
                    let ids = results
                        .iter()
                        .map(|row| row.message_id.to_string())
                        .collect();
                    requests.lock().unwrap().complete_messages(
                        ticket,
                        ui_ctx.auth_state().get().session_revision,
                        w.get_unified_search_open(),
                        ids,
                        || {
                            counts[0] += chat_count;
                            counts[1] = chat_count;
                            w.set_unified_search_counts(ModelRc::new(VecModel::from(
                                counts.to_vec(),
                            )));
                            let mut images = Vec::with_capacity(thumbnails.len());
                            let mut has_images = Vec::with_capacity(thumbnails.len());
                            for thumb in thumbnails {
                                if let Some(thumb) = thumb {
                                    images.push(
                                        slint::Image::from_rgba8(slint::SharedPixelBuffer::<
                                            slint::Rgba8Pixel,
                                        >::clone_from_slice(
                                            &thumb.pixels,
                                            thumb.width,
                                            thumb.height,
                                        )),
                                    );
                                    has_images.push(true);
                                } else {
                                    images.push(slint::Image::default());
                                    has_images.push(false);
                                }
                            }
                            w.set_unified_search_thumbnails(ModelRc::new(VecModel::from(images)));
                            w.set_unified_search_has_thumbnails(ModelRc::new(VecModel::from(
                                has_images,
                            )));
                            w.set_unified_search_message_kinds(ModelRc::new(VecModel::from(
                                kinds
                                    .into_iter()
                                    .map(SharedString::from)
                                    .collect::<Vec<_>>(),
                            )));
                            w.set_unified_search_messages(ModelRc::new(VecModel::from(results)));
                            w.set_unified_search_searching(false);
                        },
                    );
                }
            });
        });
    });
}

/// On Enter over a `nickname.discriminator` query or a pasted
/// `chatter://u/…` username link: exact resolution to a single account.
/// Privacy: exact-match only (no enumeration); a username link decrypts
/// LOCALLY (core `resolve_username_link`) and names its owner directly.
fn wire_resolve(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    requests: Arc<Mutex<SearchRequests>>,
) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_resolve(move |query: SharedString| {
        let handle = person_query(&query).to_string();
        if !resolvable_person_query(&handle) {
            return;
        }
        let Some(w) = weak.upgrade() else { return };
        if !w.get_unified_search_open() || person_query(&w.get_unified_search_query()) != handle {
            return;
        }
        let ticket = requests
            .lock()
            .unwrap()
            .begin_person(ui_ctx.auth_state().get().session_revision, &query);
        {
            w.set_unified_search_person_status("".into());
            w.set_unified_search_person_id("".into());
            w.set_unified_search_person_name("".into());
            w.set_unified_search_person_error("".into());
            w.set_unified_search_person_handle(handle.clone().into());
            w.set_unified_search_person_status("searching".into());
        }
        let weak_bg = weak.clone();
        let ui_ctx = ui_ctx.clone();
        let requests = requests.clone();
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
                    let name = if display.is_empty() {
                        shown_handle.clone()
                    } else {
                        display
                    };
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
                    let row = PersonRow {
                        id,
                        handle: shown_handle,
                        name,
                        status,
                        error,
                    };
                    requests.lock().unwrap().complete_person(
                        ticket,
                        ui_ctx.auth_state().get().session_revision,
                        w.get_unified_search_open(),
                        row,
                        |row| {
                            // Status publishes the row last; all identity fields
                            // belong to this one lookup and this one UI turn.
                            w.set_unified_search_person_status("".into());
                            w.set_unified_search_person_id(row.id.clone().into());
                            w.set_unified_search_person_handle(row.handle.clone().into());
                            w.set_unified_search_person_name(row.name.clone().into());
                            w.set_unified_search_person_error(row.error.clone().into());
                            w.set_unified_search_person_status(row.status.clone().into());
                        },
                    );
                }
            });
        });
    });
}

/// On selecting a resolved person: open their profile modal (where the
/// add-friend / safety-number actions live).
fn wire_person_selected(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    requests: Arc<Mutex<SearchRequests>>,
) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_person_selected(move |uid: SharedString| {
        let Some(w) = weak.upgrade() else { return };
        if !requests.lock().unwrap().person_selectable(
            uid.as_str(),
            ui_ctx.auth_state().get().session_revision,
            w.get_unified_search_open(),
        ) {
            return;
        }
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
fn wire_conversation_selected(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    requests: Arc<Mutex<SearchRequests>>,
) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_conversation_selected(
        move |id: SharedString, conv_type: SharedString| {
            if let Some(w) = weak.upgrade() {
                if !requests.lock().unwrap().session_current(
                    ui_ctx.auth_state().get().session_revision,
                    w.get_unified_search_open(),
                ) || !w
                    .get_unified_search_conversations()
                    .iter()
                    .any(|row| row.id == id && row.conv_type == conv_type)
                {
                    return;
                }
                open_conversation(&w, id, conv_type.as_str());
            }
        },
    );
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
        "mesh" => w
            .global::<crate::MeshGlobal>()
            .invoke_conversation_selected(id),
        // No mesh in this build: the row cannot exist, but a stored message
        // from a mesh-enabled run can. Clearing the spinner is the honest
        // outcome — any view left open would be the wrong conversation.
        _ => w.set_loading_messages(false),
    }
}

/// On message pick: navigate to the message (port of header.rs search_result_clicked).
fn wire_message_selected(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    requests: Arc<Mutex<SearchRequests>>,
) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_message_selected(move |message_id: SharedString| {
        let Some(w) = weak.upgrade() else { return };
        let session = ui_ctx.auth_state().get().session_revision;
        if !requests.lock().unwrap().message_selectable(
            message_id.as_str(),
            session,
            w.get_unified_search_open(),
        ) {
            return;
        }
        // Local lookup and target selection are synchronous: an edit/clear
        // cannot overtake a queued navigation task after the row was clicked.
        let Ok(msg_id) = message_id
            .as_str()
            .parse::<chatter_core::domain::ids::MessageId>()
        else {
            return;
        };
        let Some(message) = ui_ctx.get_message_local(&msg_id) else {
            return;
        };
        let (conv_type, conv_id) = ui_ctx.search_hit_target(&message);
        w.set_loading_messages(true);
        open_conversation(&w, conv_id.into(), conv_type);
        jump_once_open(
            w.as_weak(),
            ui_ctx.clone(),
            message_id.to_string(),
            session,
            50,
        );
    });
}

/// Jump to `message_id` once the conversation that holds it has finished
/// opening — every open path lowers `loading-messages` when its page is in.
/// Polled (100 ms, bounded) because the open paths have no completion hook.
fn jump_once_open(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    message_id: String,
    session: u64,
    tries: u32,
) {
    slint::Timer::single_shot(std::time::Duration::from_millis(100), move || {
        let Some(w) = weak.upgrade() else { return };
        if ui_ctx.auth_state().get().session_revision != session {
            return;
        }
        if !w.get_loading_messages() {
            crate::bin_ui::features::header::jump_to_message(&w, &ui_ctx, &message_id);
        } else if tries > 0 {
            jump_once_open(weak, ui_ctx, message_id, session, tries - 1);
        }
    });
}

#[path = "unified_search_requests.rs"]
mod requests;
use requests::{PersonRow, SearchRequests};

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
struct SearchHit {
    row: SearchResult,
    text: String,
    category: SearchCategory,
    created_at: chrono::DateTime<chrono::Utc>,
    size: u64,
    sender_id: Option<String>,
    chat_id: String,
    thumbnail: Option<Vec<u8>>,
}

struct SearchThumbnail {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

impl SearchDocument for SearchHit {
    fn category(&self) -> SearchCategory {
        self.category
    }
    fn search_text(&self) -> &str {
        &self.text
    }
    fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        self.created_at
    }
    fn size_bytes(&self) -> u64 {
        self.size
    }
    fn sender_id(&self) -> Option<&str> {
        self.sender_id.as_deref()
    }
    fn chat_id(&self) -> &str {
        &self.chat_id
    }
}

fn search_messages(
    ui_ctx: &Arc<ChatterClient>,
    query: &str,
    category: &str,
    sort: &str,
    date: &str,
) -> (
    Vec<SearchResult>,
    [i32; 7],
    Vec<Option<SearchThumbnail>>,
    Vec<String>,
) {
    let results = ui_ctx.search_messages_visible(query, 250);
    let mut candidates = Vec::with_capacity(results.len());
    for msg in &results {
        // Conversation name + sender name resolution both live in core.
        let channel_name =
            ui_ctx.conversation_display_name(&msg.conversation_id, &msg.conversation_type);
        let sender_name = match msg.sender_id {
            Some(_) => ui_ctx.message_sender_display_name(&msg),
            None => "Unknown".to_string(),
        };
        let sent_at = chrono::DateTime::<chrono::Utc>::from_timestamp_nanos(msg.sent_at as i64);
        let timestamp = sent_at
            .with_timezone(&chrono::Local)
            .format("%b %d, %H:%M")
            .to_string();
        let attachment = msg
            .attachments
            .first()
            .and_then(|id| ui_ctx.get_attachment_metadata(id));
        let (attachment_name, size) = if let Some(file) = attachment.as_ref() {
            let meta = &file.metadata;
            (meta.name.clone(), meta.size)
        } else {
            (String::new(), 0)
        };
        let kind = SearchCategory::from_message(
            attachment
                .as_ref()
                .map(|file| file.metadata.mime_type.as_str()),
            attachment
                .as_ref()
                .is_some_and(|file| file.metadata.waveform_samples.is_some()),
            msg.as_text(),
        );
        let content_preview = msg
            .as_text()
            .map(|t| ui_ctx.truncate_message_preview(&t))
            .unwrap_or_else(|| attachment_name.clone());
        let lower = content_preview.to_lowercase();
        let needle = query.trim().to_lowercase();
        let start = lower.find(&needle).unwrap_or(0) as i32;
        let end = if lower.contains(&needle) {
            start + needle.len() as i32
        } else {
            0
        };
        candidates.push(SearchHit {
            row: SearchResult {
                message_id: msg.id.to_string().into(),
                channel_id: msg.conversation_id.to_string().into(),
                channel_name: channel_name.into(),
                sender_name: sender_name.into(),
                content_preview: content_preview.into(),
                timestamp: timestamp.into(),
                highlight_start: start,
                highlight_end: end,
            },
            text: format!("{} {}", msg.as_text().unwrap_or(""), attachment_name),
            category: kind,
            created_at: sent_at,
            size,
            sender_id: msg.sender_id.map(|id| id.to_string()),
            chat_id: msg.conversation_id.to_string(),
            thumbnail: attachment
                .as_ref()
                .and_then(|file| file.metadata.thumbnail.clone()),
        });
    }
    let request = SearchQuery::from_controls(query, category, sort, date);
    let mut all_request = request.clone();
    all_request.category = SearchCategory::All;
    if request.category != SearchCategory::Chats {
        let mut seen: std::collections::HashSet<String> = candidates
            .iter()
            .map(|hit| hit.row.message_id.to_string())
            .collect();
        for item in ui_ctx.search_shared_content_visible(&all_request, 250) {
            if !seen.insert(item.message_id.clone()) {
                continue;
            }
            let Ok(id) = item.message_id.parse() else {
                continue;
            };
            let Some(msg) = ui_ctx.get_message_local(&id) else {
                continue;
            };
            let channel_name =
                ui_ctx.conversation_display_name(&msg.conversation_id, &msg.conversation_type);
            let sender_name = if msg.sender_id.is_some() {
                ui_ctx.message_sender_display_name(&msg)
            } else {
                "Unknown".to_string()
            };
            let category = SearchCategory::from_shared_kind(&item.category);
            let sent_at = chrono::DateTime::<chrono::Utc>::from_timestamp_nanos(msg.sent_at as i64);
            let label = if item.size_bytes > 0 {
                format!(
                    "{} · {}",
                    item.title,
                    chatter_core::domain::attachment::format_bytes(item.size_bytes)
                )
            } else {
                item.title.clone()
            };
            let needle = query.trim().to_lowercase();
            let match_start = label.to_lowercase().find(&needle);
            candidates.push(SearchHit {
                row: SearchResult {
                    message_id: item.message_id.into(),
                    channel_id: msg.conversation_id.to_string().into(),
                    channel_name: channel_name.into(),
                    sender_name: sender_name.into(),
                    content_preview: label.into(),
                    timestamp: sent_at
                        .with_timezone(&chrono::Local)
                        .format("%b %d, %H:%M")
                        .to_string()
                        .into(),
                    highlight_start: match_start.unwrap_or(0) as i32,
                    highlight_end: match_start.map_or(0, |start| start + needle.len()) as i32,
                },
                text: item.title,
                category,
                created_at: sent_at,
                size: item.size_bytes,
                sender_id: msg.sender_id.map(|id| id.to_string()),
                chat_id: msg.conversation_id.to_string(),
                thumbnail: item.thumbnail,
            });
        }
    }
    let candidates = all_request.apply(candidates);
    let mut counts = [0; 7];
    for hit in &candidates {
        counts[0] += 1;
        let slot = match hit.category {
            SearchCategory::Messages => 2,
            SearchCategory::Media => 3,
            SearchCategory::Files => 4,
            SearchCategory::Links => 5,
            SearchCategory::Voice => 6,
            _ => continue,
        };
        counts[slot] += 1;
    }
    let mut rows = Vec::new();
    let mut thumbnails = Vec::new();
    let mut kinds = Vec::new();
    for hit in request.apply(candidates).into_iter().take(50) {
        let decoded = hit
            .thumbnail
            .as_deref()
            .and_then(|bytes| image::load_from_memory(bytes).ok())
            .map(|image| image.thumbnail(320, 320).to_rgba8())
            .map(|rgba| SearchThumbnail {
                width: rgba.width(),
                height: rgba.height(),
                pixels: rgba.into_raw(),
            });
        thumbnails.push(decoded);
        kinds.push(hit.category.as_str().to_string());
        rows.push(hit.row);
    }
    (rows, counts, thumbnails, kinds)
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
