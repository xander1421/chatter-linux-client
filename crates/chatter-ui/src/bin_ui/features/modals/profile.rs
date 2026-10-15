//! Profile modal callbacks.
//!
//! Uses UserService for all profile operations - no direct network calls.

use std::cell::RefCell;
use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;

use crate::{MainWindow, SharedContentItem, SharedTab};
use chatter_core::ui::initials_from_name;

mod avatar_edit;

pub(crate) fn retire_avatar_edit() {
    avatar_edit::retire();
}

thread_local! {
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
    conversation: Option<chatter_core::state::ActiveConversation>,
    generation: u64,
    tab: String,
    loaded: usize,
    exhausted: bool,
    in_flight: bool,
}

/// Open the user profile modal with all properties populated correctly.
///
/// Handles both "self" (own profile) and "other user" paths, ensuring
/// consistent data regardless of which entry point triggered the modal.
/// Only fills the page: the caller decides whether it shows as the Profile
/// tab or as a screen pushed over the current one (`back::push`).
pub fn open_user_profile(
    w: &MainWindow,
    user_id: &UserId,
    auth_state: &chatter_core::state::AuthState,
    user_state: &chatter_core::state::UserState,
    ui_ctx: Option<&Arc<ChatterClient>>,
) {
    open_user_profile_in_conversation(
        w,
        user_id,
        auth_state,
        user_state,
        ui_ctx,
        dm_conversation(user_id),
    );
}

/// Open a sender profile while keeping shared content scoped to its source chat.
pub fn open_user_profile_in_conversation(
    w: &MainWindow,
    user_id: &UserId,
    auth_state: &chatter_core::state::AuthState,
    user_state: &chatter_core::state::UserState,
    ui_ctx: Option<&Arc<ChatterClient>>,
    conversation: chatter_core::state::ActiveConversation,
) {
    let scoped_member = matches!(
        conversation,
        chatter_core::state::ActiveConversation::AnonGroup(_)
    );
    w.set_profile_scoped_member(scoped_member);
    let is_self = !scoped_member && auth_state.get().current_user_id.as_ref() == Some(user_id);

    w.set_profile_user_id(user_id.to_string().into());
    w.set_profile_is_anonymous(conversation.is_anon());
    // Offered for another user only (set again on that path below).
    w.set_profile_can_start_secret_chat(false);
    // Reset safety number state for fresh modal
    w.set_profile_safety_number("".into());
    w.set_profile_show_safety_number(false);

    // Reset username-link share state; re-populated below for self.
    w.set_profile_share_link("".into());
    w.set_profile_share_status("".into());

    if let chatter_core::state::ActiveConversation::AnonGroup(group_id) = conversation {
        let member = ui_ctx.and_then(|ctx| ctx.get_anon_group_member(&group_id, user_id));
        let name = ui_ctx
            .and_then(|ctx| ctx.get_anon_group_member_display_name(&group_id, user_id))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Anonymous member".into());
        w.set_profile_initials(initials_from_name(&name).into());
        w.set_profile_display_name(name.into());
        w.set_profile_avatar_source(
            member
                .and_then(|member| member.avatar)
                .as_deref()
                .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
                .unwrap_or_default(),
        );
        w.set_profile_username("".into());
        w.set_profile_bio("".into());
        w.set_profile_status_text("".into());
        w.set_profile_presence("offline".into());
        w.set_profile_is_self(false);
        w.set_profile_is_friend(false);
        w.set_profile_is_blocked(false);
        w.set_profile_is_verified(false);
        crate::bin_ui::features::chat::disappearing::hide_for_profile(w);
    } else if is_self {
        let state = auth_state.get();
        let display_name = state.display_name.clone().unwrap_or_default();
        // The handle chosen at registration. Never the user id: that is not
        // something anyone can look you up by.
        let username = ui_ctx
            .and_then(|ctx| ctx.full_username())
            .filter(|u| !u.is_empty())
            .unwrap_or_default();
        let initials = initials_from_name(&display_name);

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

        crate::bin_ui::features::chat::disappearing::hide_for_profile(w);
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
        let username = ui_ctx
            .and_then(|ctx| ctx.known_username(user_id))
            .unwrap_or_default();

        let initials = initials_from_name(&display_name);

        // Core's display presence (the local user directory, hidden for a
        // blocked user) — the same source as `peer_last_seen_label`.
        let presence = ui_ctx.map_or("offline", |ctx| ctx.peer_presence(user_id).as_str());

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
        let is_blocked = ui_ctx
            .map(|ctx| ctx.is_profile_user_blocked(user_id))
            .unwrap_or(false);

        w.set_profile_display_name(display_name.into());
        w.set_profile_username(username.into());
        w.set_profile_initials(initials.into());
        // Render the peer's avatar from their decrypted server-profile bytes
        // (no URL); falls back to initials when we have none.
        let avatar_source = ui_ctx
            .and_then(|ctx| ctx.user_avatar(user_id))
            .as_deref()
            .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
            .unwrap_or_default();
        w.set_profile_avatar_source(avatar_source);
        w.set_profile_presence(presence.into());
        w.set_profile_bio(bio.into());
        // A secret DM shows only what the chat itself carried: the peer's
        // in-chat name and avatar, no account-profile bio.
        if let (chatter_core::state::ActiveConversation::AnonDm(id), Some(ctx)) =
            (conversation, ui_ctx)
        {
            if let Some((name, avatar)) = ctx.anon_dm_peer_card(&id) {
                w.set_profile_initials(initials_from_name(&name).into());
                w.set_profile_display_name(name.into());
                w.set_profile_avatar_source(
                    avatar
                        .as_deref()
                        .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
                        .unwrap_or_default(),
                );
            }
            w.set_profile_bio("".into());
        }
        // Check verification state
        let is_verified = ui_ctx
            .and_then(|ctx| ctx.is_peer_verified(user_id).ok())
            .unwrap_or(false);

        // The DM's disappearing-messages timer: this screen is the DM's
        // settings (see header.rs `wire_chat_settings_clicked`).
        match ui_ctx {
            Some(ctx) => crate::bin_ui::features::chat::disappearing::show_for_profile(
                w, ctx, user_id, is_friend,
            ),
            None => crate::bin_ui::features::chat::disappearing::hide_for_profile(w),
        }
        w.set_profile_is_self(false);
        w.set_profile_is_friend(is_friend);
        w.set_profile_is_blocked(is_blocked);
        w.set_profile_can_start_secret_chat(
            ui_ctx.is_some_and(|ctx| ctx.can_start_secret_chat(user_id)),
        );
        w.set_profile_is_verified(is_verified);
    }

    // Shared-content drawer. Reset first so the previous profile's media never
    // flashes under the new name, then load this conversation's tabs.
    w.set_profile_shared_tabs(ModelRc::new(VecModel::<SharedTab>::from(vec![])));
    w.set_profile_shared_items(ModelRc::new(VecModel::<SharedContentItem>::from(vec![])));
    w.set_profile_shared_tab("".into());
    w.set_profile_last_seen("".into());
    w.set_profile_is_muted(false);
    SHARED_PAGE.with(|p| {
        let generation = p.borrow().generation.wrapping_add(1);
        *p.borrow_mut() = SharedPage {
            generation,
            peer: Some(*user_id),
            conversation: Some(conversation),
            ..Default::default()
        }
    });

    if !is_self || shared_sender(conversation, *user_id).is_some() {
        if let Some(ctx) = ui_ctx {
            // A peer's name/bio/avatar edit is not pushed reliably; refetch on
            // open (throttled in core) so the modal catches up via
            // UserProfileUpdated. Core refuses it from a secret chat.
            if !is_self {
                let (refresh_ctx, peer) = (ctx.clone(), *user_id);
                tokio::spawn(async move {
                    refresh_ctx
                        .refresh_profile_if_stale(&peer, &conversation)
                        .await
                });
            }
            // Loading from the first frame: an empty strip is also what "no
            // tabs" looks like, and flashing "Nothing shared yet" for the
            // length of one query reads as a bug.
            w.set_profile_shared_loading(true);
            if !scoped_member {
                w.set_profile_last_seen(ctx.peer_last_seen_label(user_id).into());
            }
            w.set_profile_is_muted(
                ctx.is_conversation_muted(if scoped_member {
                    conversation
                } else {
                    dm_conversation(user_id)
                })
                .unwrap_or(false),
            );
            load_shared_tabs(w.as_weak(), ctx.clone(), *user_id, conversation);
        }
    }
}

/// The conversation whose shared content a contact's profile shows: the DM
/// with them. Same key `routing_key()` produces, so it matches the rows the
/// index stored.
fn dm_conversation(peer_id: &UserId) -> chatter_core::state::ActiveConversation {
    chatter_core::state::ActiveConversation::ServerDm { peer_id: *peer_id }
}

/// Group/channel avatar profiles show that sender's contributions only.
/// DMs retain their existing two-participant shared history.
fn shared_sender(
    conversation: chatter_core::state::ActiveConversation,
    peer: UserId,
) -> Option<UserId> {
    use chatter_core::state::ActiveConversation;
    match conversation {
        ActiveConversation::Channel(_) | ActiveConversation::AnonGroup(_) => Some(peer),
        _ => None,
    }
}

/// Load the tab strip, then open whichever tab comes first.
///
/// Core decides which tabs exist and in what order — an empty strip means
/// nothing has ever been shared, and the UI renders the empty state.
fn load_shared_tabs(
    weak: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    peer_id: UserId,
    conversation: chatter_core::state::ActiveConversation,
) {
    let generation = SHARED_PAGE.with(|p| p.borrow().generation);
    tokio::spawn(async move {
        let tabs = ui_ctx
            .shared_content_tabs_scoped(conversation, shared_sender(conversation, peer_id))
            .await;
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
            if SHARED_PAGE.with(|p| {
                (
                    p.borrow().peer,
                    p.borrow().conversation,
                    p.borrow().generation,
                )
            }) != (Some(peer_id), Some(conversation), generation)
            {
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
    let Some(conversation) = SHARED_PAGE.with(|p| p.borrow().conversation) else {
        return;
    };
    let offset = SHARED_PAGE.with(|p| {
        let mut p = p.borrow_mut();
        if append && (p.exhausted || p.in_flight || p.tab != tab) {
            return None;
        }
        if !append {
            p.generation = p.generation.wrapping_add(1);
            p.tab = tab.clone();
            p.loaded = 0;
            p.exhausted = false;
        }
        p.in_flight = true;
        Some((p.loaded as u32, p.generation))
    });
    let Some((offset, generation)) = offset else {
        return;
    };

    w.set_profile_shared_loading(true);
    let weak = w.as_weak();
    let ui_ctx = ui_ctx.clone();
    let revision = ui_ctx.auth_state().get().session_revision;
    tokio::spawn(async move {
        let page = ui_ctx
            .shared_content_page_scoped(
                conversation,
                &tab,
                SHARED_PAGE_SIZE,
                offset,
                shared_sender(conversation, peer_id),
            )
            .await;
        #[cfg(target_os = "android")]
        let page = {
            let client = ui_ctx.clone();
            tokio::task::spawn_blocking(move || {
                let mut page = page;
                // At most four missing videos per requested page, with no remote download.
                for item in page
                    .iter_mut()
                    .filter(|i| i.is_video && i.thumbnail.is_empty())
                    .take(4)
                {
                    if let Ok(id) = item.attachment_id.parse() {
                        if let Ok(result) = client.decrypt_and_extract_video_thumbnail(&id) {
                            item.thumbnail = result.image_bytes;
                        }
                    }
                }
                page
            })
            .await
            .unwrap_or_default()
        };
        let short = page.len() < SHARED_PAGE_SIZE as usize;
        let count = page.len();
        let _ = slint::invoke_from_event_loop(move || {
            if ui_ctx.auth_state().get().session_revision != revision {
                return;
            }
            let Some(w) = weak.upgrade() else { return };
            let current = SHARED_PAGE.with(|p| {
                let mut p = p.borrow_mut();
                if p.peer != Some(peer_id)
                    || p.conversation != Some(conversation)
                    || p.tab != tab
                    || p.generation != generation
                {
                    return false;
                }
                p.in_flight = false;
                p.loaded += count;
                p.exhausted = short;
                true
            });
            if !current {
                return;
            }
            if SHARED_PAGE.with(|p| {
                (
                    p.borrow().peer,
                    p.borrow().conversation,
                    p.borrow().generation,
                )
            }) != (Some(peer_id), Some(conversation), generation)
            {
                return;
            }
            w.set_profile_shared_loading(false);

            // Thumbnails decode here, on the UI thread, because a
            // `slint::Image` cannot cross threads.
            let mut rows: Vec<SharedContentItem> = if append {
                let cur = w.get_profile_shared_items();
                cur.iter().collect()
            } else {
                Vec::new()
            };
            rows.extend(page.iter().map(|item| {
                let thumbnail = crate::bin_ui::converters::avatar_bytes_to_image(&item.thumbnail)
                    .unwrap_or_default();
                let has_thumbnail = thumbnail.size().width > 0;
                SharedContentItem {
                    message_id: item.message_id.clone().into(),
                    attachment_id: item.attachment_id.clone().into(),
                    category: item.category.clone().into(),
                    title: shared_content_display_title(&item.category, &item.title).into(),
                    subtitle: item.subtitle.clone().into(),
                    thumbnail,
                    has_thumbnail,
                    is_video: item.is_video,
                }
            }));
            w.set_profile_shared_items(ModelRc::new(VecModel::from(rows)));
        });
    });
}

/// Only the painted profile title changes; indexed filenames and jump IDs stay intact.
fn shared_content_display_title<'a>(category: &str, title: &'a str) -> &'a str {
    if category == "voice" {
        // Recorded labels have appeared with spaces and underscores. Reuse the
        // attachment predicate after normalizing those separators for display only.
        let normalized = title
            .split(|c: char| c.is_whitespace() || c == '_')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if chatter_core::domain::attachment::is_generic_voice_note_filename(&normalized) {
            return "Voice message";
        }
    }
    title
}

/// Wire the shared-content drawer: tab switches, paging, and opening an item.
fn wire_shared_content(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx_tab = ui_ctx.clone();
    window.on_profile_shared_tab_selected(move |tab| {
        let Some(w) = weak_window.upgrade() else {
            return;
        };
        let Some(peer) = SHARED_PAGE.with(|p| p.borrow().peer) else {
            return;
        };
        w.set_profile_shared_items(ModelRc::new(VecModel::<SharedContentItem>::from(vec![])));
        request_shared_page(&w, &ui_ctx_tab, peer, tab.to_string(), false);
    });

    let weak_window = window.as_weak();
    let ui_ctx_more = ui_ctx.clone();
    window.on_profile_shared_load_more(move || {
        let Some(w) = weak_window.upgrade() else {
            return;
        };
        let Some(peer) = SHARED_PAGE.with(|p| p.borrow().peer) else {
            return;
        };
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
    let ui_ctx_jump = ui_ctx.clone();
    window.on_profile_open_shared_item(move |message_id, attachment_id, category, selected_url| {
        let Some(w) = weak_window.upgrade() else {
            return;
        };
        if category.as_str() == "link" {
            // Preserve the clicked row identity, including two URLs in one
            // message. The message-link callback owns validation and the
            // platform opener (Android intent or desktop browser).
            open_shared_link(
                w.get_profile_shared_items().iter(),
                message_id.as_str(),
                attachment_id.as_str(),
                selected_url.as_str(),
                |url| w.invoke_open_message_link(url.into()),
            );
            return;
        }
        let Some(conversation) = SHARED_PAGE.with(|p| p.borrow().conversation) else {
            return;
        };
        crate::bin_ui::features::back::reset_to_chats(&w);
        crate::bin_ui::features::header::jump_to_conversation_message(
            &w,
            &ui_ctx_jump,
            conversation,
            &message_id,
        );
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
            crate::bin_ui::slint_conversions::memory_to_slint_image(&png).map_err(|e| e.to_string())
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
const REMOVE_FAILED: &str = "Not removed — you are still friends. Try again.";

/// Say how a relationship change on the profile went: the button flipping
/// label alone was the only feedback (REVIEW-R4 P1-12). UI thread only.
fn show_toast(w: &MainWindow, (message, variant): (&str, &str)) {
    w.set_toast_message(message.into());
    w.set_toast_variant(variant.into());
    w.set_toast_visible(true);
}

fn spawn_create_link(weak: slint::Weak<MainWindow>, ui_ctx: Arc<ChatterClient>, rotate: bool) {
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
                tracing::error!("Failed to create username link: {}", e);
                // Never the Display chain ("invalid input: ...") — that is for logs.
                let msg = e.user_message();
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
        let Some(w) = weak_window.upgrade() else {
            return;
        };
        let Ok(peer_id) = w.get_profile_user_id().parse::<UserId>() else {
            return;
        };
        let muted = !w.get_profile_is_muted();
        // Reflect immediately, then persist — snapping back on failure.
        w.set_profile_is_muted(muted);
        if let Err(e) = ui_ctx_mute.mute_conversation(
            if w.get_profile_scoped_member() {
                SHARED_PAGE
                    .with(|p| p.borrow().conversation)
                    .unwrap_or(chatter_core::state::ActiveConversation::None)
            } else {
                dm_conversation(&peer_id)
            },
            muted,
        ) {
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
        let Some(w) = weak_window.upgrade() else {
            return;
        };
        // No calls from a secret chat (the tiles are hidden there too).
        if w.get_profile_scoped_member() || w.get_profile_is_anonymous() {
            return;
        }
        let peer_id = w.get_profile_user_id().to_string();
        if peer_id.is_empty() {
            return;
        }
        crate::bin_ui::features::back::reset_to_chats(&w);
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
        if weak
            .upgrade()
            .is_some_and(|w| w.get_profile_scoped_member())
        {
            return;
        }
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

            let notice = match tokio::time::timeout(
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                ui_ctx.send_friend_request(&friend_id, None),
            )
            .await
            {
                Ok(Ok(_)) => {
                    tracing::info!("Friend request sent to {}", friend_id);
                    // Core emits `FriendRequestsUpdated` after send — the
                    // domain-handler dispatcher refreshes the list. No
                    // manual list_friend_requests + reshape here.
                    ("Friend request sent", "success")
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to send friend request: {}", e);
                    ("The friend request was not sent. Try again.", "error")
                }
                Err(_) => {
                    tracing::error!("Friend request timed out");
                    ("The friend request was not sent. Try again.", "error")
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_profile_loading(false);
                    show_toast(&w, notice);
                }
            });
        });
    });

    // Remove friend
    let weak_window = window.as_weak();
    let ui_ctx_remove = ui_ctx.clone();

    window.on_profile_remove_friend(move || {
        let weak = weak_window.clone();
        if weak
            .upgrade()
            .is_some_and(|w| w.get_profile_scoped_member())
        {
            return;
        }
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
            )
            .await
            {
                Ok(Ok(())) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_friend(false);
                            show_toast(&w, ("Removed from friends", "success"));
                            tracing::info!("Removed friend: {}", friend_id);
                        }
                    });
                }
                Ok(Err(e)) => {
                    tracing::error!("Failed to remove friend: {}", e);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            show_toast(&w, (REMOVE_FAILED, "error"));
                        }
                    });
                }
                Err(_) => {
                    tracing::error!("Remove friend timed out");
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            show_toast(&w, (REMOVE_FAILED, "error"));
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
        if weak
            .upgrade()
            .is_some_and(|w| w.get_profile_scoped_member())
        {
            return;
        }
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
                        weak.clone(),
                        ui_ctx.clone(),
                    );
                    crate::bin_ui::features::modals::friends::refresh_blocked_users(
                        weak.clone(),
                        &ui_ctx,
                    );
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_profile_loading(false);
                            w.set_profile_is_blocked(true);
                            w.set_profile_is_friend(false);
                            // Their DM leaves the sidebar (core hides blocked
                            // contacts); history stays for an unblock.
                            crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
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

    // Send message (open DM). Core picks the conversation: a friend's DM in
    // either mode (Shield sends it over the mailbox); for anyone else in
    // shield mode the anon DM with them, or none at all. Opening goes through
    // the same `dm-selected` path as every other DM entry point.
    let weak_window = window.as_weak();
    let ui_ctx_dm = ui_ctx.clone();
    window.on_profile_send_message(move || {
        let Some(w) = weak_window.upgrade() else {
            return;
        };
        if w.get_profile_scoped_member() {
            return;
        }
        let peer = w.get_profile_user_id().to_string();
        let Some(dm_id) = ui_ctx_dm
            .resolve_dm_conversation(&peer)
            .and_then(|conversation| conversation.sidebar_key())
        else {
            tracing::warn!(
                "profile: no conversation with this peer is available in the current mode"
            );
            return;
        };
        crate::bin_ui::features::back::reset_to_chats(&w);
        w.invoke_dm_selected(dm_id.into());
    });

    avatar_edit::wire(window, ui_ctx);

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
                    // 12 groups of 5 digits as Signal lays them out: three
                    // rows of four, so the number never wraps 7 + 5.
                    let groups: Vec<String> = safety_number
                        .chars()
                        .collect::<Vec<_>>()
                        .chunks(5)
                        .map(|chunk| chunk.iter().collect::<String>())
                        .collect();
                    let formatted = groups
                        .chunks(4)
                        .map(|row| row.join(" "))
                        .collect::<Vec<_>>()
                        .join("\n");

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
            Some(w) => (
                w.get_profile_user_id().to_string(),
                w.get_profile_is_verified(),
            ),
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

#[cfg(test)]
mod shared_content_title_tests {
    use super::shared_content_display_title;

    #[test]
    fn recorded_profile_voice_titles_hide_generated_extensions() {
        for filename in [
            "Voice message.wav",
            "VOICE MESSAGE.WAV",
            "voice_message.wav",
            "  Voice   message.wav  ",
            "Voice__message.wav",
        ] {
            assert_eq!(
                shared_content_display_title("voice", filename),
                "Voice message"
            );
        }
    }

    #[test]
    fn authored_audio_and_non_voice_titles_remain_exact() {
        for filename in [
            "interview.wav",
            " My recording.wav ",
            "Voice message remix.wav",
        ] {
            assert_eq!(shared_content_display_title("voice", filename), filename);
        }
        for category in ["music", "file", "media", "link"] {
            assert_eq!(
                shared_content_display_title(category, "Voice message.wav"),
                "Voice message.wav"
            );
        }
    }
}

/// Resolve a click against the currently painted rows before delegating to the
/// existing safe message-link action. Injecting the action keeps regressions
/// independent of real browsers and platform intents.
fn open_shared_link(
    items: impl IntoIterator<Item = SharedContentItem>,
    message_id: &str,
    attachment_id: &str,
    selected_url: &str,
    open: impl FnOnce(&str),
) -> bool {
    let selected = items.into_iter().find(|item| {
        item.category.as_str() == "link"
            && item.message_id.as_str() == message_id
            && item.attachment_id.as_str() == attachment_id
            && item.subtitle.as_str() == selected_url
    });
    if let Some(item) = selected {
        open(item.subtitle.as_str());
        true
    } else {
        false
    }
}

#[cfg(test)]
mod shared_link_tests {
    use super::*;

    #[test]
    fn selected_second_url_uses_injected_message_link_opener() {
        let rows = [
            "https://example.invalid/first",
            "https://example.invalid/second",
        ]
        .map(|url| SharedContentItem {
            message_id: "same-message".into(),
            attachment_id: "".into(),
            category: "link".into(),
            subtitle: url.into(),
            ..Default::default()
        });
        let mut opened = String::new();
        assert!(open_shared_link(
            rows.clone(),
            "same-message",
            "",
            rows[1].subtitle.as_str(),
            |url| opened = url.to_owned()
        ));
        assert_eq!(opened, "https://example.invalid/second");
        assert!(!open_shared_link(
            rows,
            "same-message",
            "",
            "https://example.invalid/stale",
            |_| panic!("stale row must not invoke opener")
        ));
    }
}
