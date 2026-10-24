use super::*;

use crate::bin_ui::converters::initials_from_name;
use crate::{SharedContentItem, SharedTab};
use slint::{ModelRc, VecModel};

/// Open the user profile modal with all properties populated correctly.
///
/// Handles both "self" (own profile) and "other user" paths, ensuring
/// consistent data regardless of which entry point triggered the modal.
/// Only fills the page: the caller decides whether it shows as the Profile
/// tab or as a screen pushed over the current one (`back::push`).
pub fn open_user_profile(w: &MainWindow, user_id: &UserId, ui_ctx: Option<&Arc<ChatterClient>>) {
    open_user_profile_in_conversation(w, user_id, ui_ctx, dm_conversation(user_id));
}

/// Open a sender profile while keeping shared content scoped to its source chat.
pub fn open_user_profile_in_conversation(
    w: &MainWindow,
    user_id: &UserId,
    ui_ctx: Option<&Arc<ChatterClient>>,
    conversation: chatter_core::state::ActiveConversation,
) {
    let scoped_member = matches!(
        conversation,
        chatter_core::state::ActiveConversation::AnonGroup(_)
    );
    w.global::<crate::ModalHostState>()
        .set_profile_scoped_member(scoped_member);
    let auth = ui_ctx.map(|ctx| ctx.auth_snapshot()).unwrap_or_default();
    let user_data = ui_ctx.map(|ctx| ctx.user_snapshot()).unwrap_or_default();
    let is_self = !scoped_member && auth.current_user_id.as_ref() == Some(user_id);

    let profile_user_id = user_id.to_string();
    w.global::<crate::ModalHostState>()
        .set_profile_verification_pending(safety::verification_pending_for(user_id));
    w.global::<crate::ModalHostState>()
        .set_profile_user_id(profile_user_id.into());
    w.global::<crate::ModalHostState>()
        .set_profile_contact_name(if is_self || scoped_member { String::new() } else {
            ui_ctx.and_then(|ctx| ctx.contact_name(user_id)).unwrap_or_default()
        }.into());
    w.global::<crate::ModalHostState>()
        .set_profile_is_anonymous(conversation.is_anon());
    // Offered for another user only (set again on that path below).
    w.global::<crate::ModalHostState>()
        .set_profile_can_start_secret_chat(false);
    // Reset safety number state for fresh modal
    w.global::<crate::ModalHostState>()
        .set_profile_safety_number("".into());
    w.global::<crate::ModalHostState>()
        .set_profile_show_safety_number(false);

    // Reset username-link share state; re-populated below for self.
    w.global::<crate::ModalHostState>()
        .set_profile_share_link("".into());
    w.global::<crate::ModalHostState>()
        .set_profile_share_status("".into());

    if let chatter_core::state::ActiveConversation::AnonGroup(group_id) = conversation {
        let member = ui_ctx.and_then(|ctx| ctx.get_anon_group_member(&group_id, user_id));
        let name = ui_ctx
            .and_then(|ctx| ctx.get_anon_group_member_display_name(&group_id, user_id))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Anonymous member".into());
        w.global::<crate::ModalHostState>()
            .set_profile_initials(initials_from_name(&name).into());
        w.global::<crate::ModalHostState>()
            .set_profile_display_name(name.into());
        w.global::<crate::ModalHostState>()
            .set_profile_avatar_source(
                member
                    .and_then(|member| member.avatar)
                    .as_deref()
                    .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
                    .unwrap_or_default(),
            );
        w.global::<crate::ModalHostState>()
            .set_profile_username("".into());
        w.global::<crate::ModalHostState>()
            .set_profile_bio("".into());
        w.global::<crate::ModalHostState>()
            .set_profile_status_text("".into());
        w.global::<crate::ModalHostState>()
            .set_profile_presence("offline".into());
        w.global::<crate::ModalHostState>()
            .set_profile_is_self(false);
        w.global::<crate::ModalHostState>()
            .set_profile_is_friend(false);
        w.global::<crate::ModalHostState>()
            .set_profile_is_blocked(false);
        w.global::<crate::ModalHostState>()
            .set_profile_is_verified(false);
        crate::bin_ui::features::chat::disappearing::hide_for_profile(w);
    } else if is_self {
        let display_name = auth.display_name.clone().unwrap_or_default();
        // The handle chosen at registration. Never the user id: that is not
        // something anyone can look you up by.
        let username = ui_ctx
            .and_then(|ctx| ctx.full_username())
            .filter(|u| !u.is_empty())
            .unwrap_or_default();
        let initials = initials_from_name(&display_name);

        w.global::<crate::ModalHostState>()
            .set_profile_display_name(display_name.into());
        w.global::<crate::ModalHostState>()
            .set_profile_username(username.into());
        w.global::<crate::ModalHostState>()
            .set_profile_initials(initials.into());
        w.global::<crate::ModalHostState>()
            .set_profile_avatar_source(w.global::<crate::SidebarPane>().get_user_avatar_source());
        w.global::<crate::ModalHostState>()
            .set_profile_presence(w.global::<crate::SidebarPane>().get_user_presence_status());

        // Fall back to local storage. UserState is populated by profile SYNC,
        // which covers friends — nobody broadcasts your own profile back to
        // you, so for `is_self` the in-memory entry is normally absent and the
        // bio silently read as empty after every restart, even though
        // `update_user_profile` had persisted it via `user_repo.upsert_profile`
        // (OPEN-ITEMS §112h). The display name never showed the bug because it
        // comes from auth_state instead.
        let own_profile = user_data
            .profiles
            .get(user_id)
            .cloned()
            .or_else(|| ui_ctx.and_then(|c| c.get_profile_local(user_id)));
        match own_profile.and_then(|p| p.status_message) {
            Some(msg) => w
                .global::<crate::ModalHostState>()
                .set_profile_bio(msg.text.into()),
            None => w
                .global::<crate::ModalHostState>()
                .set_profile_bio("".into()),
        }

        crate::bin_ui::features::chat::disappearing::hide_for_profile(w);
        w.global::<crate::ModalHostState>()
            .set_profile_is_self(true);
        w.global::<crate::ModalHostState>()
            .set_profile_is_friend(false);
        w.global::<crate::ModalHostState>()
            .set_profile_is_blocked(false);
        w.global::<crate::ModalHostState>()
            .set_profile_is_verified(false);

        // Show the existing username link (local copy) without a network
        // round-trip; creating/rotating goes through the wired callbacks.
        if let Some(ctx) = ui_ctx {
            if let Ok(Some(url)) = ctx.username_link_local() {
                set_share_link_props(w, ctx, &url);
            }
        }
    } else {
        // Other-user path
        let profile = user_data.profiles.get(user_id).cloned();

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
        // blocked user) — the same source as `peer_last_seen`.
        let presence = ui_ctx.map_or("offline", |ctx| ctx.peer_presence(user_id).as_str());

        // From core's local profile, like the avatar below: `user_state` is not
        // populated for peers, so the bio never showed.
        let bio = ui_ctx
            .and_then(|ctx| ctx.get_profile_local(user_id))
            .and_then(|p| p.status_message)
            .map(|m| m.text)
            .unwrap_or_default();

        let is_friend = user_data.friends.contains(user_id)
            || ui_ctx.map(|ctx| ctx.is_friend(user_id)).unwrap_or(false);
        let is_blocked = ui_ctx
            .map(|ctx| ctx.is_profile_user_blocked(user_id))
            .unwrap_or(false);

        w.global::<crate::ModalHostState>()
            .set_profile_display_name(display_name.into());
        w.global::<crate::ModalHostState>()
            .set_profile_username(username.into());
        w.global::<crate::ModalHostState>()
            .set_profile_initials(initials.into());
        // Render the peer's avatar from their decrypted server-profile bytes
        // (no URL); falls back to initials when we have none.
        let avatar_source = ui_ctx
            .and_then(|ctx| ctx.user_avatar(user_id))
            .as_deref()
            .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
            .unwrap_or_default();
        w.global::<crate::ModalHostState>()
            .set_profile_avatar_source(avatar_source);
        w.global::<crate::ModalHostState>()
            .set_profile_presence(presence.into());
        w.global::<crate::ModalHostState>()
            .set_profile_bio(bio.into());
        // A secret DM shows only what the chat itself carried: the peer's
        // in-chat name and avatar, no account-profile bio.
        if let (chatter_core::state::ActiveConversation::AnonDm(id), Some(ctx)) =
            (conversation, ui_ctx)
        {
            if let Some((name, avatar)) = ctx.anon_dm_peer_card(&id) {
                w.global::<crate::ModalHostState>()
                    .set_profile_initials(initials_from_name(&name).into());
                w.global::<crate::ModalHostState>()
                    .set_profile_display_name(name.into());
                w.global::<crate::ModalHostState>()
                    .set_profile_avatar_source(
                        avatar
                            .as_deref()
                            .and_then(crate::bin_ui::converters::avatar_bytes_to_image)
                            .unwrap_or_default(),
                    );
            }
            w.global::<crate::ModalHostState>()
                .set_profile_bio("".into());
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
        w.global::<crate::ModalHostState>()
            .set_profile_is_self(false);
        w.global::<crate::ModalHostState>()
            .set_profile_is_friend(is_friend);
        w.global::<crate::ModalHostState>()
            .set_profile_is_blocked(is_blocked);
        w.global::<crate::ModalHostState>()
            .set_profile_can_start_secret_chat(
                ui_ctx.is_some_and(|ctx| ctx.can_start_secret_chat(user_id)),
            );
        w.global::<crate::ModalHostState>()
            .set_profile_is_verified(is_verified);
    }

    // Shared-content drawer. Reset first so the previous profile's media never
    // flashes under the new name, then load this conversation's tabs.
    w.global::<crate::ModalHostState>()
        .set_profile_shared_tabs(ModelRc::new(VecModel::<SharedTab>::from(vec![])));
    w.global::<crate::ModalHostState>()
        .set_profile_shared_items(ModelRc::new(VecModel::<SharedContentItem>::from(vec![])));
    w.global::<crate::ModalHostState>()
        .set_profile_shared_tab("".into());
    w.global::<crate::ModalHostState>()
        .set_profile_shared_error("".into());
    w.global::<crate::ModalHostState>()
        .set_profile_last_seen("".into());
    w.global::<crate::ModalHostState>()
        .set_profile_is_muted(false);
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
            w.global::<crate::ModalHostState>()
                .set_profile_shared_loading(true);
            if !scoped_member {
                w.global::<crate::ModalHostState>().set_profile_last_seen(
                    ctx.peer_last_seen(user_id)
                        .map(|(presence, at)| {
                            crate::bin_ui::converters::last_seen_label(presence, at)
                        })
                        .unwrap_or_default()
                        .into(),
                );
            }
            w.global::<crate::ModalHostState>().set_profile_is_muted(
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
