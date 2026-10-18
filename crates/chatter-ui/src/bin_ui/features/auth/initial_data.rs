//! Initial data loading after login.
//!
//! Uses ChatterClient services for all operations - no direct repository access.

use slint::ComponentHandle;
use std::sync::Arc;

use slint::{Model, ModelRc, VecModel, Weak};

use chatter_core::client::ChatterClient;

use crate::bin_ui::converters::initials_from_name;
use crate::bin_ui::converters::{FriendDataExt, FriendRequestExt, FriendRequestSendData};
use crate::{FriendData, FriendRequestData, MainWindow};

/// Rebuild the FriendsModal "Friends" tab list and push it into the UI.
///
/// An empty list is painted from local storage before `ListFriends`
/// returns, so a cold start with the server down does not sit on
/// "No friends yet" until the RPC fails. A list that is already showing
/// is left alone until the server answers: painting the offline rows
/// over it would grey every presence dot for the length of the call.
/// When the server answers, its rows replace the local ones. When it
/// does not, the local rows stay (or replace a stale online list).
pub fn refresh_friends_list(window_weak: Weak<MainWindow>, ui_ctx: Arc<ChatterClient>) {
    let owner = super::super::history_owner::SessionOwner::capture(&ui_ctx);
    tokio::spawn(async move {
        let empty = friends_list_empty(&window_weak).await;
        if !owner.is_current() {
            return;
        }
        if empty {
            publish_friends(
                window_weak.clone(),
                ui_ctx.local_friend_tab_entries(),
                owner.clone(),
            );
        }
        if !owner.is_current() {
            return;
        }
        publish_friends(window_weak, ui_ctx.friends_tab_entries().await, owner);
    });
}

/// Repaint Contacts from the profile already committed by the event stream.
/// A network friends refresh can return an older snapshot and overwrite the
/// new name immediately after the live profile update.
pub fn refresh_friends_list_local(window_weak: Weak<MainWindow>, ui_ctx: Arc<ChatterClient>) {
    let owner = super::super::history_owner::SessionOwner::capture(&ui_ctx);
    tokio::spawn(async move {
        publish_friends(window_weak, ui_ctx.local_friend_tab_entries(), owner);
    });
}

/// `true` when Contacts has nothing to show yet. A dropped window counts
/// as empty so the caller still publishes the local rows.
async fn friends_list_empty(window_weak: &Weak<MainWindow>) -> bool {
    let weak = window_weak.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    if slint::invoke_from_event_loop(move || {
        let empty = weak
            .upgrade()
            .map(|w| {
                w.global::<crate::ModalHostState>()
                    .get_friends_list()
                    .row_count()
                    == 0
            })
            .unwrap_or(true);
        let _ = tx.send(empty);
    })
    .is_err()
    {
        return true;
    }
    rx.await.unwrap_or(true)
}

fn publish_friends(
    window_weak: Weak<MainWindow>,
    friends_data: Vec<chatter_core::ui::sidebar_data::FriendDataSendData>,
    owner: super::super::history_owner::SessionOwner,
) {
    if !owner.is_current() {
        return;
    }
    crate::run_history_ui!(window_weak, owner, |w| {
        let slint_list: Vec<FriendData> = friends_data.into_iter().map(|f| f.to_slint()).collect();
        w.global::<crate::ModalHostState>()
            .set_friends_list(ModelRc::new(VecModel::from(slint_list)));
    });
}

/// Restore the local conversation snapshot before the main view first paints.
/// The later initial-data refresh reconciles any server changes in place.
pub fn show_cached_chat_rows(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let rows = ui_ctx.sidebar_rows();
    if rows.is_empty() {
        return;
    }
    crate::bin_ui::features::sidebar::apply_chat_list(window, ui_ctx, rows);
    window
        .global::<crate::SidebarPane>()
        .set_chat_rows_loading(false);
}

/// Load initial data after login or on startup.
pub async fn load_initial_data(window_weak: Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    let session_owner = super::super::history_owner::SessionOwner::capture(ui_ctx);
    let history_owner = crate::bin_ui::features::history_owner::HistoryOwner::capture(
        ui_ctx,
        ui_ctx.active_conversation(),
    );
    tracing::info!("Loading initial data...");

    // The sidebar snapshot is local. Paint it before any channel/profile RPC
    // so returning users see their conversations on the first main-view frame.
    let cached_rows = ui_ctx.sidebar_rows();
    if !cached_rows.is_empty() {
        let cached_client = ui_ctx.clone();
        crate::run_history_ui!(window_weak, session_owner, |w| {
            crate::bin_ui::features::sidebar::apply_chat_list(&w, &cached_client, cached_rows);
            w.global::<crate::SidebarPane>()
                .set_chat_rows_loading(false);
        });
    }

    // Friends tab, off the awaits below. ListFriends used to run before the
    // first paint and, on failure, publish an empty list over people the
    // chat list was about to render from storage.
    refresh_friends_list(window_weak.clone(), ui_ctx.clone());

    // Now that we're authenticated, start the auth-gated notification streams
    // (gRPC message stream + call → ring-notification bridge). The boot-time
    // wiring only started the consumer; these need a logged-in user / call
    // service, which a fresh user doesn't have at boot. Idempotent.
    ui_ctx.start_authed_notification_streams();

    // Sync channels from server first (fetches and upserts to local storage)
    match ui_ctx.list_channels_paged(100, 0, None).await {
        Ok(synced_channels) => {
            for ch in &synced_channels {
                tracing::debug!(
                    "Synced channel: id={}, name={}, type={:?}",
                    ch.id,
                    ch.name,
                    ch.channel_type
                );
            }
            tracing::info!("Synced {} channels from server", synced_channels.len());
        }
        Err(e) => {
            tracing::warn!("Failed to sync channels from server: {}", e);
        }
    }

    if !session_owner.is_current() {
        return;
    }
    // Pre-fetch anonymous tokens for sealed sender messaging
    // This ensures tokens are available before the first SKDM distribution
    if let Err(e) = ui_ctx.prefetch_tokens(10).await {
        tracing::warn!("Failed to pre-fetch anonymous tokens: {}", e);
    }

    if !session_owner.is_current() {
        return;
    }
    // Refresh any anon group mailboxes nearing their 30-day expiry so the
    // user can continue receiving messages across long offline gaps.
    match ui_ctx.refresh_expiring_group_mailboxes().await {
        Ok(refreshed) if !refreshed.is_empty() => {
            tracing::info!(
                count = refreshed.len(),
                "Refreshed expiring anon group mailboxes"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("Failed to refresh anon group mailboxes: {}", e),
    }

    if !session_owner.is_current() {
        return;
    }
    // Our own name/avatar: restored from the server under our profile key
    // (throttled in core). Only login used to do this, so a linked device
    // showed initials and no photo while peers saw it.
    ui_ctx.refresh_own_profile().await;

    if !session_owner.is_current() {
        return;
    }
    // The chat list is built ENTIRELY in core: one list of every loose
    // conversation, mode-exclusive, ordered by activity with pins first,
    // previews/markers/presence included. Same source as every later
    // rebuild (`sidebar::rebuild_chat_list`) — the UI never hand-rolls it.
    let chat_rows = ui_ctx.sidebar_rows();

    // Get user profile info
    let user_display_name = ui_ctx
        .auth_snapshot()
        .display_name
        .clone()
        .unwrap_or_default();
    let user_initials = initials_from_name(&user_display_name);

    // Load user's own avatar from settings (stored during registration)
    // Pre-decode before entering UI thread to avoid blocking
    let user_avatar_decoded: Option<(Vec<u8>, u32, u32)> = ui_ctx.my_avatar().and_then(|bytes| {
        image::load_from_memory(&bytes).ok().map(|img| {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            tracing::info!(
                avatar_size = bytes.len(),
                width = w,
                height = h,
                "Loaded user's own avatar from settings"
            );
            (rgba.into_raw(), w, h)
        })
    });

    // First channel to select (exclude DMs and SavedMessages) — rule in core.
    let first_channel_id = ui_ctx.first_selectable_channel_id();

    // Friend requests (Received/Sent) are populated EXCLUSIVELY by the
    // `FriendRequestsUpdated` event handler, which decrypts each sender's ECIES
    // envelope (real display name + avatar + intro note). Building them here
    // from `list_friend_requests` would only yield ciphertext names with no
    // note/avatar — and then get clobbered by the event anyway. Start empty and
    // fire the event once below so the rich rows appear immediately on load.
    let received_ui: Vec<FriendRequestSendData> = Vec::new();
    let sent_ui: Vec<FriendRequestSendData> = Vec::new();

    // Load pending anon DM requests (message requests) - show as pending in sidebar and modal
    let mut private_requests_ui: Vec<FriendRequestSendData> = Vec::new();
    {
        let requests = ui_ctx.list_anon_dm_message_requests();
        {
            tracing::info!("Loaded {} pending anon DM requests", requests.len());
            for dm in requests {
                // Get peer display name from conversation (revealed after ECIES decryption)
                // Do NOT use user_service - these are strangers
                let peer_name = dm
                    .peer_display_name
                    .clone()
                    .unwrap_or_else(|| ui_ctx.fallback_display_name(&dm.peer_id));

                // Get intro message if any
                let last_message = match ui_ctx.get_latest_anon_dm_message(&dm.id) {
                    Some(msg) => msg.as_text().unwrap_or_default().to_string(),
                    None => "Message request".to_string(),
                };

                // DM-sidebar row for this request is produced by list_dm_sidebar
                // (core). Here we only build the Friends-modal Requests tab entry.
                // Add to friends modal private requests list (use conversation_id as user_id)
                private_requests_ui.push(FriendRequestSendData {
                    user_id: dm.id.to_string(),
                    display_name: peer_name,
                    username: "Private".to_string(),
                    avatar_bytes: ui_ctx.anon_dm_avatar(&dm),
                    secondary_text: last_message,
                });
            }
        }
    }

    // Load blocked users for the Blocked tab
    let blocked_users_slint = crate::bin_ui::features::modals::friends::blocked_user_rows(&ui_ctx);
    tracing::info!("Loaded {} blocked users", blocked_users_slint.len());

    // Pre-decode avatars before entering UI thread to avoid blocking

    // Update UI with channels, DMs, private groups, and friend requests
    let window_weak_ui = window_weak.clone();
    let pending_message_requests_count = private_requests_ui.len() as i32;
    let ui_ctx_for_refresh = ui_ctx.clone();
    let search_shortcut_pref = ui_ctx.get_search_shortcut().as_str();
    let notification_detail = ui_ctx.notification_detail_level().as_str();
    let own_username = ui_ctx.full_username().unwrap_or_default();
    crate::run_history_ui!(window_weak_ui, session_owner, |w| {
        // Reconcile rows by identity; replacing the model resets scroll and
        // flashes cached conversations a second time after server sync.
        crate::bin_ui::features::sidebar::apply_chat_list(&w, &ui_ctx_for_refresh, chat_rows);
        w.global::<crate::SidebarPane>()
            .set_chat_rows_loading(false);

        #[cfg(target_os = "android")]
        crate::platform::android::notification_route::open_pending(&w);

        w.global::<crate::SidebarPane>()
            .set_user_display_name(user_display_name.into());
        w.global::<crate::SidebarPane>()
            .set_user_initials(user_initials.into());
        // The account menu's @username line (Profile tab) reads this.
        w.global::<crate::SettingsPane>()
            .set_settings_username(own_username.into());

        // Hydrate the persisted search-shortcut preference so the
        // FocusScope picks the right combo (Ctrl+K / Ctrl+F / Both) at startup.
        w.global::<crate::SearchPane>()
            .set_search_shortcut_pref(search_shortcut_pref.into());
        // Same for the notification detail level, so Settings shows the
        // persisted level even when opened without the sidebar hydration
        // (Ctrl+, opens the Settings tab directly).
        w.global::<crate::SettingsPane>()
            .set_settings_notification_detail(notification_detail.into());

        // Set user's own avatar if available
        if let Some((pixels, width, height)) = user_avatar_decoded {
            w.global::<crate::SidebarPane>().set_user_avatar_source(
                crate::bin_ui::slint_conversions::rgba_to_slint_image(&pixels, width, height),
            );
        }

        // Set friend requests (including private/anonymous DM requests)
        // Convert Send-safe data to Slint types inside event loop
        let received_ui_slint: Vec<FriendRequestData> =
            received_ui.into_iter().map(|r| r.to_slint()).collect();
        let sent_ui_slint: Vec<FriendRequestData> =
            sent_ui.into_iter().map(|r| r.to_slint()).collect();
        let private_requests_ui_slint: Vec<FriendRequestData> = private_requests_ui
            .into_iter()
            .map(|r| r.to_slint())
            .collect();
        w.global::<crate::ModalHostState>()
            .set_friends_received_requests(ModelRc::new(VecModel::from(received_ui_slint)));
        w.global::<crate::ModalHostState>()
            .set_friends_sent_requests(ModelRc::new(VecModel::from(sent_ui_slint)));
        w.global::<crate::ModalHostState>()
            .set_friends_private_requests(ModelRc::new(VecModel::from(private_requests_ui_slint)));

        // Friends-tab list is published by `refresh_friends_list` (local
        // rows first, server rows when ListFriends returns). Setting it
        // again here raced that reconcile and put the offline snapshot
        // back on top.

        // Set blocked users
        let blocked_users_slint: Vec<crate::BlockedUserData> = blocked_users_slint
            .into_iter()
            .map(crate::bin_ui::features::modals::friends::BlockedUserRow::to_slint)
            .collect();
        w.global::<crate::ModalHostState>()
            .set_friends_blocked_users(ModelRc::new(VecModel::from(blocked_users_slint)));

        // Set sidebar badge count for message requests
        w.global::<crate::ModalHostState>()
            .set_pending_message_requests(pending_message_requests_count);
    });

    // If there's a channel, select it and load messages — desktop only. On a
    // phone the conversation list IS the home screen: selecting here opened
    // the first channel over it at every launch and, via mark_as_read, cleared
    // its unread badge before the user had seen it.
    let first_channel_id =
        first_channel_id.filter(|_| !cfg!(any(target_os = "android", target_os = "ios")));
    if let Some(channel_id) = first_channel_id {
        let channel_id_str = channel_id.to_string();

        let window_weak_select = window_weak.clone();
        let initial_client = ui_ctx.clone();
        let startup_owner = history_owner.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if !startup_owner.is_current()
                || initial_client.active_conversation()
                    != chatter_core::state::ActiveConversation::None
            {
                return;
            }
            if let Some(w) = window_weak_select.upgrade() {
                w.global::<crate::SidebarPane>()
                    .invoke_channel_selected(channel_id_str.into());
            }
        });

        // The selection callback owns the history request and its publication.
        // A second startup loader could race it or a later manual selection.
    }

    // Populate the Received/Sent friend-request tabs via the event path, which
    // decrypts each sender's ECIES envelope (real name + avatar + note). This
    // is the single source of truth — see the empty `received_ui`/`sent_ui`
    // above.
    if session_owner.is_current() {
        ui_ctx.refresh_friend_requests().await;
    }

    tracing::info!("Initial data load complete");
}

pub fn update_sidebar_typing(window: &MainWindow, id: &str, is_typing: bool) {
    let model = window.global::<crate::SidebarPane>().get_chat_rows();
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            let named = row.conversation_id.as_str() == id || row.user_id.as_str() == id;
            if named && row.is_typing != is_typing {
                row.is_typing = is_typing;
                model.set_row_data(i, row);
            }
        }
    }
}

/// Show incoming call modal with caller information.
#[cfg(feature = "call-ui")]
pub fn show_incoming_call(
    window: &MainWindow,
    call_id: &str,
    peer_id: &str,
    caller_name: &str,
    call_type: &str,
    is_identity_verified: bool,
    identity_warning: &str,
    caller_avatar: Option<&[u8]>,
) {
    use crate::IncomingCallData;

    // Decode the caller's avatar bytes to an image; falls back to initials when
    // absent or undecodable.
    let caller_avatar_source = caller_avatar
        .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
        .unwrap_or_default();

    // Create incoming call data structure
    let call_data = IncomingCallData {
        call_id: call_id.into(),
        caller_id: peer_id.into(),
        caller_name: caller_name.into(),
        caller_initials: initials_from_name(caller_name).into(),
        caller_avatar: String::new().into(),
        caller_avatar_source,
        channel_name: String::new().into(),
        is_video_call: call_type == "video",
        is_identity_verified,
        identity_warning: identity_warning.into(),
        encryption_key: String::new().into(),
        call_start_timestamp_ns: String::new().into(),
    };

    // Set call data and show modal
    window
        .global::<crate::CallView>()
        .set_incoming_call_data(call_data);
    window
        .global::<crate::CallView>()
        .set_incoming_call_minimized(false);
    window
        .global::<crate::CallView>()
        .set_incoming_call_visible(true);
}

/// Hide incoming call modal.
#[cfg(feature = "call-ui")]
pub fn hide_incoming_call(window: &MainWindow) {
    window
        .global::<crate::CallView>()
        .set_incoming_call_visible(false);
}

/// Update friend requests lists in the UI.
pub fn update_friend_requests(
    window: &MainWindow,
    received: Vec<crate::FriendRequestData>,
    sent: Vec<crate::FriendRequestData>,
) {
    use slint::{ModelRc, VecModel};

    // Update received requests
    let received_model = VecModel::from(received);
    window
        .global::<crate::ModalHostState>()
        .set_friends_received_requests(ModelRc::new(received_model));

    // Update sent requests
    let sent_model = VecModel::from(sent);
    window
        .global::<crate::ModalHostState>()
        .set_friends_sent_requests(ModelRc::new(sent_model));
}
