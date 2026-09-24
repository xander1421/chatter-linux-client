//! Profile modal callbacks.
//!
//! Uses UserService for all profile operations - no direct network calls.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;

use crate::bin_ui::converters::get_initials;
use crate::{MainWindow, SharedContentItem, SharedTab};

/// Bytes of an avatar picked in the Edit Profile modal, held until Save.
///
/// NOT a `thread_local!`: the picker runs on a tokio worker and Save runs on
/// another, so a thread-local would race-lose the value (the same reason
/// `create_channel.rs` stashes its icon path on a Slint property).
///
/// The picker used to call `set_avatar_bytes` directly, which wrote
/// `keys::AVATAR` immediately — so Cancel left the new avatar in place and the
/// next profile update uploaded it. Verified on device: pick, Cancel, and the
/// bottom-nav avatar had already changed (§112v).
static PENDING_AVATAR_BYTES: std::sync::Mutex<Option<Vec<u8>>> = std::sync::Mutex::new(None);

thread_local! {
    static PENDING_AVATAR_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    /// Paging cursor for the profile's shared-content drawer. UI-thread only —
    /// every reader and writer runs inside the Slint event loop.
    static SHARED_PAGE: RefCell<SharedPage> = RefCell::new(SharedPage::default());
}

/// How many items one page of a shared-content tab pulls. Big enough that a
/// 3-column grid fills several screens, small enough that decoding that many
/// thumbnails does not stall a frame.
const SHARED_PAGE_SIZE: u32 = 60;

/// Where the open profile's shared-content list has got to.
///
/// `exhausted` is what stops the Flickable's scroll callback from asking for
/// page after page at the bottom of a finished list, and `in_flight` stops it
/// from asking again while the previous request is still running.
#[derive(Default)]
struct SharedPage {
    peer: Option<UserId>,
    tab: String,
    loaded: usize,
    exhausted: bool,
    in_flight: bool,
}

/// Open the user profile modal with all properties populated correctly.
///
/// Handles both "self" (own profile) and "other user" paths, ensuring
/// consistent data regardless of which entry point triggered the modal.
pub fn open_user_profile(
    w: &MainWindow,
    user_id: &UserId,
    auth_state: &chatter_core::state::AuthState,
    user_state: &chatter_core::state::UserState,
    ui_ctx: Option<&Arc<ChatterClient>>,
) {
    let is_self = auth_state.get().current_user_id.as_ref() == Some(user_id);

    w.set_profile_user_id(user_id.to_string().into());
    // Reset safety number state for fresh modal
    w.set_profile_safety_number("".into());
    w.set_profile_show_safety_number(false);

    // Reset username-link share state; re-populated below for self.
    w.set_profile_share_link("".into());
    w.set_profile_share_status("".into());

    if is_self {
        let state = auth_state.get();
        let display_name = state.display_name.clone().unwrap_or_default();
        // The handle chosen at registration. Never the user id: that is not
        // something anyone can look you up by.
        let username = ui_ctx
            .and_then(|ctx| ctx.full_username())
            .or_else(|| user_state.get_user(user_id).map(|u| u.username.clone()))
            .filter(|u| !u.is_empty())
            .unwrap_or_default();
        let initials = get_initials(&display_name);

        w.set_profile_display_name(display_name.into());
        w.set_profile_username(username.into());
        w.set_profile_initials(initials.into());
        w.set_profile_avatar_source(w.get_user_avatar_source());
        w.set_profile_presence(w.get_user_presence_status());

        // Fall back to local storage. UserState is populated by profile SYNC,
        // which covers friends — nobody broadcasts your own profile back to
        // you, so for `is_self` the in-memory entry is normally absent and the
        // bio silently read as empty after every restart, even though
        // `update_user_profile` had persisted it via `user_repo.upsert_profile`
        // (OPEN-ITEMS §112h). The display name never showed the bug because it
        // comes from auth_state instead.
        let own_profile = user_state
            .get_profile(user_id)
            .or_else(|| ui_ctx.and_then(|c| c.get_profile_local(user_id)));
        match own_profile.and_then(|p| p.status_message) {
            Some(msg) => w.set_profile_bio(msg.text.into()),
            None => w.set_profile_bio("".into()),
        }

        w.set_profile_is_self(true);
        w.set_profile_is_friend(false);
        w.set_profile_is_blocked(false);
        w.set_profile_is_verified(false);

        // Show the existing username link (local copy) without a network
        // round-trip; creating/rotating goes through the wired callbacks.
        if let Some(ctx) = ui_ctx {
            if let Ok(Some(url)) = ctx.username_link_local() {
                set_share_link_props(w, ctx, &url);
            }
        }
    } else {
        // Other-user path
        let profile = user_state.get_profile(user_id);

        // Core resolves names from the local user directory. The in-memory
        // UserState used here before is not populated for peers, so a friend's
        // profile read "5ea4c7b3" while the Friends list showed their name.
        let display_name = match ui_ctx {
            Some(ctx) => ctx.display_name(user_id),
            None => profile
                .as_ref()
                .map(|p| p.display_name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_default(),
        };

        // A peer's handle is not known locally (the server stores only its
        // lookup hash); show none rather than the user id dressed up as one.
        let username = ui_ctx.and_then(|ctx| ctx.known_username(user_id)).unwrap_or_default();

        let initials = get_initials(&display_name);

        let presence = profile.as_ref()
            .map(|p| p.presence.as_str())
            .unwrap_or("offline");

        // From core's local profile, like the avatar below: `user_state` is not
        // populated for peers, so the bio never showed.
        let bio = ui_ctx
            .and_then(|ctx| ctx.get_profile_local(user_id))
            .and_then(|p| p.status_message)
            .map(|m| m.text)
            .unwrap_or_default();

        let user_data = user_state.get();
        let is_friend = user_data.friends.contains(user_id)
            || ui_ctx.map(|ctx| ctx.is_friend(user_id)).unwrap_or(false);
        let is_blocked = ui_ctx.map(|ctx| ctx.is_profile_user_blocked(user_id)).unwrap_or(false);

        w.set_profile_display_name(display_name.into());
        w.set_profile_username(username.into());
        w.set_profile_initials(initials.into());
        // Render the peer's avatar from their decrypted server-profile bytes
        // (no URL); falls back to initials when we have none.
        let avatar_source = ui_ctx
            .and_then(|ctx| ctx.get_profile_local(user_id))
            .and_then(|p| p.avatar_bytes)
            .as_deref()
            .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
            .unwrap_or_default();
        w.set_profile_avatar_source(avatar_source);
        w.set_profile_presence(presence.into());
        w.set_profile_bio(bio.into());
        // Check verification state
        let is_verified = ui_ctx
            .and_then(|ctx| ctx.is_peer_verified(user_id).ok())
            .unwrap_or(false);

        w.set_profile_is_self(false);
        w.set_profile_is_friend(is_friend);
        w.set_profile_is_blocked(is_blocked);
        w.set_profile_is_verified(is_verified);
    }

    // Shared-content drawer. Reset first so the previous profile's media never
    // flashes under the new name, then load this conversation's tabs.
    w.set_profile_shared_tabs(ModelRc::new(VecModel::<SharedTab>::from(vec![])));
    w.set_profile_shared_items(ModelRc::new(VecModel::<SharedContentItem>::from(vec![])));
    w.set_profile_shared_tab("".into());
    w.set_profile_last_seen("".into());
    w.set_profile_is_muted(false);
    SHARED_PAGE.with(|p| *p.borrow_mut() = SharedPage { peer: Some(*user_id), ..Default::default() });

    if !is_self {
        if let Some(ctx) = ui_ctx {
            // A peer's name/bio/avatar edit is not pushed reliably; refetch on
            // open (throttled in core) so the modal catches up via
            // UserProfileUpdated.
            let (refresh_ctx, peer) = (ctx.clone(), *user_id);
            tokio::spawn(async move { refresh_ctx.refresh_profile_if_stale(&peer).await });
            // Loading from the first frame: an empty strip is also what "no
            // tabs" looks like, and flashing "Nothing shared yet" for the
            // length of one query reads as a bug.
            w.set_profile_shared_loading(true);
            w.set_profile_last_seen(ctx.peer_last_seen_label(user_id).into());
            w.set_profile_is_muted(
                ctx.is_conversation_muted(dm_conversation(user_id)).unwrap_or(false),
            );
            load_shared_tabs(w.as_weak(), ctx.clone(), *user_id);
        }
    }

    w.set_active_modal("user-profile".into());
}

/// The conversation whose shared content a contact's profile shows: the DM
/// with them. Same key `routing_key()` produces, so it matches the rows the
/// index stored.
fn dm_conversation(peer_id: &UserId) -> chatter_core::state::ActiveConversation {
    chatter_core::state::ActiveConversation::ServerDm { peer_id: *peer_id }
}

/// Load the tab strip, then open whichever tab comes first.
///
/// Core decides which tabs exist and in what order — an empty strip means
/// nothing has ever been shared, and the UI renders the empty state.
fn load_shared_tabs(weak: slint::Weak<MainWindow>, ui_ctx: Arc<ChatterClient>, peer_id: UserId) {
    tokio::spawn(async move {
        let tabs = ui_ctx.shared_content_tabs(dm_conversation(&peer_id)).await;
        let first = tabs.first().map(|t| t.id.clone());
        let rows: Vec<SharedTab> = tabs
            .into_iter()
            .map(|t| SharedTab {
                id: t.id.into(),
                label: t.label.into(),
                count: t.count,
            })
            .collect();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            // A different profile may have been opened while this was in
            // flight — do not paint its tabs over the current one.
            if SHARED_PAGE.with(|p| p.borrow().peer) != Some(peer_id) {
                return;
            }
            w.set_profile_shared_tabs(ModelRc::new(VecModel::from(rows)));
            match first {
                Some(tab) => {
                    w.set_profile_shared_tab(tab.clone().into());
                    // `request_shared_page` owns the loading flag from here.
                    request_shared_page(&w, &ui_ctx, peer_id, tab, false);
                }
                // Nothing shared at all — clear the flag ourselves, or the
                // spinner outlives the query that would have cleared it.
                None => w.set_profile_shared_loading(false),
            }
        });
    });
}

/// Fetch one page of a tab and paint it. `append` continues the open tab;
/// otherwise the list is replaced (a tab switch).
///
/// Must be called on the UI thread — it owns the paging cursor.
fn request_shared_page(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    peer_id: UserId,
    tab: String,
    append: bool,
) {
    let offset = SHARED_PAGE.with(|p| {
        let mut p = p.borrow_mut();
        if append && (p.exhausted || p.in_flight || p.tab != tab) {
            return None;
        }
        if !append {
            p.tab = tab.clone();
            p.loaded = 0;
            p.exhausted = false;
        }
        p.in_flight = true;
        Some(p.loaded as u32)
    });
    let Some(offset) = offset else { return };

    w.set_profile_shared_loading(true);
    let weak = w.as_weak();
    let ui_ctx = ui_ctx.clone();
    tokio::spawn(async move {
        let page = ui_ctx
            .shared_content_page(dm_conversation(&peer_id), &tab, SHARED_PAGE_SIZE, offset)
            .await;
        let short = page.len() < SHARED_PAGE_SIZE as usize;
        let count = page.len();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            SHARED_PAGE.with(|p| {
                let mut p = p.borrow_mut();
                p.in_flight = false;
                // Stale response (profile closed, or tab switched underneath).
                if p.peer != Some(peer_id) || p.tab != tab {
                    return;
                }
                p.loaded += count;
                p.exhausted = short;
            });
            if SHARED_PAGE.with(|p| p.borrow().peer) != Some(peer_id) {
                return;
            }
            w.set_profile_shared_loading(false);

            // Thumbnails decode here, on the UI thread, because a
            // `slint::Image` cannot cross threads.
            let mut rows: Vec<SharedContentItem> = if append {
                let cur = w.get_profile_shared_items();
                (0..cur.row_count()).filter_map(|i| cur.row_data(i)).collect()
            } else {
                Vec::new()
            };
            rows.extend(page.iter().map(|item| SharedContentItem {
                message_id: item.message_id.clone().into(),
                attachment_id: item.attachment_id.clone().into(),
                category: item.category.clone().into(),
                title: item.title.clone().into(),
                subtitle: item.subtitle.clone().into(),
                thumbnail: crate::bin_ui::converters::avatar_bytes_to_image(&item.thumbnail)
                    .unwrap_or_default(),
                has_thumbnail: item.has_thumbnail,
                is_video: item.is_video,
            }));
            w.set_profile_shared_items(ModelRc::new(VecModel::from(rows)));
        });
    });
}

/// Wire the shared-content drawer: tab switches, paging, and opening an item.
fn wire_shared_content(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_tab = ui_ctx.clone();
    window.on_profile_shared_tab_selected(move |tab| {
        let Some(w) = weak_window.upgrade() else { return };
        let Some(peer) = SHARED_PAGE.with(|p| p.borrow().peer) else { return };
        w.set_profile_shared_items(ModelRc::new(VecModel::<SharedContentItem>::from(vec![])));
        request_shared_page(&w, &ui_ctx_tab, peer, tab.to_string(), false);
    });

    let weak_window = window.as_weak();
    let ui_ctx_more = ui_ctx.clone();
    window.on_profile_shared_load_more(move || {
        let Some(w) = weak_window.upgrade() else { return };
        let Some(peer) = SHARED_PAGE.with(|p| p.borrow().peer) else { return };
        let tab = w.get_profile_shared_tab().to_string();
        if tab.is_empty() {
            return;
        }
        request_shared_page(&w, &ui_ctx_more, peer, tab, true);
    });

    // Opening an item: a link goes to the browser, everything else jumps the
    // conversation to the message it arrived in — which is the existing
    // quote-click path, including its "not loaded yet, fetch older history"
    // retry.
    let weak_window = window.as_weak();
    window.on_profile_open_shared_item(move |message_id, _attachment_id, category| {
        let Some(w) = weak_window.upgrade() else { return };
        if category.as_str() == "link" {
            // For a link the title IS the URL; find it in the painted model
            // rather than re-querying.
            let items = w.get_profile_shared_items();
            let url = (0..items.row_count())
                .filter_map(|i| items.row_data(i))
                .find(|i| i.message_id == message_id)
                .map(|i| i.subtitle.to_string());
            if let Some(url) = url {
                // `ExternalOpener` is desktop-only (chatter-core/src/system/mod.rs
                // gates it on `not(android/ios)`), which is why every other caller
                // — pdf_preview.rs, code_preview.rs, attachment_view.rs — guards
                // the import the same way. Unguarded it compiles on the host and
                // breaks only the Android build, where `cargo check` never looks.
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                if let Err(e) = chatter_core::system::ExternalOpener.open_url(&url) {
                    tracing::warn!(error = %e, %url, "Failed to open shared link");
                }
                #[cfg(any(target_os = "android", target_os = "ios"))]
                tracing::warn!(
                    %url,
                    "Opening a shared link needs a platform intent on mobile; \
                     no opener is wired yet"
                );
            }
            return;
        }
        let Some(peer) = SHARED_PAGE.with(|p| p.borrow().peer) else { return };
        w.set_active_modal("none".into());
        w.invoke_dm_selected(peer.to_string().into());
        w.invoke_quote_clicked(message_id);
    });
}

/// Push a username-link URL + its QR image into the profile-modal properties.
/// Must be called on the UI thread (creates a `slint::Image`).
fn set_share_link_props(w: &MainWindow, ui_ctx: &Arc<ChatterClient>, url: &str) {
    w.set_profile_share_link(url.into());
    match ui_ctx
        .username_link_qr(url, 150)
        .map_err(|e| e.to_string())
        .and_then(|png| {
            crate::bin_ui::slint_conversions::memory_to_slint_image(&png)
                .map_err(|e| e.to_string())
        }) {
        Ok(img) => w.set_profile_share_qr(img),
        Err(e) => tracing::error!("Failed to render username-link QR: {}", e),
    }
}

/// Wire the username-link share callbacks (self profile view).
fn wire_share_link(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Generate: create a link if none exists yet (first press), otherwise
    // re-show the stored one. Rotation is the explicit Reset button.
    let weak_window = window.as_weak();
    let ui_ctx_gen = ui_ctx.clone();
    window.on_profile_generate_share_link(move || {
        spawn_create_link(weak_window.clone(), ui_ctx_gen.clone(), false);
    });

    // Reset: always create a fresh link (server-side rotation — previously
    // shared URLs stop resolving).
    let weak_window = window.as_weak();
    let ui_ctx_reset = ui_ctx.clone();
    window.on_profile_reset_share_link(move || {
        spawn_create_link(weak_window.clone(), ui_ctx_reset.clone(), true);
    });

    // Copy: same clipboard strategy as the friend-code copy (wl-copy → xclip).
    let weak_window = window.as_weak();
    window.on_profile_copy_share_link(move || {
        if let Some(w) = weak_window.upgrade() {
            let url = w.get_profile_share_link().to_string();
            if url.is_empty() {
                return;
            }
            std::thread::spawn(move || {
                let result = std::process::Command::new("wl-copy")
                    .arg(&url)
                    .status()
                    .or_else(|_| {
                        std::process::Command::new("xclip")
                            .args(["-selection", "clipboard"])
                            .stdin(std::process::Stdio::piped())
                            .spawn()
                            .and_then(|mut child| {
                                use std::io::Write;
                                if let Some(stdin) = child.stdin.as_mut() {
                                    stdin.write_all(url.as_bytes())?;
                                }
                                child.wait()
                            })
                    });
                if let Err(e) = result {
                    tracing::error!("Failed to copy username link: {}", e);
                }
            });
            w.set_profile_share_status("Link copied to clipboard".into());
        }
    });
}

/// Create (or rotate) the username link in the background and push URL + QR
/// into the modal. `rotate=false` reuses an existing local link if present.
fn spawn_create_link(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    rotate: bool,
) {
    tokio::spawn(async move {
        // First press with an existing link: just re-show it.
        if !rotate {
            if let Ok(Some(url)) = ui_ctx.username_link_local() {
                let ui_ctx2 = ui_ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        set_share_link_props(&w, &ui_ctx2, &url);
                        w.set_profile_share_status("".into());
                    }
                });
                return;
            }
        }

        match ui_ctx.create_username_link().await {
            Ok(url) => {
                let status = if rotate {
                    "Link reset — previously shared links no longer work"
                } else {
                    ""
                };
                let ui_ctx2 = ui_ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        set_share_link_props(&w, &ui_ctx2, &url);
                        w.set_profile_share_status(status.into());
                    }
                });
            }
            Err(e) => {
                let msg = format!("{}", e);
                tracing::error!("Failed to create username link: {}", msg);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_profile_share_status(msg.into());
                    }
                });
            }
        }
    });
}

pub fn wire_profile_actions(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {

    // Username-link share (self profile view)
    wire_share_link(window, ui_ctx);

    // Shared-content drawer (Media / Files / Links / Voice / GIFs / Music)
    wire_shared_content(window, ui_ctx);

    // Mute — persists through the same `mute_conversation` path channel
    // settings uses, keyed on the DM with this peer.
    let weak_window = window.as_weak();
    let ui_ctx_mute = ui_ctx.clone();
    window.on_profile_toggle_mute(move || {
        let Some(w) = weak_window.upgrade() else { return };
        let Ok(peer_id) = w.get_profile_user_id().parse::<UserId>() else { return };
        let muted = !w.get_profile_is_muted();
        // Reflect immediately, then persist — snapping back on failure.
        w.set_profile_is_muted(muted);
        if let Err(e) = ui_ctx_mute.mute_conversation(dm_conversation(&peer_id), muted) {
            tracing::error!(error = %e, "Failed to persist profile mute");
            w.set_profile_is_muted(!muted);
        }
    });

    // Call / Video. `start-voice-call` deliberately takes no target — it reads
    // the ACTIVE conversation. So the profile's buttons open the DM first,
    // exactly the way the Calls screen's redial does, and only then place the
    // call.
    let weak_window = window.as_weak();
    window.on_profile_start_call(move |is_video| {
        let Some(w) = weak_window.upgrade() else { return };
        let peer_id = w.get_profile_user_id().to_string();
        if peer_id.is_empty() {
            return;
        }
        w.set_active_modal("none".into());
        // Opening first commits `ActiveConversation`, which is what
        // `start_*_call` dispatches on — same ordering the Calls screen's
        // redial uses.
        w.invoke_dm_selected(peer_id.into());
        if is_video {
            w.invoke_start_video_call();
        } else {
            w.invoke_start_voice_call();
        }
    });

    // Add friend
    let weak_window = window.as_weak();
    let ui_ctx_add = ui_ctx.clone();

    window.on_profile_add_friend(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_add.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
        let profile_user_id = match profile_user_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                tracing::error!("Could not get profile user ID");
                return;
            }
        };

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_profile_loading(true);
                }
            });

            let friend_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            match tokio::time::timeout(
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                ui_ctx.send_friend_request(&friend_id, None),
            ).await {
                Ok(Ok(_)) => {
                    tracing::info!("Friend request sent to {}", friend_id);
                    // Core emits `FriendRequestsUpdated` after send — the
                    // domain-handler dispatcher refreshes the list. No
                    // manual list_friend_requests + reshape here.
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to send friend request: {}", e);
                }
                Err(_) => {
                    tracing::error!("Friend request timed out");
                }
            }
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_profile_loading(false);
                }
            });
        });
    });

    // Remove friend
    let weak_window = window.as_weak();
    let ui_ctx_remove = ui_ctx.clone();

    window.on_profile_remove_friend(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_remove.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
        let profile_user_id = match profile_user_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                tracing::error!("Could not get profile user ID");
                return;
            }
        };

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_profile_loading(true);
                }
            });

            let friend_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            match tokio::time::timeout(
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                ui_ctx.remove_friend(&friend_id),
            ).await {
                Ok(Ok(())) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_friend(false);
                            tracing::info!("Removed friend: {}", friend_id);
                        }
                    });
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to remove friend: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
                Err(_) => {
                    tracing::error!("Remove friend timed out");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Block user
    let weak_window = window.as_weak();
    let ui_ctx_block = ui_ctx.clone();

    window.on_profile_block_user(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_block.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
        let profile_user_id = match profile_user_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                tracing::error!("Could not get profile user ID");
                return;
            }
        };

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_profile_loading(true);
                }
            });

            let user_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            match ui_ctx.block_profile_user(&user_id).await {
                Ok(()) => {
                    crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                        weak.clone(), ui_ctx.clone(),
                    );
                    crate::bin_ui::features::modals::friends::refresh_blocked_users(
                        weak.clone(), &ui_ctx,
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_blocked(true);
                            w.set_profile_is_friend(false);
                            tracing::info!("Blocked user: {}", user_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to block user: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Unblock user
    let weak_window = window.as_weak();
    let ui_ctx_unblock = ui_ctx.clone();

    window.on_profile_unblock_user(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_unblock.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
        let profile_user_id = match profile_user_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                tracing::error!("Could not get profile user ID");
                return;
            }
        };

        tokio::spawn(async move {
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_profile_loading(true);
                }
            });

            let user_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                    return;
                }
            };

            match ui_ctx.unblock_profile_user(&user_id).await {
                Ok(()) => {
                    crate::bin_ui::features::auth::initial_data::refresh_friends_list(
                        weak.clone(), ui_ctx.clone(),
                    );
                    crate::bin_ui::features::modals::friends::refresh_blocked_users(
                        weak.clone(), &ui_ctx,
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_blocked(false);
                            tracing::info!("Unblocked user: {}", user_id);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to unblock user: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                        }
                    });
                }
            }
        });
    });

    // Send message (open DM). Core picks the conversation: in shield mode the
    // anon DM with this peer (or none at all — never the identified DM),
    // otherwise the channel-free server DM. Opening goes through the same
    // `dm-selected` path as every other DM entry point.
    let weak_window = window.as_weak();
    let ui_ctx_dm = ui_ctx.clone();
    window.on_profile_send_message(move || {
        let Some(w) = weak_window.upgrade() else { return };
        let peer = w.get_profile_user_id().to_string();
        let Some(dm_id) = ui_ctx_dm
            .resolve_dm_conversation(&peer)
            .and_then(|conversation| conversation.sidebar_key())
        else {
            tracing::warn!("profile: no conversation with this peer is available in the current mode");
            return;
        };
        w.set_active_modal("none".into());
        w.invoke_dm_selected(dm_id.into());
    });

    // Edit profile actions
    let weak_window = window.as_weak();
    window.on_profile_edit_profile(move || {
        let weak = weak_window.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_edit_profile_display_name(w.get_profile_display_name());
                w.set_edit_profile_bio(w.get_profile_bio());
                w.set_edit_profile_avatar(w.get_user_avatar_source());
                
                // Clear any pending avatar
                PENDING_AVATAR_PATH.with(|p| *p.borrow_mut() = None);
                *PENDING_AVATAR_BYTES.lock().unwrap() = None;
                
                w.set_active_modal("edit-profile".into());
            }
        });
    });

    let weak_window = window.as_weak();
    let ui_ctx_save = ui_ctx.clone();
    window.on_edit_profile_save(move |display_name: slint::SharedString, bio: slint::SharedString| {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_save.clone();
        let display_name = display_name.to_string();
        let bio = bio.to_string();

        tokio::spawn(async move {
            // Set loading state
            let weak_loading = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_edit_profile_loading(true);
                    w.set_edit_profile_error("".into());
                }
            });

            // Map bio to status message
            let status_msg = if !bio.is_empty() {
                Some(chatter_core::domain::user::StatusMessage::new(bio))
            } else {
                None
            };

            // Update profile
            tracing::info!(
                "Updating user profile - display_name: {}, bio: {:?}",
                display_name, status_msg.as_ref().map(|m| &m.text)
            );

            // Persist the picked avatar HERE, not at pick time: Save is what
            // commits the edit, and `update_user_profile` re-reads
            // `keys::AVATAR` to build the upload (§112v).
            if let Some(bytes) = PENDING_AVATAR_BYTES.lock().unwrap().take() {
                if let Err(e) = ui_ctx.set_avatar_bytes(&bytes) {
                    tracing::error!(error = %e, "Failed to persist picked avatar bytes");
                }
            }

            match ui_ctx
                .update_user_profile(Some(display_name.clone()), status_msg.clone())
                .await
            {
                Ok(profile) => {
                    tracing::info!(
                        "Profile updated - display_name: {}, bio: {:?}",
                        profile.display_name, profile.status_message.as_ref().map(|m| &m.text)
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_edit_profile_loading(false);
                            // Back to the profile page it was opened from (its
                            // only entry point), like Cancel — not the chat list.
                            w.set_active_modal("user-profile".into());

                            // Update UI with new profile info
                            w.set_profile_display_name(profile.display_name.clone().into());
                            if let Some(msg) = &profile.status_message {
                                w.set_profile_bio(msg.text.clone().into());
                            } else {
                                w.set_profile_bio("".into());
                            }

                            // Update sidebar name too
                            w.set_user_display_name(profile.display_name.into());
                            
                            // Clear pending avatar
                            PENDING_AVATAR_PATH.with(|p| *p.borrow_mut() = None);
                        }
                    });
                }
                Err(e) => {
                    let error_msg = e.to_string();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_edit_profile_loading(false);
                            w.set_edit_profile_error(error_msg.into());
                        }
                    });
                }
            }
        });
    });

    // Change avatar with file picker
    let weak_window = window.as_weak();
    window.on_edit_profile_change_avatar(move || {
        let weak = weak_window.clone();

        tokio::spawn(async move {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            let picked = {
                let config = chatter_core::media::FilePickerConfig::new()
                    .with_title("Select Profile Picture")
                    .with_filter(chatter_core::media::FileFilter::images());
                chatter_core::media::FilePicker::pick_file(config).await
            };
            // Android has no `rfd` backend; go through the SAF bridge instead,
            // otherwise "Change Avatar" is a dead button on the phone.
            #[cfg(target_os = "android")]
            let picked = crate::platform::android::file_picker::pick_files(&["image/*"])
                .await
                .into_iter()
                .next();
            #[cfg(target_os = "ios")]
            let picked: Option<std::path::PathBuf> = {
                tracing::warn!("File picker not supported on iOS");
                None
            };

            let Some(path) = picked else { return };
            match std::fs::read(&path) {
                Ok(data) => {
                    // PERSIST the picked avatar into the local `keys::AVATAR`
                    // store NOW (the F-UI / MISSING-FEATURES #3 fix). Before,
                    // the picked file went ONLY into the write-never-read
                    // PENDING_AVATAR_PATH + a Slint preview, so Save's
                    // `update_user_profile` re-read the STALE avatar and the
                    // change never uploaded. Writing keys::AVATAR here means
                    // Save encrypts + uploads THESE bytes as `encrypted_avatar`
                    // (and broadcasts to anon DMs), so the change actually
                    // persists server-side and reaches friends.
                    *PENDING_AVATAR_BYTES.lock().unwrap() = Some(data.clone());
                    // Store the path for later use
                    PENDING_AVATAR_PATH.with(|p| *p.borrow_mut() = Some(path.clone()));

                    // Create the image on the UI thread
                    let _ = slint::invoke_from_event_loop(move || {
                        // Process image on UI thread where it's safe
                        match crate::bin_ui::slint_conversions::memory_to_slint_image(&data) {
                            Ok(slint_img) => {
                                if let Some(w) = weak.upgrade() {
                                    w.set_edit_profile_avatar(slint_img);
                                }
                            }
                            Err(e) => tracing::error!("Failed to decode avatar image: {}", e),
                        }
                    });
                }
                Err(e) => tracing::error!("Failed to read avatar file: {}", e),
            }
        });
    });

    // View safety number — generates and displays the 60-digit fingerprint
    let weak_window = window.as_weak();
    let ui_ctx_safety = ui_ctx.clone();
    window.on_profile_view_safety_number(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_safety.clone();

        let profile_user_id = weak.upgrade().map(|w| w.get_profile_user_id().to_string());
        let profile_user_id = match profile_user_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                tracing::error!("No profile user ID for safety number");
                return;
            }
        };

        tokio::spawn(async move {
            let peer_id: UserId = match profile_user_id.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid user ID for safety number: {}", e);
                    return;
                }
            };

            match ui_ctx.get_safety_number(&peer_id).await {
                Ok(safety_number) => {
                    // Format as 12 groups of 5 digits
                    let formatted: String = safety_number
                        .chars()
                        .collect::<Vec<_>>()
                        .chunks(5)
                        .map(|chunk| chunk.iter().collect::<String>())
                        .collect::<Vec<_>>()
                        .join(" ");

                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_safety_number(formatted.into());
                            w.set_profile_show_safety_number(true);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to generate safety number: {}", e);
                }
            }
        });
    });

    // Toggle verified state
    let weak_window = window.as_weak();
    let ui_ctx_verify = ui_ctx.clone();
    window.on_profile_toggle_verified(move || {
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_verify.clone();

        let (profile_user_id, currently_verified) = match weak.upgrade() {
            Some(w) => (w.get_profile_user_id().to_string(), w.get_profile_is_verified()),
            None => return,
        };

        if profile_user_id.is_empty() {
            return;
        }

        let peer_id: UserId = match profile_user_id.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Invalid user ID for verification toggle: {}", e);
                return;
            }
        };

        if currently_verified {
            // Unverify — sync operation
            match ui_ctx.mark_peer_unverified(&peer_id) {
                Ok(()) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_is_verified(false);
                        }
                    });
                }
                Err(e) => tracing::error!("Failed to unverify peer: {}", e),
            }
        } else {
            // Verify — async (needs to generate safety number first)
            tokio::spawn(async move {
                match ui_ctx.mark_peer_verified(&peer_id).await {
                    Ok(()) => {
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_profile_is_verified(true);
                            }
                        });
                    }
                    Err(e) => tracing::error!("Failed to verify peer: {}", e),
                }
            });
        }
    });
}

