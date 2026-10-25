//! Friend request, presence, and profile update handlers.

use slint::ComponentHandle;
use std::sync::Arc;

use slint::{Model, Weak};

use crate::MainWindow;
use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::UserId;

/// Handle friend requests updated event.
///
/// Received requests may carry an ECIES-encrypted intro note. Decryption is
/// async and must NOT run inside the Slint event-loop closure, so we decrypt
/// every received note up front (here, off the UI thread) and hand the
/// pre-decrypted plaintext into the closure. Undecryptable/empty notes render
/// nothing (we never surface ciphertext).
pub(super) async fn handle_friend_requests_updated(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    received: Vec<chatter_core::inbox::events::FriendRequestInfo>,
    sent: Vec<chatter_core::inbox::events::FriendRequestInfo>,
) {
    let weak = window.clone();
    // Requests from someone we blocked stay pending server-side, unseen here.
    let received = ui_ctx.visible_friend_requests(received);
    let received_count = received.len();
    let sent_count = sent.len();

    // Received requests carry an ECIES envelope (sender's profile key + optional
    // note). Ingest each off the UI thread: persist the key, then decrypt the
    // sender's real display name + avatar + note. The non-Send `slint::Image`
    // is built inside the event loop from the decoded RGBA collected here.
    struct Prepped {
        display_name: String,
        avatar: Option<(u32, u32, Vec<u8>)>,
        note: String,
    }
    // Core resolves the name — never the server display_name, which is
    // ciphertext without the peer's profile key.
    let peer_name = |r: &chatter_core::inbox::events::FriendRequestInfo| {
        r.user_id
            .parse::<UserId>()
            .map(|u| ui_ctx.friend_request_peer_name(&u, &r.username))
            .unwrap_or_default()
    };
    let sent_names: Vec<String> = sent
        .iter()
        .map(|r| {
            r.user_id
                .parse::<UserId>()
                .map(|u| ui_ctx.outgoing_friend_request_peer_name(&u, &r.username))
                .unwrap_or_else(|_| "Pending request".to_string())
        })
        .collect();
    // Outgoing rows show the peer's avatar when a local profile already has it.
    let sent_avatars: Vec<Option<(u32, u32, Vec<u8>)>> = sent
        .iter()
        .map(|r| {
            r.user_id
                .parse::<UserId>()
                .ok()
                .and_then(|u| ui_ctx.user_avatar(&u))
                .as_deref()
                .and_then(decode_avatar_rgba)
        })
        .collect();
    let mut prepped: Vec<Prepped> = Vec::with_capacity(received.len());
    for r in &received {
        let p = match r.user_id.parse::<UserId>() {
            Ok(sender_id) => {
                let profile = ui_ctx
                    .ingest_received_friend_request(&sender_id, &r.encrypted_intro)
                    .await;
                Prepped {
                    display_name: profile.display_name.unwrap_or_else(|| {
                        ui_ctx.friend_request_peer_name(&sender_id, &r.username)
                    }),
                    avatar: profile.avatar_bytes.as_deref().and_then(decode_avatar_rgba),
                    note: profile.note.unwrap_or_default(),
                }
            }
            Err(_) => Prepped {
                display_name: peer_name(r),
                avatar: None,
                note: String::new(),
            },
        };
        prepped.push(p);
    }

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Update friend request lists
            let received_model: Vec<crate::FriendRequestData> = received
                .into_iter()
                .zip(prepped)
                .map(|(r, p)| {
                    let (avatar_source, has_avatar) = match p.avatar {
                        Some((aw, ah, px)) => (
                            crate::bin_ui::slint_conversions::rgba_to_slint_image(&px, aw, ah),
                            true,
                        ),
                        None => (slint::Image::default(), false),
                    };
                    crate::FriendRequestData {
                        user_id: r.user_id.into(),
                        display_name: p.display_name.into(),
                        username: r.username.into(),
                        avatar_source,
                        has_avatar,
                        sent_at: "".into(),
                        note: p.note.into(),
                    }
                })
                .collect();

            let sent_model: Vec<crate::FriendRequestData> = sent
                .into_iter()
                .zip(sent_names)
                .zip(sent_avatars)
                .map(|((r, display_name), avatar)| {
                    let (avatar_source, has_avatar) = match avatar {
                        Some((aw, ah, px)) => (
                            crate::bin_ui::slint_conversions::rgba_to_slint_image(&px, aw, ah),
                            true,
                        ),
                        None => (slint::Image::default(), false),
                    };
                    // Name resolved by core without exposing an opaque recipient ID.
                    crate::FriendRequestData {
                        user_id: r.user_id.into(),
                        display_name: display_name.into(),
                        username: r.username.into(),
                        avatar_source,
                        has_avatar,
                        sent_at: if r.created_at_ns > 0 {
                            crate::bin_ui::converters::server_record_time(r.created_at_ns).into()
                        } else {
                            "".into()
                        },
                        note: "".into(),
                    }
                })
                .collect();

            // Update friend requests in UI
            super::super::super::initial_data::update_friend_requests(
                &w,
                received_model,
                sent_model,
            );
        }
    });

    tracing::debug!(
        received = received_count,
        sent = sent_count,
        "Updated friend requests in UI"
    );
}

/// Decode raw avatar image bytes → RGBA (width, height, pixels) for building a
/// `slint::Image` on the UI thread. `None` if the bytes don't decode.
fn decode_avatar_rgba(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((w, h, rgba.into_raw()))
}

/// Handle friend accepted event.
///
/// DMs are channel-free (Signal-style): there is no server DM channel to
/// fetch. The sidebar is built directly from the friends list (see
/// `list_dm_sidebar`), so a newly-accepted friend appears as soon as we
/// refresh it — no local conversation row needs to be created first.
pub(super) fn handle_friend_accepted(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    friend_id: UserId,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Single source of truth: the chat list is rebuilt from core in
            // one pass.
            crate::bin_ui::features::sidebar::refresh_chat_list(&w, &ui_ctx);
            tracing::info!(
                friend_id = %friend_id,
                "Friend accepted - refreshed DM sidebar"
            );
        }
    });
}

/// Handle user presence changed event.
pub(super) fn handle_presence_changed(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    user_id: UserId,
    presence: chatter_core::domain::user::PresenceStatus,
    timestamp: u64,
) {
    let weak = window.clone();
    let user_id_str = user_id.to_string();
    // Our own presence arrives here too — `set_presence` emits it locally,
    // since the server fans presence out to friends and not back to us.
    let is_self = ui_ctx.session_current_user_id() == Some(user_id);
    // A blocked peer's live presence is hidden like their stored one.
    let presence = if is_self {
        presence
    } else {
        ui_ctx.peer_display_presence(&user_id, presence)
    };
    let presence_str = presence.as_str();

    let presence_slint: slint::SharedString = presence_str.into();
    let last_seen_label: slint::SharedString = ui_ctx.peer_last_seen(&user_id)
        .map(|(_, at)| crate::bin_ui::converters::last_seen_label(presence, at))
        .unwrap_or_default()
        .into();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        // 0. Our own dot — profile strip and collapsed-rail avatar both read
        // this one property. Nothing below matches self (we are not our own
        // DM row or contact), so this is the whole self case.
        if is_self {
            w.global::<crate::SidebarPane>()
                .set_user_presence_status(presence_slint.clone());
            if w.global::<crate::ModalHostState>().get_profile_is_self() {
                w.global::<crate::ModalHostState>()
                    .set_profile_presence(presence_slint.clone());
            }
            return;
        }

        // 1. Update sidebar DM presence dots
        let dms = w.global::<crate::SidebarPane>().get_chat_rows();
        for i in 0..dms.row_count() {
            if let Some(mut dm) = dms.row_data(i) {
                if dm.user_id == user_id_str {
                    dm.presence_status = presence_slint.clone();
                    dms.set_row_data(i, dm);
                    crate::bin_ui::features::sidebar::refresh_flat_rows(&w);
                    break;
                }
            }
        }

        // 2. Update chat header presence indicator for active DM.
        // Read is_selected (Rust-maintained) from the DM model — avoids a
        // stringly-typed selected-id comparison.
        if w.global::<crate::ChatPane>().get_is_dm() {
            for i in 0..dms.row_count() {
                if let Some(dm) = dms.row_data(i) {
                    if dm.is_selected && dm.user_id == user_id_str {
                        w.global::<crate::ChatPane>()
                            .set_header_presence_status(presence_slint.clone());
                        break;
                    }
                }
            }
        }

        // 3. Update profile modal presence if showing this user
        if !w
            .global::<crate::ModalHostState>()
            .get_profile_scoped_member()
            && w.global::<crate::ModalHostState>().get_profile_user_id() == user_id_str
        {
            w.global::<crate::ModalHostState>()
                .set_profile_presence(presence_slint.clone());
        }

        // 4. Contacts tab rows. Same in-place patch as the DM rows above —
        // without it the Friends list only ever showed the presence it was
        // built with, so a contact coming online was visible in Chats and
        // stale in Contacts at the same time. Anon rows carry an anon_dm_id
        // in `user_id`, never a real one, so they never match.
        let friends = w.global::<crate::ModalHostState>().get_friends_list();
        for i in 0..friends.row_count() {
            if let Some(mut f) = friends.row_data(i) {
                if !f.is_anon && f.user_id == user_id_str {
                    f.presence_status = presence_slint.clone();
                    f.last_seen_label = last_seen_label.clone();
                    friends.set_row_data(i, f);
                    crate::bin_ui::features::modals::friends::refresh_new_chat_results(&w);
                    break;
                }
            }
        }

        tracing::debug!(
            user_id = %user_id_str,
            presence = presence_str,
            timestamp,
            "Updated presence in UI"
        );
    });

    tracing::info!(
        user_id = %user_id,
        presence = ?presence,
        timestamp,
        "User presence updated"
    );
}

/// Handle anonymous DM profile updated event.
pub(super) fn handle_anon_dm_profile_updated(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<chatter_core::client::ChatterClient>,
    conversation_id: chatter_core::domain::anon_dm::AnonDmId,
    user_id: UserId,
    display_name: Option<String>,
    avatar: Option<Vec<u8>>,
    avatar_url: Option<String>,
    bio: Option<String>,
    timestamp: u64,
) {
    let weak = window.clone();
    let channel_id_str = conversation_id.to_string();
    let display_name_clone = display_name.clone();

    // Update display name in sidebar immediately
    let weak_for_name = weak.clone();
    let conv_id_for_name = channel_id_str.clone();
    let name_for_ui = display_name_clone.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_for_name.upgrade() {
            let dms = w.global::<crate::SidebarPane>().get_chat_rows();
            for i in 0..dms.row_count() {
                if let Some(mut dm) = dms.row_data(i) {
                    if dm.conversation_id == conv_id_for_name {
                        if let Some(name) = &name_for_ui {
                            dm.display_name = name.clone().into();
                        }
                        dms.set_row_data(i, dm);
                        crate::bin_ui::features::sidebar::refresh_flat_rows(&w);
                        break;
                    }
                }
            }
        }
    });

    // Update avatar in sidebar
    if let Some(avatar_bytes) = avatar {
        // Use decrypted bytes from the event (most efficient)
        let weak_for_avatar = weak.clone();
        let channel_id_for_avatar = channel_id_str.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_for_avatar.upgrade() {
                update_dm_avatar(&w, &channel_id_for_avatar, &avatar_bytes);
                update_pending_anon_dm_avatar(&w, &channel_id_for_avatar, &avatar_bytes);
            }
        });
    } else if avatar_url.is_some() {
        // CRYPTO-2: a peer-supplied avatar URL is NOT fetched. It used to be
        // GETted with no user interaction, no scheme allowlist, no size cap, no
        // digest and, off the Tor path, the system root store instead of the
        // pinned CA — with the host chosen by an anonymous peer. The inline,
        // conversation-key-encrypted avatar above is the supported path; a URL
        // is ignored.
        tracing::debug!(
            conversation_id = %conversation_id,
            "Ignoring peer-supplied avatar URL in anon-DM profile update (inline avatar only)"
        );
    } else {
        // No inline avatar or URL — check if the DB already has it (fallback)
        let weak_for_avatar = weak.clone();
        let channel_id_for_avatar = channel_id_str.clone();

        if let Some(dm) = ui_ctx.get_anon_dm_conversation(&conversation_id) {
            if let Some(ref avatar_bytes) = dm.peer_avatar {
                let bytes = avatar_bytes.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_for_avatar.upgrade() {
                        update_dm_avatar(&w, &channel_id_for_avatar, &bytes);
                        update_pending_anon_dm_avatar(&w, &channel_id_for_avatar, &bytes);
                    }
                });
            }
        }
    }

    tracing::info!(
        conversation_id = %conversation_id,
        user_id = %user_id,
        ?display_name,
        ?avatar_url,
        ?bio,
        timestamp,
        "Anonymous DM profile updated"
    );
}

/// Update a DM's avatar in the sidebar. Must be called on the UI thread.
fn update_dm_avatar(w: &MainWindow, channel_id: &str, bytes: &[u8]) {
    let dms = w.global::<crate::SidebarPane>().get_chat_rows();
    for i in 0..dms.row_count() {
        if let Some(mut dm) = dms.row_data(i) {
            if dm.conversation_id == channel_id {
                if let Some(img) = crate::bin_ui::converters::avatar_bytes_to_image(bytes) {
                    dm.avatar_source = img;
                    dm.has_avatar = true;
                    dms.set_row_data(i, dm);
                    crate::bin_ui::features::sidebar::refresh_flat_rows(w);
                }
                break;
            }
        }
    }
}

/// A fetched request avatar arrives after the request row was first shown.
fn update_pending_anon_dm_avatar(w: &MainWindow, conversation_id: &str, bytes: &[u8]) {
    let requests = w
        .global::<crate::ModalHostState>()
        .get_friends_private_requests();
    for i in 0..requests.row_count() {
        if let Some(mut request) = requests.row_data(i) {
            if request.user_id == conversation_id {
                if let Some(image) = crate::bin_ui::converters::avatar_bytes_to_image(bytes) {
                    request.avatar_source = image;
                    request.has_avatar = true;
                    requests.set_row_data(i, request);
                }
                break;
            }
        }
    }
}

/// Handle server-managed user profile updated event.
///
/// Updates display name + status in the open chat header, message rows,
/// the channel member list and the profile modal in real time; the chat list
/// is rebuilt from core by the caller. `own` (decided in core) also
/// repaints this account's own name, initials and avatar — the sidebar footer,
/// Profile tab and Settings account card all read `SidebarPane`.
pub(super) fn handle_user_profile_updated(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    own: bool,
    user_id: UserId,
    display_name: String,
    status_message: Option<String>,
    avatar_bytes: Option<Vec<u8>>,
    timestamp: u64,
) {
    tracing::info!(
        user_id = %user_id,
        %display_name,
        ?status_message,
        timestamp,
        "Server-managed user profile updated"
    );

    let weak = window.clone();
    // Same resolution the DM open path uses (core already stored the profile).
    let header_name: Option<slint::SharedString> = (ui_ctx.active_conversation()
        == chatter_core::state::ActiveConversation::ServerDm { peer_id: user_id })
    .then(|| ui_ctx.display_name(&user_id).into());
    let user_id_str = user_id.to_string();
    let stored_name = ui_ctx.display_name(&user_id);
    let display_name_slint: slint::SharedString = stored_name.clone().into();
    let initials = crate::bin_ui::converters::initials_from_name(&stored_name);
    let client = ui_ctx.clone();

    // Decode the avatar to raw RGBA now (Send-safe); the non-Send `slint::Image`
    // is built inside the event-loop closure below, for the own-profile
    // surfaces, the open header and the profile sheet.
    let avatar_decoded: Option<(u32, u32, Vec<u8>)> = avatar_bytes.as_ref().and_then(|b| {
        image::load_from_memory(b).ok().map(|img| {
            let rgba = img.to_rgba8();
            let (aw, ah) = rgba.dimensions();
            (aw, ah, rgba.into_raw())
        })
    });

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        let avatar_img: Option<slint::Image> = avatar_decoded.as_ref().map(|(aw, ah, px)| {
            crate::bin_ui::slint_conversions::rgba_to_slint_image(px, *aw, *ah)
        });

        if own {
            let sidebar = w.global::<crate::SidebarPane>();
            sidebar.set_user_display_name(display_name_slint.clone());
            sidebar.set_user_initials(initials.clone().into());
            if let Some(img) = &avatar_img {
                sidebar.set_user_avatar_source(img.clone());
            }
            if w.global::<crate::ModalHostState>().get_profile_user_id() == user_id_str {
                w.global::<crate::ModalHostState>()
                    .set_profile_initials(initials.clone().into());
            }
        }

        // 1. Chat-list rows are not patched here: the caller rebuilds them
        // from core (`rebuild_chat_list`), which names a friend DM from the
        // stored profile. Patching by `user_id` hit the first row for this
        // peer — a secret chat with them carries the same `user_id` — so the
        // secret chat took the friend name and the DM row kept the old one.

        // Message rows and a currently open channel roster are snapshots.
        // Resolve names from core, including after the app has been open for
        // hours, instead of keeping the name captured when each row loaded.
        let members = w
            .global::<crate::ModalHostState>()
            .get_channel_settings_members();
        for i in 0..members.row_count() {
            if let Some(mut member) = members.row_data(i) {
                if member.user_id == user_id_str {
                    member.display_name = display_name_slint.clone();
                    members.set_row_data(i, member);
                }
            }
        }

        // A join notice may have been rendered before this profile was known.
        // Ask core for the stored notice and its current sender name so the
        // visible row catches up without changing the message on disk.
        let messages = w.global::<crate::ChatPane>().get_messages();
        for i in 0..messages.row_count() {
            let Some(mut row) = messages.row_data(i) else {
                continue;
            };
            if row.sender_id.as_str() != user_id_str.as_str() {
                continue;
            }
            if !row.is_system {
                row.sender_name = display_name_slint.clone();
                row.sender_initials = initials.clone().into();
                messages.set_row_data(i, row);
                continue;
            }
            let Ok(message_id) = row.message_id.parse() else {
                continue;
            };
            let Some((name, content)) = client.refreshed_join_notice(&message_id, &user_id) else {
                continue;
            };
            row.sender_name = name.clone().into();
            row.sender_initials = crate::bin_ui::converters::initials_from_name(&name).into();
            (row.content, row.rich_content) = crate::bin_ui::converters::body_fields(&content, &[], false);
            messages.set_row_data(i, row);
        }

        // 2. Chat header, when this peer's DM is the open conversation. Asked
        // of core, not the sidebar model: a DM filed into a folder has no row
        // there, and its header kept the name it opened with.
        if let Some(name) = header_name {
            w.global::<crate::ChatPane>().set_channel_name(name);
            if let Some(img) = &avatar_img {
                w.global::<crate::ChatPane>()
                    .set_channel_avatar_source(img.clone());
                w.global::<crate::ChatPane>().set_channel_has_avatar(true);
            }
        }

        // 3. Profile modal if currently showing this user.
        if !w
            .global::<crate::ModalHostState>()
            .get_profile_scoped_member()
            && w.global::<crate::ModalHostState>().get_profile_user_id() == user_id_str
        {
            w.global::<crate::ModalHostState>()
                .set_profile_display_name(display_name_slint.clone());
            // `status_message` is the bio Edit Profile writes (profile.rs), so it
            // belongs in the About row, not the custom-status line.
            w.global::<crate::ModalHostState>()
                .set_profile_bio(status_message.unwrap_or_default().into());
            if let Some(img) = avatar_img {
                w.global::<crate::ModalHostState>()
                    .set_profile_avatar_source(img);
            }
        }
    });
}
