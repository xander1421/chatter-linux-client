//! Unified search wiring — top-bar palette merging conversation navigation
//! ("Jump to") with message-content search ("Messages").
//!
//! Subsumes the former `quick_switcher.rs` and the search half of `header.rs`.

use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::services::search::{SearchQuery, preview_fragments};

use crate::{CalendarDay, MainWindow, QuickSwitcherItem, SearchResult};

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
    wire_recents(window, ui_ctx);
    wire_calendar(window);
    wire_picker(window, ui_ctx);
    wire_more(window);
}

fn wire_more(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_unified_search_show_more(move |section| {
        let Some(w) = weak.upgrade() else { return };
        let slot = match section.as_str() {
            "messages" => 2,
            "media" => 3,
            "files" => 4,
            "links" => 5,
            "voice" => 6,
            _ => return,
        };
        let mut limits = w
            .get_unified_search_page_limits()
            .iter()
            .collect::<Vec<_>>();
        if limits.len() != 7 {
            return;
        }
        limits[slot] = (limits[slot] + 50).min(500);
        w.set_unified_search_page_limits(ModelRc::new(VecModel::from(limits)));
        w.invoke_unified_search_changed(w.get_unified_search_query());
    });
}

fn wire_picker(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_picker_request(move |kind, query| {
        let Some(w) = weak.upgrade() else { return };
        if !w.get_unified_search_open() {
            return;
        }
        let rows = ui_ctx
            .search_filter_options(kind.as_str(), query.as_str())
            .into_iter()
            .map(|option| {
                let avatar = option
                    .avatar
                    .as_deref()
                    .and_then(crate::bin_ui::converters::avatar_bytes_to_image);
                QuickSwitcherItem {
                    id: option.id.into(),
                    name: option.name.into(),
                    conv_type: option.kind.into(),
                    presence_status: "".into(),
                    unread_count: 0,
                    has_avatar: avatar.is_some(),
                    avatar: avatar.unwrap_or_default(),
                }
            })
            .collect::<Vec<_>>();
        w.set_unified_search_picker_options(ModelRc::new(VecModel::from(rows)));
    });
}

fn wire_calendar(window: &MainWindow) {
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    let month = Arc::new(Mutex::new(
        chrono::NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap(),
    ));
    let weak = window.as_weak();
    window.on_unified_search_calendar_shift(move |delta| {
        let Some(w) = weak.upgrade() else { return };
        let mut current = month.lock().unwrap();
        if delta != 0 {
            let index = current.year() * 12 + current.month0() as i32 + delta;
            let year = index.div_euclid(12);
            let month = index.rem_euclid(12) as u32 + 1;
            let Some(next) = chrono::NaiveDate::from_ymd_opt(year, month, 1) else {
                return;
            };
            *current = next;
        }
        w.set_unified_search_calendar_month(current.format("%B %Y").to_string().into());
        let offset = current.weekday().num_days_from_monday() as i64;
        let first = *current - chrono::Duration::days(offset);
        let days = (0..42)
            .map(|i| {
                let date = first + chrono::Duration::days(i);
                CalendarDay {
                    date: date.format("%Y-%m-%d").to_string().into(),
                    label: date.day().to_string().into(),
                    in_month: date.month() == current.month(),
                }
            })
            .collect::<Vec<_>>();
        w.set_unified_search_calendar_days(ModelRc::new(VecModel::from(days)));
    });
}

fn wire_filters(window: &MainWindow) {
    let weak = window.as_weak();
    window.on_unified_search_filters_changed(move |_, _, _, _, _| {
        if let Some(w) = weak.upgrade() {
            w.set_unified_search_page_limits(ModelRc::new(VecModel::from(vec![50; 7])));
            w.invoke_unified_search_changed(w.get_unified_search_query());
        }
    });
}

fn set_recents(window: &MainWindow, rows: Vec<String>) {
    window.set_unified_search_recents(ModelRc::new(VecModel::from(
        rows.into_iter().map(SharedString::from).collect::<Vec<_>>(),
    )));
}

fn wire_recents(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    window.on_unified_search_recent_selected(move |query| {
        if let Some(w) = weak.upgrade() {
            w.set_unified_search_query(query.clone());
            w.invoke_unified_search_changed(query);
        }
    });
    let weak = window.as_weak();
    let clear_ctx = ui_ctx.clone();
    window.on_unified_search_recent_clear(move || {
        if let Some(w) = weak.upgrade() {
            set_recents(&w, clear_ctx.clear_recent_searches());
        }
    });
    let weak = window.as_weak();
    let ui_ctx = ui_ctx.clone();
    window.on_unified_search_recent_remove(move |query| {
        if let Some(w) = weak.upgrade() {
            set_recents(&w, ui_ctx.remove_recent_search(query.as_str()));
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
            w.set_unified_search_page_limits(ModelRc::new(VecModel::from(vec![50; 7])));
            let mut previous_mode = recent_mode.lock().unwrap();
            if previous_mode.is_some_and(|previous| previous != mode) {
                w.set_unified_search_recents(ModelRc::new(VecModel::default()));
                w.set_unified_search_from("".into());
                w.set_unified_search_in("".into());
                w.set_unified_search_from_label("".into());
                w.set_unified_search_in_label("".into());
            }
            *previous_mode = Some(mode);
            set_recents(&w, ui_ctx.recent_searches());
            requests
                .lock()
                .unwrap()
                .invalidate(ui_ctx.auth_snapshot().session_revision, "");
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
    let auth = ui_ctx.clone();
    let mut events = auth.subscribe_auth_snapshots();
    let mut revision = auth.auth_snapshot().session_revision;
    tokio::spawn(async move {
        loop {
            if matches!(
                events.recv().await,
                Err(tokio::sync::broadcast::error::RecvError::Closed)
            ) {
                break;
            }
            let next = auth.auth_snapshot().session_revision;
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
                if auth.auth_snapshot().session_revision != next {
                    return;
                }
                if let Some(w) = weak.upgrade() {
                    requests.lock().unwrap().invalidate(next, "");
                    clear_search_rows(&w);
                    w.set_unified_search_conversations(ModelRc::new(VecModel::default()));
                    w.set_unified_search_recents(ModelRc::new(VecModel::default()));
                    w.set_unified_search_query("".into());
                    w.set_unified_search_from("".into());
                    w.set_unified_search_in("".into());
                    w.set_unified_search_from_label("".into());
                    w.set_unified_search_in_label("".into());
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
            .invalidate(ui_ctx.auth_snapshot().session_revision, "");
        if let Some(w) = weak.upgrade() {
            let query = w.get_unified_search_query().trim().to_string();
            if !query.is_empty() {
                set_recents(&w, ui_ctx.remember_search(&query));
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
        if let Some(w) = weak.upgrade() {
            if !requests.lock().unwrap().same_query(&query_str) {
                w.set_unified_search_page_limits(ModelRc::new(VecModel::from(vec![50; 7])));
            }
        }
        let ticket = requests
            .lock()
            .unwrap()
            .invalidate(ui_ctx.auth_snapshot().session_revision, &query_str);

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
        let (category, sort, date, from, in_chat, page_limits) = if let Some(w) = weak.upgrade() {
            (
                w.get_unified_search_category().to_string(),
                w.get_unified_search_sort().to_string(),
                w.get_unified_search_date().to_string(),
                w.get_unified_search_from().to_string(),
                w.get_unified_search_in().to_string(),
                w.get_unified_search_page_limits()
                    .iter()
                    .collect::<Vec<_>>(),
            )
        } else {
            return;
        };
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(160));
            if !requests
                .lock()
                .unwrap()
                .pending(ticket, ui_ctx.auth_snapshot().session_revision)
            {
                return;
            }
            let (results, mut counts, thumbnails, kinds) = search_messages(
                &ui_ctx,
                &query_str,
                &category,
                &sort,
                &date,
                &from,
                &in_chat,
                &page_limits,
            );
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_bg.upgrade() {
                    let ids = results
                        .iter()
                        .map(|row| row.message_id.to_string())
                        .collect();
                    requests.lock().unwrap().complete_messages(
                        ticket,
                        ui_ctx.auth_snapshot().session_revision,
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
            .begin_person(ui_ctx.auth_snapshot().session_revision, &query);
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
                        ui_ctx.auth_snapshot().session_revision,
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
            ui_ctx.auth_snapshot().session_revision,
            w.get_unified_search_open(),
        ) {
            return;
        }
        let user_id = match uid.to_string().parse::<chatter_core::domain::ids::UserId>() {
            Ok(id) => id,
            Err(_) => return,
        };
        if let Some(w) = weak.upgrade() {
            crate::bin_ui::features::modals::open_user_profile(&w, &user_id, Some(&ui_ctx));
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
                    ui_ctx.auth_snapshot().session_revision,
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
        let session = ui_ctx.auth_snapshot().session_revision;
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
        if ui_ctx.auth_snapshot().session_revision != session {
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
            avatar: slint::Image::default(),
            has_avatar: false,
        })
        .collect()
}

fn set_conversations(window: &MainWindow, items: Vec<QuickSwitcherItem>) {
    window.set_unified_search_conversations(ModelRc::new(VecModel::from(items)));
}

/// Port of header.rs message-search shaping. Searches every conversation the
/// current mode shows (core filters; the palette has no per-channel scope).
struct SearchThumbnail {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

fn search_messages(
    ui_ctx: &Arc<ChatterClient>,
    query: &str,
    category: &str,
    sort: &str,
    date: &str,
    from: &str,
    in_chat: &str,
    page_limits: &[i32],
) -> (
    Vec<SearchResult>,
    [i32; 7],
    Vec<Option<SearchThumbnail>>,
    Vec<String>,
) {
    let mut request = SearchQuery::from_controls(query, category, sort, date);
    request.from = (!from.is_empty()).then(|| from.to_owned());
    request.in_chat = (!in_chat.is_empty()).then(|| in_chat.to_owned());
    let page = ui_ctx.search_visible(request, page_limits);
    let mut rows = Vec::with_capacity(page.hits.len());
    let mut thumbnails = Vec::with_capacity(page.hits.len());
    let mut kinds = Vec::with_capacity(page.hits.len());
    for hit in page.hits {
        let (before, matched, after, start, end) = preview_fragments(&hit.preview, query);
        rows.push(SearchResult {
            message_id: hit.message_id.into(),
            channel_id: hit.channel_id.into(),
            channel_name: hit.channel_name.into(),
            sender_name: hit.sender_name.into(),
            content_preview: hit.preview.into(),
            timestamp: hit
                .created_at
                .with_timezone(&chrono::Local)
                .format("%b %d, %H:%M")
                .to_string()
                .into(),
            highlight_start: start,
            highlight_end: end,
            preview_before: before.into(),
            preview_match: matched.into(),
            preview_after: after.into(),
        });
        thumbnails.push(
            hit.thumbnail
                .as_deref()
                .and_then(|bytes| image::load_from_memory(bytes).ok())
                .map(|image| image.thumbnail(320, 320).to_rgba8())
                .map(|rgba| SearchThumbnail {
                    width: rgba.width(),
                    height: rgba.height(),
                    pixels: rgba.into_raw(),
                }),
        );
        kinds.push(hit.category.as_str().to_string());
    }
    (rows, page.counts, thumbnails, kinds)
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
