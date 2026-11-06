//! Username-first friend search (exact @handle) for the Add-friend tab.
//!
//! Wires the Slint search field to core's exact-match `search_users` — the same
//! privacy model invite-member uses (only an exact handle resolves; no
//! enumeration) — and the Add button to `send_friend_request`. Shield/anon
//! forbids username lookup, so the UI hides search there and these stay inert.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use slint::{ComponentHandle, SharedString};

use chatter_core::client::ChatterClient;

use crate::{MainWindow, UserSearchData};

fn first_initial(name: &str) -> String {
    name.trim()
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_default()
}

pub(crate) fn wire_search(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Latest-wins: each keystroke bumps the generation so a slow lookup that
    // lands after a newer one can't overwrite the newer result.
    let generation = Arc::new(AtomicU64::new(0));

    let weak_window = window.as_weak();
    let ui_ctx_search = ui_ctx.clone();
    let gen_search = generation.clone();
    window
        .global::<crate::ModalHostState>()
        .on_friends_search_changed(move |query: SharedString| {
            let trimmed = query.trim().to_string();
            let g = gen_search.fetch_add(1, Ordering::SeqCst) + 1;
            let weak = weak_window.clone();

            if trimmed.is_empty() {
                if let Some(w) = weak.upgrade() {
                    let host = w.global::<crate::ModalHostState>();
                    host.set_friends_search_active(false);
                    host.set_friends_search_performed(false);
                    host.set_friends_search_result(UserSearchData::default());
                }
                return;
            }

            if let Some(w) = weak.upgrade() {
                w.global::<crate::ModalHostState>()
                    .set_friends_search_active(true);
            }

            let ui_ctx = ui_ctx_search.clone();
            let gen_check = gen_search.clone();
            tokio::spawn(async move {
                // Debounce a burst of keystrokes into one lookup.
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                if gen_check.load(Ordering::SeqCst) != g {
                    return; // superseded by a newer keystroke
                }
                let hit = ui_ctx
                    .search_users(&trimmed)
                    .await
                    .ok()
                    .and_then(|mut v| v.pop());
                let _ = slint::invoke_from_event_loop(move || {
                    if gen_check.load(Ordering::SeqCst) != g {
                        return;
                    }
                    if let Some(w) = weak.upgrade() {
                        let host = w.global::<crate::ModalHostState>();
                        host.set_friends_search_active(false);
                        host.set_friends_search_performed(true);
                        let data = match hit {
                            Some(h) => UserSearchData {
                                user_id: h.user_id.to_string().into(),
                                initials: first_initial(&h.display_name).into(),
                                display_name: h.display_name.into(),
                                username: h.username.into(),
                                avatar_source: slint::Image::default(),
                                has_avatar: false,
                                is_friend: h.is_friend,
                                request_sent: false,
                            },
                            None => UserSearchData::default(),
                        };
                        host.set_friends_search_result(data);
                    }
                });
            });
        });

    // Add button → send a friend request (core refuses in anon mode).
    let weak_window = window.as_weak();
    let ui_ctx_add = ui_ctx.clone();
    window
        .global::<crate::ModalHostState>()
        .on_friends_add_searched_friend(move |user_id: SharedString| {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_add.clone();
            let user_id_str = user_id.to_string();
            let note = weak
                .upgrade()
                .map(|w| {
                    w.global::<crate::ModalHostState>()
                        .get_friends_request_note()
                        .to_string()
                })
                .unwrap_or_default();
            tokio::spawn(async move {
                let friend_id = match user_id_str.parse() {
                    Ok(id) => id,
                    Err(_) => {
                        tracing::error!("Invalid user ID: {}", user_id_str);
                        return;
                    }
                };
                let note_opt = {
                    let t = note.trim();
                    if t.is_empty() { None } else { Some(t) }
                };
                match ui_ctx.send_friend_request(&friend_id, note_opt).await {
                    Ok(_) => {
                        tracing::info!("Friend request sent to {}", user_id_str);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let host = w.global::<crate::ModalHostState>();
                                let mut r = host.get_friends_search_result();
                                if r.user_id.to_string() == user_id_str {
                                    r.request_sent = true;
                                    host.set_friends_search_result(r);
                                }
                            }
                        });
                    }
                    Err(e) => {
                        let msg = e.user_message();
                        tracing::warn!("Friend request failed: {}", e);
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.global::<crate::ModalHostState>()
                                    .set_friends_code_status(msg.into());
                            }
                        });
                    }
                }
            });
        });
}
