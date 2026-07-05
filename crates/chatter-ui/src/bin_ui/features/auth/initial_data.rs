//! Initial data loading after login.
//!
//! Uses ChatterClient services for all operations - no direct repository access.

use std::sync::Arc;

use slint::{ComponentHandle, Image, ModelRc, SharedString, VecModel, Weak, Model};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::ChannelId;

use crate::bin_ui::converters::{
    channel_to_slint, get_initials, messages_to_display_batch,
    DMListItemExt, DMListItemSendData, FriendDataExt, FriendDataSendData,
    FriendRequestExt, FriendRequestSendData,
};
use crate::{ChannelListItemData, DMListItemData, FriendData, FriendRequestData, MainWindow, PrivateGroupListItemData};

use chatter_core::domain::message::truncate_preview_text;

/// Truncate text for sidebar preview via the shared core helper (50 chars).
fn truncate_sidebar_preview(text: &str) -> String {
    truncate_preview_text(text, 50)
}

/// Rebuild the FriendsModal "Friends" tab list — accepted friendships
/// plus Active anon-DM peers — and push it into the UI. Cheap enough to
/// run on every relevant event (friend accepted, anon-DM accepted,
/// anon-DM profile updated); the modal pulls the model by reference.
pub fn refresh_friends_list(window_weak: Weak<MainWindow>, ui_ctx: Arc<ChatterClient>) {
    tokio::spawn(async move {
        let mut friends_data: Vec<FriendDataSendData> = Vec::new();
        if let Ok(pairs) = ui_ctx.list_friends_with_profiles().await {
            for (friendship, profile) in pairs {
                friends_data.push(FriendDataSendData {
                    user_id: friendship.addressee_id.to_string(),
                    display_name: profile.display_name.clone(),
                    username: String::new(),
                    avatar_bytes: profile.avatar_bytes.clone(),
                    avatar_decoded: None,
                    presence_status: profile.presence.as_str().to_string(),
                    custom_status: String::new(),
                    is_anon: false,
                });
            }
        }
        if let Ok(anon_dms) = ui_ctx.list_anon_dms() {
            for dm in anon_dms {
                if !matches!(dm.status, chatter_core::domain::anon_dm::AnonDmStatus::Active) {
                    continue;
                }
                let peer_name = dm
                    .peer_display_name
                    .clone()
                    .unwrap_or_else(|| ui_ctx.fallback_display_name(&dm.peer_id));
                friends_data.push(FriendDataSendData {
                    user_id: dm.id.to_string(),
                    display_name: peer_name,
                    username: String::new(),
                    avatar_bytes: dm.peer_avatar.clone(),
                    avatar_decoded: None,
                    presence_status: "online".to_string(),
                    custom_status: String::new(),
                    is_anon: true,
                });
            }
        }
        for f in &mut friends_data {
            f.pre_decode_avatar();
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = window_weak.upgrade() {
                let slint_list: Vec<FriendData> =
                    friends_data.into_iter().map(|f| f.to_slint()).collect();
                w.set_friends_list(ModelRc::new(VecModel::from(slint_list)));
            }
        });
    });
}

/// Load initial data after login or on startup.
pub async fn load_initial_data(window_weak: Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    tracing::info!("Loading initial data...");

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
                    ch.id, ch.name, ch.channel_type
                );
            }
            tracing::info!("Synced {} channels from server", synced_channels.len());
        }
        Err(e) => {
            tracing::warn!("Failed to sync channels from server: {}", e);
        }
    }

    // Pre-fetch anonymous tokens for sealed sender messaging
    // This ensures tokens are available before the first SKDM distribution
    if let Err(e) = ui_ctx.prefetch_tokens(10).await {
        tracing::warn!("Failed to pre-fetch anonymous tokens: {}", e);
    }

    // Refresh any anon group mailboxes nearing their 30-day expiry so the
    // user can continue receiving messages across long offline gaps.
    match ui_ctx.refresh_expiring_group_mailboxes().await {
        Ok(refreshed) if !refreshed.is_empty() => {
            tracing::info!(count = refreshed.len(), "Refreshed expiring anon group mailboxes");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("Failed to refresh anon group mailboxes: {}", e),
    }

    // Get current user ID for DM peer detection
    let current_user_id = ui_ctx.auth_state().get().current_user_id;

    // Separate channels and DMs (using Send-safe types for DMs).
    // ChannelListItemData contains a slint::Image (not Send) — hold domain
    // Channels here and convert inside the UI-thread closure below.
    let mut dm_list: Vec<DMListItemSendData> = Vec::new();

    // Anon Mode: when on, drop server-friend DMs and public/private
    // channels from the sidebar models. Anon DMs (is_anon=true rows below)
    // and anon groups stay because they have no server-side identity link.
    let anon_mode = ui_ctx.anon_mode_enabled();

    // Regular DMs are channel-free (Signal-style) — sourced from friends in
    // core, keyed by peer user_id. The UI no longer scans channels for DMs.
    if !anon_mode {
        dm_list.extend(ui_ctx.list_regular_dm_sidebar());
    }

    // Sidebar-inclusion rule (Public/Private, anon-mode aware) lives in core.
    let channel_domain_list = ui_ctx.sidebar_channels();

    // Get user profile info
    let user_display_name = ui_ctx
        .auth_state()
        .get()
        .display_name
        .clone()
        .unwrap_or_default();
    let user_initials = get_initials(&user_display_name);

    // Load user's own avatar from settings (stored during registration)
    // Pre-decode before entering UI thread to avoid blocking
    let user_avatar_decoded: Option<(Vec<u8>, u32, u32)> =
        ui_ctx.my_avatar().and_then(|bytes| {
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

    // Load anonymous DMs from local storage and add to DM list
    {
        let anon_dms = ui_ctx.list_anon_dm_conversations();
        tracing::info!("Loaded {} anonymous DM conversations", anon_dms.len());
        {
            for dm in anon_dms {
                // Get peer display name from conversation (revealed on accept/init)
                // Do NOT use user_service - these are strangers
                let peer_name = dm.peer_display_name.clone()
                    .unwrap_or_else(|| ui_ctx.fallback_display_name(&dm.peer_id));
                let peer_id_str = dm.peer_id.to_string();

                // Get latest message for preview
                let (last_message, timestamp) = match ui_ctx.get_latest_anon_dm_message(&dm.id) {
                    Some(msg) => {
                        let is_own = current_user_id.map(|id| msg.sender_id == Some(id)).unwrap_or(false);
                        let preview = msg.as_text()
                            .map(|text| {
                                if text.chars().count() > 50 {
                                    truncate_sidebar_preview(text)
                                } else if is_own {
                                    format!("You: {}", text)
                                } else {
                                    text.to_string()
                                }
                            })
                            .unwrap_or_default();
                        let ts = msg.created_at
                            .with_timezone(&chrono::Local)
                            .format("%H:%M")
                            .to_string();
                        (preview, ts)
                    }
                    // Message-less anon DM (e.g. just accepted) — show a
                    // placeholder so it isn't blank on either side after reload.
                    _ => (
                        "Conversation started".to_string(),
                        dm.updated_at.with_timezone(&chrono::Local).format("%H:%M").to_string(),
                    ),
                };

                // Anon DMs share the DM model with regular DMs — the
                // `is_anon` flag is the discriminator.
                let dm_item = DMListItemSendData {
                    channel_id: dm.id.to_string(),
                    user_id: peer_id_str.clone(),
                    display_name: peer_name.clone(),
                    avatar_bytes: dm.peer_avatar.clone(),
                    avatar_decoded: None,
                    initials: get_initials(&peer_name),
                    presence_status: if dm.status == chatter_core::domain::anon_dm::AnonDmStatus::Active {
                        "online".to_string()
                    } else {
                        "offline".to_string()
                    },
                    last_message,
                    timestamp,
                    unread_count: dm.unread_count,
                    is_typing: false,
                    is_anon: true,
                };
                dm_list.push(dm_item);
            }
        }
    }

    // Load pending anon DM requests (message requests) - show as pending in sidebar and modal
    let mut private_requests_ui: Vec<FriendRequestSendData> = Vec::new();
    {
        let requests = ui_ctx.list_anon_dm_message_requests();
        {
            tracing::info!("Loaded {} pending anon DM requests", requests.len());
            for dm in requests {
                // Get peer display name from conversation (revealed after ECIES decryption)
                // Do NOT use user_service - these are strangers
                let peer_name = dm.peer_display_name.clone()
                    .unwrap_or_else(|| ui_ctx.fallback_display_name(&dm.peer_id));
                let peer_id_str = dm.peer_id.to_string();

                // Get intro message if any
                let last_message = match ui_ctx.get_latest_anon_dm_message(&dm.id) {
                    Some(msg) => msg.as_text().unwrap_or_default().to_string(),
                    None => "Message request".to_string(),
                };

                // Add to DM sidebar
                let dm_item = DMListItemSendData {
                    channel_id: dm.id.to_string(),
                    user_id: peer_id_str.clone(),
                    display_name: format!("\u{1F4E9} {}", peer_name), // Envelope emoji for pending
                    avatar_bytes: dm.peer_avatar.clone(),
                    avatar_decoded: None,
                    initials: get_initials(&peer_name),
                    presence_status: "pending".to_string(),
                    last_message: last_message.clone(),
                    timestamp: dm.created_at
                        .with_timezone(&chrono::Local)
                        .format("%H:%M")
                        .to_string(),
                    unread_count: 1,
                    is_typing: false,
                    is_anon: true,
                };
                dm_list.push(dm_item);

                // Add to friends modal private requests list (use conversation_id as user_id)
                private_requests_ui.push(FriendRequestSendData {
                    user_id: dm.id.to_string(),
                    display_name: peer_name,
                    username: "Private".to_string(),
                    avatar_bytes: dm.peer_avatar.clone(),
                    avatar_decoded: None,
                    sent_at: last_message,
                });
            }
        }
    }

    // Load outgoing pending anon DM requests (requests we sent but haven't been accepted)
    {
        let requests = ui_ctx.list_anon_dm_outgoing_requests();
        tracing::info!("Loaded {} outgoing anon DM requests", requests.len());
        {
            for dm in requests {
                // Get peer display name from conversation
                let peer_name = dm.peer_display_name.clone()
                    .unwrap_or_else(|| ui_ctx.fallback_display_name(&dm.peer_id));
                let peer_id_str = dm.peer_id.to_string();

                // Get our last sent message for preview
                let (last_message, timestamp) = match ui_ctx.get_latest_anon_dm_message(&dm.id) {
                    Some(msg) => {
                        let preview = msg.as_text()
                            .map(|text| format!("You: {}", truncate_sidebar_preview(text)))
                            .unwrap_or_else(|| "Message request sent".to_string());
                        let ts = msg.created_at
                            .with_timezone(&chrono::Local)
                            .format("%H:%M")
                            .to_string();
                        (preview, ts)
                    }
                    _ => ("Waiting for response...".to_string(), dm.created_at.with_timezone(&chrono::Local).format("%H:%M").to_string()),
                };

                // Add to DM sidebar with "Waiting" indicator
                let dm_item = DMListItemSendData {
                    channel_id: dm.id.to_string(),
                    user_id: peer_id_str.clone(),
                    display_name: format!("\u{1F4EE} {} (pending)", peer_name), // Postbox emoji
                    avatar_bytes: dm.peer_avatar.clone(),
                    avatar_decoded: None,
                    initials: get_initials(&peer_name),
                    presence_status: "offline".to_string(),
                    last_message,
                    timestamp,
                    unread_count: 0,
                    is_typing: false,
                    is_anon: true,
                };
                dm_list.push(dm_item);
            }
        }
    }

    // Load private groups from local storage
    let private_groups: Vec<PrivateGroupListItemData> = match ui_ctx.list_anon_groups() {
        Ok(groups) => groups
            .into_iter()
            .map(|g| {
                let member_count = ui_ctx.get_anon_group_members(&g.id).len() as i32;

                // Get the latest message for preview
                let (last_message, timestamp) = match ui_ctx.get_latest_anon_group_message(&g.id) {
                    Some(msg) => {
                        let preview = msg.as_text()
                            .map(|text| truncate_sidebar_preview(text))
                            .unwrap_or_default();
                        let ts = msg.created_at
                            .with_timezone(&chrono::Local)
                            .format("%H:%M")
                            .to_string();
                        (preview, ts)
                    }
                    _ => (String::new(), String::new()),
                };

                PrivateGroupListItemData {
                    group_id: g.id.to_string().into(),
                    name: g.name.into(),
                    member_count,
                    unread_count: 0, // TODO: track unread
                    last_message: last_message.into(),
                    timestamp: timestamp.into(),
                    is_typing: false,
                    is_selected: false,
                }
            })
            .collect(),
        Err(e) => {
            tracing::warn!("Failed to load private groups: {}", e);
            Vec::new()
        }
    };

    tracing::info!("Loaded {} private groups", private_groups.len());

    // Load blocked users for the Blocked tab
    let users = ui_ctx.get_anon_dm_blocked_users();
    tracing::info!("Loaded {} blocked users", users.len());
    let blocked_users_data: Vec<(String, String)> = users.into_iter()
        .map(|user_id| {
            let id_str = user_id.to_string();
            let display_name = format!("User {}", &id_str[..8.min(id_str.len())]);
            (id_str, display_name)
        })
        .collect();

    // Pre-decode avatars before entering UI thread to avoid blocking
    let mut dm_list = dm_list;
    for dm in &mut dm_list {
        dm.pre_decode_avatar();
    }
    let mut private_requests_ui = private_requests_ui;
    for req in &mut private_requests_ui {
        req.pre_decode_avatar();
    }

    // Build the Friends-tab list — unified over accepted server friendships
    // and Active anon-DM peers. Both surface in the FriendsModal's "Friends"
    // tab; the `is_anon` flag discriminates so anon rows route to the anon
    // chat and render the peer-supplied avatar bytes.
    let mut friends_data: Vec<FriendDataSendData> = Vec::new();
    match ui_ctx.list_friends_with_profiles().await {
        Ok(pairs) => {
            for (friendship, profile) in pairs {
                // Profile carries display_name; username is
                // on the User entity. We don't need it here — the modal
                // only shows display_name prominently.
                friends_data.push(FriendDataSendData {
                    user_id: friendship.addressee_id.to_string(),
                    display_name: profile.display_name.clone(),
                    username: String::new(),
                    avatar_bytes: profile.avatar_bytes.clone(),
                    avatar_decoded: None,
                    presence_status: profile.presence.as_str().to_string(),
                    custom_status: String::new(),
                    is_anon: false,
                });
            }
        }
        Err(e) => {
            tracing::warn!("Failed to load accepted friends: {}", e);
        }
    }
    if let Ok(anon_dms) = ui_ctx.list_anon_dms() {
        for dm in anon_dms {
            if !matches!(dm.status, chatter_core::domain::anon_dm::AnonDmStatus::Active) {
                continue;
            }
            let peer_name = dm
                .peer_display_name
                .clone()
                .unwrap_or_else(|| ui_ctx.fallback_display_name(&dm.peer_id));
            friends_data.push(FriendDataSendData {
                user_id: dm.id.to_string(),
                display_name: peer_name,
                username: String::new(),
                avatar_bytes: dm.peer_avatar.clone(),
                avatar_decoded: None,
                presence_status: "online".to_string(),
                custom_status: String::new(),
                is_anon: true,
            });
        }
    }
    for f in &mut friends_data {
        f.pre_decode_avatar();
    }

    // Update UI with channels, DMs, private groups, and friend requests
    let window_weak_ui = window_weak.clone();
    let pending_message_requests_count = private_requests_ui.len() as i32;
    let ui_ctx_for_refresh = ui_ctx.clone();
    let ui_ctx_for_convert = ui_ctx.clone();
    let search_shortcut_pref = ui_ctx.get_search_shortcut().as_str();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = window_weak_ui.upgrade() {
            let channel_list: Vec<ChannelListItemData> = channel_domain_list
                .iter()
                .map(|c| channel_to_slint(c, &ui_ctx_for_convert))
                .collect();
            w.set_channels(ModelRc::new(VecModel::from(channel_list)));
            // Convert Send-safe DM data to Slint types inside event loop
            let dm_list_slint: Vec<DMListItemData> = dm_list.into_iter().map(|d| d.to_slint()).collect();
            w.set_dm_conversations(ModelRc::new(VecModel::from(dm_list_slint)));
            w.set_private_groups(ModelRc::new(VecModel::from(private_groups)));

            // Apply selection flags on newly-loaded models.
            crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx_for_refresh);

            // Compute initial total unread count for mobile bottom nav badge
            crate::bin_ui::features::auth::inbox::domain_handler::recompute_total_unread(&w, &ui_ctx_for_refresh);

            w.set_user_display_name(user_display_name.into());
            w.set_user_initials(user_initials.into());
            w.set_user_presence_status("online".into());

            // Hydrate the persisted search-shortcut preference so the
            // FocusScope picks the right combo (Ctrl+K / Ctrl+F / Both) at startup.
            w.set_search_shortcut_pref(search_shortcut_pref.into());

            // Set user's own avatar if available
            if let Some((pixels, width, height)) = user_avatar_decoded {
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &pixels, width, height,
                );
                w.set_user_avatar_source(Image::from_rgba8(buffer));
            }

            // Set friend requests (including private/anonymous DM requests)
            // Convert Send-safe data to Slint types inside event loop
            let received_ui_slint: Vec<FriendRequestData> = received_ui.into_iter().map(|r| r.to_slint()).collect();
            let sent_ui_slint: Vec<FriendRequestData> = sent_ui.into_iter().map(|r| r.to_slint()).collect();
            let private_requests_ui_slint: Vec<FriendRequestData> = private_requests_ui.into_iter().map(|r| r.to_slint()).collect();
            w.set_friends_received_requests(ModelRc::new(VecModel::from(received_ui_slint)));
            w.set_friends_sent_requests(ModelRc::new(VecModel::from(sent_ui_slint)));
            w.set_friends_private_requests(ModelRc::new(VecModel::from(private_requests_ui_slint)));

            // Friends-tab list — accepted friendships + Active anon-DM peers.
            let friends_slint: Vec<FriendData> = friends_data.into_iter().map(|f| f.to_slint()).collect();
            w.set_friends_list(ModelRc::new(VecModel::from(friends_slint)));

            // Set blocked users
            let blocked_users_slint: Vec<crate::BlockedUserData> = blocked_users_data
                .into_iter()
                .map(|(user_id, display_name)| crate::BlockedUserData {
                    user_id: user_id.into(),
                    display_name: display_name.into(),
                    username: "Private".into(),
                    blocked_at: SharedString::default(),
                })
                .collect();
            w.set_friends_blocked_users(ModelRc::new(VecModel::from(blocked_users_slint)));

            // Set sidebar badge count for message requests
            w.set_pending_message_requests(pending_message_requests_count);
        }
    });

    // If there's a channel, select it and load messages
    if let Some(channel_id) = first_channel_id {
        let channel_id_str = channel_id.to_string();

        let window_weak_select = window_weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = window_weak_select.upgrade() {
                w.invoke_channel_selected(channel_id_str.into());
            }
        });

        let current_user_id_str = current_user_id.map(|id| id.to_string());
        load_channel_into_ui_from_services(window_weak, &channel_id, ui_ctx, current_user_id_str)
            .await;
    }

    // Populate the Received/Sent friend-request tabs via the event path, which
    // decrypts each sender's ECIES envelope (real name + avatar + note). This
    // is the single source of truth — see the empty `received_ui`/`sent_ui`
    // above.
    ui_ctx.refresh_friend_requests().await;

    tracing::info!("Initial data load complete");
}

/// Load channel data and messages into UI using services (no direct repo access).
async fn load_channel_into_ui_from_services(
    window_weak: Weak<MainWindow>,
    channel_id: &ChannelId,
    ui_ctx: &Arc<ChatterClient>,
    current_user_id: Option<String>,
) {
    // Get channel info from service
    let channel = match ui_ctx.get_channel_local(channel_id) {
        Some(ch) => ch,
        None => {
            tracing::warn!("Channel {} not found in local storage", channel_id);
            let weak = window_weak;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_loading_messages(false);
                }
            });
            return;
        }
    };

    // Use the channel's own conversation_type() which respects require_encryption
    let conversation_type = channel.conversation_type();

    // Fetch recent messages (limit 50)
    let messages = match ui_ctx.get_messages_local_asc(channel_id, &conversation_type, None, 50) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::error!("Failed to get messages for channel {}: {}", channel_id, e);
            Vec::new()
        }
    };

    // Convert messages to display data
    let display_messages = messages_to_display_batch(
        &messages,
        |m| ui_ctx.message_sender_display_name(m),
        |att_id| ui_ctx.get_attachment_metadata(att_id),
        |m| ui_ctx.reply_author_display_name(m),
        |m| ui_ctx.reply_parent_missing_for(m),
    );

    // Get current user ID for is_own calculation in to_slint_with_user
    let current_user_id_for_slint = current_user_id.clone().unwrap_or_default();

    // Update UI
    let is_saved = matches!(channel.channel_type, chatter_core::domain::channel::ChannelType::SavedMessages);
    let channel_name = if is_saved { "Saved Messages".to_string() } else { channel.name.clone() };
    let channel_id_str = channel_id.to_string();

    let ui_ctx_load = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = window_weak.upgrade() {
            // Convert MessageDisplayData to MessageItemData on UI thread
            let slint_messages: Vec<_> = display_messages
                .iter()
                .map(|m| m.to_slint_with_user(&current_user_id_for_slint))
                .collect();

            w.set_channel_name(channel_name.into());
            let _ = channel_id_str;
            w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
            w.set_loading_messages(false);

            // Auto-register GIF animations for inline playback
            crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                w.as_weak(),
                ui_ctx_load.clone(),
            );
        }
    });
}

/// Refresh the DM list in the UI from core.
///
/// Core's `list_dm_sidebar` is the single source of truth — it builds regular
/// DMs *and* anon DMs, applies the anon-mode filter, and orders them. The UI
/// just renders. Do not pair this with a second list-writer on the same event;
/// two writers race and one clobbers a freshly-accepted DM.
pub fn refresh_dm_list(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let window_weak = window.as_weak();

    tokio::spawn(async move {
        // The channel-free DM sidebar is built from the LOCAL friends table.
        // Sync friends from the server first — `list_friends_with_profiles`
        // persists each accepted friend into local storage — so the sidebar
        // reflects server truth even when a friendship was just accepted on the
        // other device (whose local row we'd otherwise never have written).
        let _ = ui_ctx.list_friends_with_profiles().await;

        // DB work off the UI thread.
        let dm_list = ui_ctx.list_dm_sidebar();

        // Marshal the model mutation back onto the UI thread. Slint models must
        // only be mutated from the event loop; an off-thread `set_*` is silently
        // dropped, which is why a refreshed DM never rendered. `to_slint`
        // (builds a non-Send `Image`) also has to run inside the closure.
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = window_weak.upgrade() {
                let dm_list_slint: Vec<DMListItemData> = dm_list.into_iter()
                    .map(|mut d| {
                        // Decode avatar bytes → RGBA so to_slint builds an
                        // Image; without this the sidebar shows initials only.
                        d.pre_decode_avatar();
                        d.to_slint()
                    })
                    .collect();
                w.set_dm_conversations(ModelRc::new(VecModel::from(dm_list_slint)));
                crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx);
            }
        });
    });
}

/// Refresh the anonymous groups list in the UI.
pub fn refresh_anon_groups(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let window_weak = window.as_weak();

    tokio::spawn(async move {
        let mut private_group_list: Vec<PrivateGroupListItemData> = Vec::new();

        // Load anonymous groups
        match ui_ctx.list_anon_groups() {
            Ok(groups) => {
                for group in groups {
                    let group_item = PrivateGroupListItemData {
                        group_id: SharedString::from(group.id.to_string()),
                        name: SharedString::from(group.name.clone()),
                        member_count: 0, // Could query member count from storage if needed
                        unread_count: 0,
                        last_message: SharedString::from(""),
                        timestamp: SharedString::from(""),
                        is_typing: false,
                        is_selected: false,
                    };
                    private_group_list.push(group_item);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to load anonymous groups: {}", e);
            }
        }

        // Update UI on the main thread
        if let Some(w) = window_weak.upgrade() {
            let private_group_model = VecModel::from(private_group_list);
            w.set_private_groups(ModelRc::new(private_group_model));
            crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx);
        }
    });
}

/// Update typing indicator for a specific user in a DM.
pub fn update_dm_typing_indicator(window: &MainWindow, user_id: &str, is_typing: bool) {
    let dm_list = window.get_dm_conversations();
    
    // Find and update the specific DM
    let dm_count = dm_list.iter().count();
    for i in 0..dm_count {
        if let Some(dm) = dm_list.iter().nth(i) {
            if dm.user_id.to_string() == user_id {
                // Need to recreate the model to update
                let mut all_dms = Vec::new();
                for j in 0..dm_count {
                    if let Some(mut item) = dm_list.iter().nth(j) {
                        if j == i {
                            item.is_typing = is_typing;
                        }
                        all_dms.push(item);
                    }
                }
                window.set_dm_conversations(ModelRc::new(VecModel::from(all_dms)));
                break;
            }
        }
    }
}

/// Show incoming call modal with caller information.
pub fn show_incoming_call(
    window: &MainWindow,
    call_id: &str,
    peer_id: &str,
    caller_name: &str,
    call_type: &str,
    is_identity_verified: bool,
    identity_warning: &str,
    is_anonymous: bool,
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
        caller_initials: crate::bin_ui::converters::get_initials(caller_name).into(),
        caller_avatar: String::new().into(),
        caller_avatar_source,
        channel_name: String::new().into(),
        is_video_call: call_type == "video",
        is_anonymous_call: is_anonymous,
        is_identity_verified,
        identity_warning: identity_warning.into(),
        encryption_key: String::new().into(),
        call_start_timestamp_ns: String::new().into(),
    };
    
    // Set call data and show modal
    window.set_incoming_call_data(call_data);
    window.set_incoming_call_visible(true);
}

/// Hide incoming call modal.
pub fn hide_incoming_call(window: &MainWindow) {
    window.set_incoming_call_visible(false);
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
    window.set_friends_received_requests(ModelRc::new(received_model));
    
    // Update sent requests
    let sent_model = VecModel::from(sent);
    window.set_friends_sent_requests(ModelRc::new(sent_model));
}
