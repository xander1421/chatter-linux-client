//! DM sidebar callbacks.
//!
//! Handles selecting a DM conversation, loading messages, and presence.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::{ChannelId, UserId};

use crate::bin_ui::converters::messages_to_display_batch_deferred;
use crate::MainWindow;

use super::stop_audio_playback;
use crate::bin_ui::features::history_owner::HistoryOwner;

// Notification delivery and the selection callback run on the UI thread.
// Capture this one-shot preference before the asynchronous history load.
static NOTIFICATION_OPEN_LATEST: AtomicBool = AtomicBool::new(false);

pub(crate) fn open_from_notification(
    window: &MainWindow,
    conversation: chatter_core::state::ActiveConversation,
) {
    if matches!(
        conversation,
        chatter_core::state::ActiveConversation::ServerDm { .. }
            | chatter_core::state::ActiveConversation::AnonDm(_)
    ) {
        NOTIFICATION_OPEN_LATEST.store(true, Ordering::Relaxed);
    }
    super::select_conversation(window, conversation);
}

/// Sidebar menu › Delete chat on a DM row, or a channel we are no longer in
/// (confirmed in the menu). Core resolves the row and deletes
/// (`delete_conversation`); if it was the open conversation the chat pane is
/// emptied, then the lists rebuild.
pub fn wire_delete_conversation(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();
    window.on_delete_conversation(move |row_id: SharedString, kind: crate::ConversationKind| {
        let (weak, ui_ctx) = (weak.clone(), ui_ctx.clone());
        tokio::spawn(async move {
            let kind = crate::bin_ui::converters::conversation_type(kind);
            let Some(conv) = ui_ctx.resolve_row_conversation(row_id.as_str(), &kind) else {
                tracing::warn!(row_id = %row_id, "delete chat: row resolves to no conversation");
                return;
            };
            if let Err(e) = ui_ctx.delete_conversation(conv).await {
                tracing::error!(error = %e, row_id = %row_id, "delete chat failed");
                // A secret chat stays when its peer could not be told.
                crate::run_ui!(weak, |w| {
                    w.set_toast_message("Could not delete the chat. Check your connection and try again.".into());
                    w.set_toast_variant("error".into());
                    w.set_toast_visible(true);
                });
                return;
            }
            super::conversation_removed(&weak, &ui_ctx, conv);
        });
    });
}

/// A secret chat's header: the peer as the chat presents them (core's
/// `anon_dm_peer_card`). UI thread.
fn apply_anon_dm_header(
    w: &MainWindow,
    ui_ctx: &ChatterClient,
    id: &chatter_core::domain::anon_dm::AnonDmId,
) {
    let (name, avatar) = ui_ctx
        .anon_dm_peer_card(id)
        .unwrap_or_else(|| ("Private Chat".to_string(), None));
    let image = avatar
        .as_deref()
        .and_then(crate::bin_ui::converters::avatar_bytes_to_image);
    w.set_channel_name(name.into());
    w.set_topic("Private Chat".into());
    w.set_channel_has_avatar(image.is_some());
    w.set_channel_avatar_source(image.unwrap_or_default());
}

/// Repaint the open secret chat's header and composer after its peer's name,
/// avatar or acceptance changed (RC108 O1: the initiator's header kept
/// "Private Chat" after the peer accepted until the chat was reopened).
pub(crate) fn refresh_open_anon_dm(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    id: chatter_core::domain::anon_dm::AnonDmId,
) {
    let open = chatter_core::state::ActiveConversation::AnonDm(id);
    if ui_ctx.active_conversation() != open {
        return;
    }
    let ctx = ui_ctx.clone();
    crate::run_ui!(weak, |w| {
        if ctx.active_conversation() == open {
            apply_anon_dm_header(&w, &ctx, &id);
        }
    });
    crate::bin_ui::features::chat::send_message::show_send_notice(weak, ui_ctx, id.into());
}

/// Accept / Decline under an unanswered secret-chat request opened from the
/// chat list (RC108 O2): the same handlers as Contacts › Requests › Private,
/// for the secret chat that is open.
pub fn wire_secret_request_answer(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    fn open_secret_chat(ctx: &ChatterClient) -> Option<SharedString> {
        match ctx.active_conversation() {
            chatter_core::state::ActiveConversation::AnonDm(id) => Some(id.to_string().into()),
            _ => None,
        }
    }
    let (weak, ctx) = (window.as_weak(), ui_ctx.clone());
    window.on_secret_request_accept_clicked(move || {
        if let (Some(w), Some(id)) = (weak.upgrade(), open_secret_chat(&ctx)) {
            w.invoke_friends_accept_private_request(id);
        }
    });
    let (weak, ctx) = (window.as_weak(), ui_ctx.clone());
    window.on_secret_request_decline_clicked(move || {
        if let (Some(w), Some(id)) = (weak.upgrade(), open_secret_chat(&ctx)) {
            w.invoke_friends_decline_private_request(id);
        }
    });
}

/// Profile › Start secret chat, and the chat header's ⋯ › Start secret chat
/// on a regular DM. Core starts the secret chat with the peer (or finds the
/// one that exists, `start_secret_chat`); the list rebuilds with its row and
/// the chat pane opens it through `dm-selected`, like every other DM.
pub fn wire_start_secret_chat(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let (weak, ctx) = (window.as_weak(), ui_ctx.clone());
    window.on_profile_start_secret_chat(move || {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let Ok(peer) = w.get_profile_user_id().parse::<UserId>() else {
            return;
        };
        start_secret_chat(&weak, &ctx, peer);
    });

    let ctx = ui_ctx.clone();
    window.on_chat_menu_can_start_secret_chat(move || {
        open_dm_peer(&ctx).is_some_and(|peer| ctx.can_start_secret_chat(&peer))
    });

    let (weak, ctx) = (window.as_weak(), ui_ctx.clone());
    window.on_chat_menu_start_secret_chat(move || {
        if let Some(peer) = open_dm_peer(&ctx) {
            start_secret_chat(&weak, &ctx, peer);
        }
    });
}

/// The peer of the open regular DM, if that is what is open.
fn open_dm_peer(ui_ctx: &ChatterClient) -> Option<UserId> {
    match ui_ctx.chat_state().active_conversation() {
        chatter_core::state::ActiveConversation::ServerDm { peer_id } => Some(peer_id),
        _ => None,
    }
}

fn start_secret_chat(weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>, peer: UserId) {
    if let Some(w) = weak.upgrade() {
        w.set_secret_chat_starts_pending(w.get_secret_chat_starts_pending() + 1);
        w.set_toast_message("Secret chat is getting ready — will start automatically".into());
        w.set_toast_variant("info".into());
        w.set_toast_visible(true);
    }
    let (weak, ui_ctx) = (weak.clone(), ui_ctx.clone());
    tokio::spawn(async move {
        let result = ui_ctx.start_secret_chat(peer).await;
        let done = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = done.upgrade() {
                w.set_secret_chat_starts_pending((w.get_secret_chat_starts_pending() - 1).max(0));
                if w.get_secret_chat_starts_pending() == 0
                    && w.get_toast_message()
                        == "Secret chat is getting ready — will start automatically"
                {
                    w.set_toast_visible(false);
                }
            }
        });
        match result {
            Ok(dm) => {
                super::rebuild_chat_list(&weak, &ui_ctx);
                let dm_id = dm.id.to_string();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        crate::bin_ui::features::back::reset_to_chats(&w);
                        w.invoke_dm_selected(dm_id.into());
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "start secret chat failed");
                let message = format!("Could not start secret chat: {e}");
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_toast_message(message.into());
                        w.set_toast_variant("error".into());
                        w.set_toast_visible(true);
                    }
                });
            }
        }
    });
}

/// Wire DM selected callback.
pub fn wire_dm_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx_clone = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_dm_selected(move |dm_id: SharedString| {
        let open_latest = NOTIFICATION_OPEN_LATEST.swap(false, Ordering::Relaxed);
        let open_started = std::time::Instant::now();
        tracing::info!(kind = "dm", stage = "tap", source = if open_latest { "notification" } else { "other" }, elapsed_ms = 0, "conversation_open");
        let dm_id_str = dm_id.to_string();
        if dm_id_str.is_empty() {
            return;
        }
        crate::trace_chat_open_started(open_started, "dm");

        let ui_ctx = ui_ctx_clone.clone();
        let weak = weak_window.clone();

        // What kind of DM is this? Core answers from storage, not from the
        // sidebar model: a DM filed into a custom folder is deliberately absent
        // from that model, and scanning it here made every foldered DM
        // unopenable.
        let active_pre_spawn = ui_ctx.resolve_dm_conversation(&dm_id_str);
        let Some(active) = active_pre_spawn else {
            // Nothing to open. Showing the chat pane anyway put the PREVIOUS
            // conversation on screen as if it were this one, so stay on the
            // list (the sidebar row already asked for the pane).
            tracing::warn!(dm_id = %dm_id_str, "[UI_OPEN_DM] resolves to no conversation");
            if let Some(w) = weak.upgrade() {
                w.set_mobile_show_chat(false);
                w.set_loading_messages(false);
                super::super::header::finish_pending_jump(&w, &ui_ctx);
                super::super::header::refresh_pinned_ui(&w, &ui_ctx);
            }
            return;
        };
        super::show_chat_pane(&weak);
        tracing::info!(kind = "dm", stage = "slide_requested", elapsed_ms = open_started.elapsed().as_millis(), "conversation_open");

        // Stop any playing audio
        stop_audio_playback();

        // Clear inline GIF animations
        crate::bin_ui::gif_state::clear_inline_gifs();

        super::super::header::cancel_pending_jump_for(active);

        let is_anon_dm = matches!(active, chatter_core::state::ActiveConversation::AnonDm(_));
        let peer_id_for_dm: Option<UserId> = match active {
            chatter_core::state::ActiveConversation::ServerDm { peer_id } => Some(peer_id),
            _ => None,
        };

        // Commit active-conversation state SYNCHRONOUSLY on the UI thread,
        // before spawning the async loader. Sends + incoming-message gates
        // both read `active_channel_id`; if they fire before the async task
        // sets it, send_message.rs:86 early-returns and
        // handle_message_received drops the bubble (`is_active=false`).
        tracing::info!(?active, dm_id = %dm_id_str, is_anon = is_anon_dm, "[UI_OPEN_DM] committing active conversation pre-spawn");
        ui_ctx.chat_state().set_active_conversation(active);
        if let Some(w) = weak.upgrade() {
            w.invoke_refresh_screen_protection();
        }

        if let Some(w) = weak.upgrade() {
            crate::bin_ui::features::composer::sync_composer_draft(
                &w, &ui_ctx, ui_ctx.chat_state().get().attachment_revision,
            );
        }

        let owner = HistoryOwner::begin(&ui_ctx, active);
        let dm_id_str_for_ui = dm_id_str.clone();
        let ui_ctx_for_ui = ui_ctx.clone();
        crate::run_history_ui!(weak, owner, |w| {
            w.set_loading_messages(true);

            // Clear unread count in sidebar
            crate::bin_ui::features::auth::inbox::domain_handler::update_unread_count(&w, &ui_ctx_for_ui, &dm_id_str_for_ui, 0);

            // Clear pins
            w.set_has_pinned_messages(false);
            w.set_pinned_message_preview("".into());
            w.set_pinned_total_count(0);

            // Clear the identity-change prompt up front. It is re-evaluated
            // below for a server DM; every other branch (anon DM, or a failed
            // load) must not inherit the previous conversation's prompt, which
            // would offer to pin a key belonging to somebody else.
            crate::bin_ui::features::chat::identity_change::clear(&w);
        });

        tokio::spawn(async move {
            if !owner.is_current() { return; }
            // Mark as read — the local half persists `is_read`, which is what
            // the sidebar badge counts. Clearing the Slint model above only
            // hides the badge until the next rebuild. Same call the channel
            // path makes; covers anon and server DMs alike.
            // Where the user left off. This is the only moment it exists:
            // the call that returns it is the call that clears `is_read`.
            // Receipts go out below, for the page this open loads anyway.
            let read_handle = ui_ctx.conversation(active);
            let anchor = read_handle.mark_read_local().unwrap_or_default();
            tracing::info!(kind = "dm", stage = "read_marked", elapsed_ms = open_started.elapsed().as_millis(), "conversation_open");

            // 1. Handle Anon DM selection
            // The resolved conversation, not the row id: a peer's row can
            // resolve to the anon DM we share with them.
            if let chatter_core::state::ActiveConversation::AnonDm(anon_dm_id) = active {
                // A peer avatar whose fetch never came through gets another try.
                ui_ctx.retry_secret_chat_avatar(anon_dm_id);

                let ui_ctx_for_sel = ui_ctx.clone();
                crate::run_history_ui!(weak, owner, |w| {
                    super::refresh_selection(&w, &ui_ctx_for_sel);
                    super::refresh_section_badges(&w, &ui_ctx_for_sel);
                });

                // Load pins for anon DM
                let pinned = ui_ctx.list_anon_dm_pinned(&anon_dm_id);
                let (msg_ids, preview) = if pinned.is_empty() {
                    (vec![], String::new())
                } else {
                    let ids = pinned.iter().map(|m| m.id).collect();
                    let preview = pinned[0].as_text()
                        .filter(|t| !t.is_empty())
                        .map(|t| ui_ctx.truncate_message_preview(&t))
                        .unwrap_or_else(|| "[Pinned message]".to_string());
                    (ids, preview)
                };
                if !owner.is_current() { return; }
                super::apply_pinned_ui_owned(&weak, &ui_ctx.chat_state(), msg_ids, preview, &owner);

                // Load messages
                let messages = match ui_ctx.get_anon_dm_messages_asc(&anon_dm_id, None, chatter_core::config::MESSAGE_PAGE_SIZE) {
                    Ok(msgs) => msgs,
                    Err(e) => {
                        tracing::error!("Failed to list anon DM messages: {}", e);
                        crate::run_history_ui!(weak, owner, |w| w.set_loading_messages(false));
                        return;
                    }
                };
                if !owner.is_current() { return; }
                tracing::info!(kind = "secret_dm", stage = "page_loaded", rows = messages.len(), elapsed_ms = open_started.elapsed().as_millis(), "conversation_open");
            read_handle.send_read_receipts_for_page(&messages);

                let display_messages = messages_to_display_batch_deferred(
                    &messages,
                    |ms| ui_ctx.message_sender_display_names(ms),
                    |m| ui_ctx.message_sender_avatar(m),
                    crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
                    |m| ui_ctx.reply_author_display_name(m),
                    |m| ui_ctx.reply_parent_missing_for(m),
                    |m| ui_ctx.reply_parent_media(m),
                );

                let initial_count = display_messages.len();
                tracing::info!(kind = "secret_dm", stage = "display_built", rows = initial_count, elapsed_ms = open_started.elapsed().as_millis(), "conversation_open");
                tracing::info!(dm_id = %anon_dm_id, count = initial_count, "[UI_OPEN_DM] anon DM spawn run_ui firing");
                let anchor_page = messages.clone();
                crate::run_history_ui!(weak, owner, |w| {
                    use slint::Model;
                    super::apply_unread_anchor(
                        &w,
                        if open_latest { Default::default() } else { anchor },
                        &anchor_page,
                    );
                    let before = w.get_messages().row_count();
                    let current_user_id = ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default();
                    let slint_messages: Vec<_> = display_messages.iter().map(|d| d.to_slint_preview_with_user(&current_user_id)).collect();
                    tracing::info!(before_count = before, replacing_with = slint_messages.len(), "[UI_OPEN_DM] anon DM run_ui replacing messages");
                    w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
                    crate::trace_chat_open_model(initial_count, 0);
                    let fill_weak = w.as_weak();
                    crate::trace_chat_open_after_content(move || super::fill_open_media(fill_weak, owner, display_messages, current_user_id));
                    tracing::info!(kind = "secret_dm", stage = "model_replaced", rows = initial_count, elapsed_ms = open_started.elapsed().as_millis(), "conversation_open");
                    tracing::debug!(elapsed_ms = open_started.elapsed().as_millis(), "conversation open: messages presented");
                    w.set_loading_messages(false);
                super::super::header::finish_pending_jump(&w, &ui_ctx);
                super::super::header::refresh_pinned_ui(&w, &ui_ctx);
                    crate::bin_ui::converters::set_conversation_kind(
                        &w, &ui_ctx, &chatter_core::domain::message::ConversationType::AnonDm, false,
                    );
                    w.set_is_dm(true);
                    apply_anon_dm_header(&w, &ui_ctx, &anon_dm_id);
                    w.set_header_presence_status("online".into());
                    crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), ui_ctx.clone());
                });
                return;
            }

            // 2. Handle Normal Server DM
            let peer_id = match peer_id_for_dm {
                Some(id) => id,
                None => {
                    tracing::error!("Could not determine peer ID for DM: {}", dm_id_str);
                    crate::run_history_ui!(weak, owner, |w| w.set_loading_messages(false));
                    return;
                }
            };

            // The sidebar's `channel_id` field for a regular DM is the
            // peer's user_id (same UUID, wrapped as ChannelId — see
            // `ChannelId::from(UserId)`), so parsing it directly gives the
            // exact local storage key `send_server_dm` stores under.
            let channel_id: ChannelId = match dm_id_str.parse() {
                Ok(id) => id,
                Err(_) => ChannelId::new(),
            };

            // Regular (non-anon) DM sidebar rows are channel-free ServerDm
            // conversations — no channel row to probe.
            let conv_type = chatter_core::domain::message::ConversationType::ServerDm;

            let ui_ctx_for_sel = ui_ctx.clone();
            crate::run_history_ui!(weak, owner, |w| {
                super::refresh_selection(&w, &ui_ctx_for_sel);
                super::refresh_section_badges(&w, &ui_ctx_for_sel);
            });

            // Load pins
            let (msg_ids, preview) = match ui_ctx.load_pinned_for_channel(&channel_id, &conv_type).await {
                Ok(pinned) if !pinned.is_empty() => {
                    let ids = pinned.iter().map(|p| p.message_id).collect();
                    let preview = pinned[0].preview.clone();
                    (ids, preview)
                }
                _ => (vec![], String::new()),
            };
            if !owner.is_current() { return; }
            tracing::info!(kind = "dm", stage = "pins_loaded", elapsed_ms = open_started.elapsed().as_millis(), "conversation_open");
                super::apply_pinned_ui_owned(&weak, &ui_ctx.chat_state(), msg_ids, preview, &owner);

            // Load messages
            let messages = match ui_ctx.get_messages_local_asc(&channel_id, &conv_type, None, chatter_core::config::MESSAGE_PAGE_SIZE as u32) {
                Ok(msgs) => msgs,
                Err(e) => {
                    tracing::error!("Failed to list DM messages: {}", e);
                    crate::run_history_ui!(weak, owner, |w| w.set_loading_messages(false));
                    return;
                }
            };
            if !owner.is_current() { return; }
            tracing::info!(kind = "dm", stage = "page_loaded", rows = messages.len(), elapsed_ms = open_started.elapsed().as_millis(), "conversation_open");
            read_handle.send_read_receipts_for_page(&messages);

            // The stored profile, not a server fetch: the open no longer waits
            // on a network round trip (and in Shield makes no identified
            // request at all). The throttled refresh after the first paint,
            // below, patches the header through `UserProfileUpdated`. The name
            // is the chat list's resolution (profile → username →
            // placeholder), never the raw id.
            let peer_profile = ui_ctx.get_profile_local(&peer_id);
            let title = ui_ctx.display_name(&peer_id);

            let status_str = ui_ctx.peer_presence(&peer_id).as_str();

            let display_messages = messages_to_display_batch_deferred(
                &messages,
                |ms| ui_ctx.message_sender_display_names(ms),
                |m| ui_ctx.message_sender_avatar(m),
                crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
                |m| ui_ctx.reply_author_display_name(m),
                |m| ui_ctx.reply_parent_missing_for(m),
                |m| ui_ctx.reply_parent_media(m),
            );
            tracing::info!(kind = "dm", stage = "display_built", rows = display_messages.len(), elapsed_ms = open_started.elapsed().as_millis(), "conversation_open");

            if !owner.is_current() { return; }
            // Decode the peer's avatar (Send-safe RGBA); the non-Send Image is
            // built in the run_ui closure. Header shows it if present.
            let header_avatar: Option<(u32, u32, Vec<u8>)> = peer_profile
                .as_ref()
                .and_then(|p| p.avatar_bytes.as_ref())
                .and_then(|b| {
                    image::load_from_memory(b).ok().map(|img| {
                        let rgba = img.to_rgba8();
                        let (aw, ah) = rgba.dimensions();
                        (aw, ah, rgba.into_raw())
                    })
                });

            let refresh_ctx = ui_ctx.clone();
            let anchor_page = messages.clone();
            let media_owner = owner.clone();
            crate::run_history_ui!(weak, owner, |w| {
                super::apply_unread_anchor(
                    &w,
                    if open_latest { Default::default() } else { anchor },
                    &anchor_page,
                );
                let current_user_id = ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default();
                let slint_messages: Vec<_> = display_messages.iter().map(|d| d.to_slint_preview_with_user(&current_user_id)).collect();
                w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
                tracing::info!(kind = "dm", stage = "model_replaced", rows = display_messages.len(), elapsed_ms = open_started.elapsed().as_millis(), "conversation_open");
                crate::trace_chat_open_model(display_messages.len(), 0);
                let fill_weak = w.as_weak();
                crate::trace_chat_open_after_content(move || super::fill_open_media(fill_weak, media_owner, display_messages, current_user_id));
                tracing::debug!(elapsed_ms = open_started.elapsed().as_millis(), "conversation open: messages presented");
                w.set_loading_messages(false);
                super::super::header::finish_pending_jump(&w, &ui_ctx);
                super::super::header::refresh_pinned_ui(&w, &ui_ctx);
                crate::bin_ui::converters::set_conversation_kind(&w, &ui_ctx, &conv_type, false);
                w.set_is_dm(true);
                w.set_channel_name(title.into());
                // A DM has no topic. This used to carry the presence word
                // ("online"/"offline"), which the header centres beside the
                // name — so every DM header spelled out what the dot next to
                // the name already says, in the row with the least room for it.
                w.set_topic("".into());
                w.set_header_presence_status(status_str.into());
                if let Some((aw, ah, px)) = &header_avatar {
                    w.set_channel_avatar_source(crate::bin_ui::slint_conversions::rgba_to_slint_image(px, *aw, *ah));
                    w.set_channel_has_avatar(true);
                } else {
                    w.set_channel_has_avatar(false);
                    w.set_channel_avatar_source(slint::Image::default());
                }
                // Is this conversation wedged on an unaccepted identity key?
                // Read on every open, not only on the live event, so a change
                // parked while the app was closed is still surfaced.
                crate::bin_ui::features::chat::identity_change::refresh(&w, &ui_ctx, &peer_id);

                // Re-register GIFs for inline animation on every open. The
                // animation state is UI-thread-local and the message model is
                // rebuilt here, so without this a ServerDM gif renders its
                // static first frame and never animates on reload. Mirrors the
                // anon-DM / channel / group open paths.
                crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), ui_ctx.clone());
            });

            // Catch up on the peer's profile edits once the conversation is on
            // screen (throttled in core; skipped in Shield).
            if owner.is_current() { refresh_ctx.refresh_profile_if_stale(&peer_id, &chatter_core::state::ActiveConversation::ServerDm { peer_id }).await; }
        });
    });
}
