//! DM sidebar callbacks.
//!
//! Handles selecting a DM conversation, loading messages, and presence.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::AnonDmId;

use crate::{MainWindow, run_ui};
use crate::bin_ui::converters::messages_to_display_batch;


/// Wire DM selected callback.
pub fn wire_dm_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx_clone = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_dm_selected(move |dm_id: SharedString| {
        let dm_id_str = dm_id.to_string();
        if dm_id_str.is_empty() {
            return;
        }

        let ui_ctx = ui_ctx_clone.clone();
        let weak = weak_window.clone();
        
        // Clear inline GIF animations
        crate::bin_ui::gif_state::clear_inline_gifs();

        // Look up is_anon + peer_id BEFORE spawning (we're on UI thread here).
        // The DM model stores raw uuids; the `is_anon` flag discriminates
        // anon conversations from regular ones.
        let (is_anon_dm, peer_id_for_dm) = weak
            .upgrade()
            .map(|w| {
                let conversations = w.get_dm_conversations();
                for i in 0..conversations.row_count() {
                    if let Some(item) = conversations.row_data(i) {
                        if item.channel_id.as_str() == dm_id_str {
                            let peer = item.user_id.to_string().parse::<UserId>().ok();
                            return (item.is_anon, peer);
                        }
                    }
                }
                (false, None)
            })
            .unwrap_or((false, None));

        // Commit active-conversation state SYNCHRONOUSLY on the UI thread,
        // before spawning the async loader. Sends + incoming-message gates
        // both read `active_channel_id`; if they fire before the async task
        // sets it, send_message.rs:86 early-returns and
        // handle_message_received drops the bubble (`is_active=false`).
        let active_pre_spawn: Option<chatter_core::state::ActiveConversation> = if is_anon_dm {
            match dm_id_str.parse::<AnonDmId>() {
                Ok(id) => Some(chatter_core::state::ActiveConversation::AnonDm(id)),
                Err(e) => {
                    tracing::error!("Invalid anon DM ID: {}", e);
                    None
                }
            }
        } else if let Some(ref peer) = peer_id_for_dm {
            // Regular (non-anon) DM sidebar rows are built entirely from the
            // friends list (`list_dm_sidebar`) and keyed by peer_id — there
            // is no server DM channel anymore, so this is unconditionally
            // a ServerDm keyed by the peer.
            Some(chatter_core::state::ActiveConversation::ServerDm { peer_id: peer.clone() })
        } else {
            None
        };
        if let Some(active) = active_pre_spawn.clone() {
            tracing::info!(?active, dm_id = %dm_id_str, is_anon = is_anon_dm, "[UI_OPEN_DM] committing active conversation pre-spawn");
            ui_ctx.chat_state().set_active_conversation(active);
        } else {
            tracing::warn!(dm_id = %dm_id_str, is_anon = is_anon_dm, "[UI_OPEN_DM] no active conversation computed; sends will silently drop");
        }

        let dm_id_str_for_ui = dm_id_str.clone();
        let ui_ctx_for_ui = ui_ctx.clone();
        run_ui!(weak, |w| {
            w.set_loading_messages(true);

            // Clear unread count in sidebar
            crate::bin_ui::features::auth::inbox::domain_handler::update_unread_count(&w, &ui_ctx_for_ui, &dm_id_str_for_ui, 0);


            // Clear composer state
            w.set_composer_is_replying(false);
            w.set_composer_reply_to_sender("".into());
            w.set_composer_reply_to_sender_name("".into());
            w.set_composer_reply_to_timestamp("".into());
            w.set_composer_reply_preview("".into());
            w.set_composer_reply_has_thumbnail(false);
            w.set_composer_reply_thumbnail(slint::Image::default());
            w.set_composer_is_editing(false);
            w.set_composer_edit_message_id("".into());

            // Clear pins
            w.set_has_pinned_messages(false);
            w.set_pinned_message_preview("".into());
            w.set_pinned_total_count(0);
        });

        tokio::spawn(async move {
            // 1. Handle Anon DM selection
            if is_anon_dm {
                let anon_dm_id: AnonDmId = match dm_id_str.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid anon DM ID: {}", e);
                        run_ui!(weak, |w| w.set_loading_messages(false));
                        return;
                    }
                };

                let ui_ctx_for_sel = ui_ctx.clone();
                run_ui!(weak, |w| super::refresh_selection(&w, &ui_ctx_for_sel));

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
                let channel_id = anon_dm_id.to_string().parse().unwrap_or_else(|_| ChannelId::new());
                super::apply_pinned_ui(&weak, &ui_ctx.chat_state(), channel_id, msg_ids, preview);

                // Load messages
                let messages = match ui_ctx.get_anon_dm_messages_asc(&anon_dm_id, None, chatter_core::config::MESSAGE_PAGE_SIZE) {
                    Ok(msgs) => msgs,
                    Err(e) => {
                        tracing::error!("Failed to list anon DM messages: {}", e);
                        run_ui!(weak, |w| w.set_loading_messages(false));
                        return;
                    }
                };

                let display_messages = messages_to_display_batch(
                    &messages,
                    |m| ui_ctx.message_sender_display_name(m),
                    |att_id| ui_ctx.get_attachment(att_id),
                    |m| ui_ctx.reply_author_display_name(m),
                    |m| ui_ctx.reply_parent_missing_for(m),
                    |m| ui_ctx.reply_parent_media(m),
                );

                // Get conversation info for the header
                let (title, subtitle) = match ui_ctx.get_anon_dm_conversation(&anon_dm_id) {
                    Some(conv) => (
                        conv.peer_display_name.clone().unwrap_or_else(|| "Private Chat".to_string()),
                        "Private Chat".to_string()
                    ),
                    None => ("Private Chat".to_string(), "NDSS'21 Protocol".to_string()),
                };

                let initial_count = display_messages.len();
                tracing::info!(dm_id = %anon_dm_id, count = initial_count, "[UI_OPEN_DM] anon DM spawn run_ui firing");
                run_ui!(weak, |w| {
                    use slint::Model;
                    let before = w.get_messages().row_count();
                    let current_user_id = ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default();
                    let slint_messages: Vec<_> = display_messages.iter().map(|d| d.to_slint_with_user(&current_user_id)).collect();
                    tracing::info!(before_count = before, replacing_with = slint_messages.len(), "[UI_OPEN_DM] anon DM run_ui replacing messages");
                    w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
                    w.set_loading_messages(false);
                    w.set_channel_type("anon-dm".into());
                    w.set_is_dm(true);
                    w.set_channel_name(title.into());
                    w.set_topic(subtitle.into());
                    w.set_header_presence_status("online".into());
                    w.set_channel_has_avatar(false);
                    w.set_channel_avatar_source(slint::Image::default());
                    crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), ui_ctx.clone());
                });
                return;
            }

            // 2. Handle Normal Server DM
            let peer_id = match peer_id_for_dm {
                Some(id) => id,
                None => {
                    tracing::error!("Could not determine peer ID for DM: {}", dm_id_str);
                    run_ui!(weak, |w| w.set_loading_messages(false));
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
            run_ui!(weak, |w| super::refresh_selection(&w, &ui_ctx_for_sel));

            // Load pins
            let (msg_ids, preview) = match ui_ctx.load_pinned_for_channel(&channel_id, &conv_type).await {
                Ok(pinned) if !pinned.is_empty() => {
                    let ids = pinned.iter().map(|p| p.message_id).collect();
                    let preview = pinned[0].preview.clone();
                    (ids, preview)
                }
                _ => (vec![], String::new()),
            };
            super::apply_pinned_ui(&weak, &ui_ctx.chat_state(), channel_id, msg_ids, preview);

            // Load messages
            let messages = match ui_ctx.get_messages_local_asc(&channel_id, &conv_type, None, chatter_core::config::MESSAGE_PAGE_SIZE as u32) {
                Ok(msgs) => msgs,
                Err(e) => {
                    tracing::error!("Failed to list DM messages: {}", e);
                    run_ui!(weak, |w| w.set_loading_messages(false));
                    return;
                }
            };

            let peer_profile = ui_ctx.get_user_profile(&peer_id).await.ok();
            let title = peer_profile.as_ref()
                .map(|p| p.display_name.clone())
                .unwrap_or_else(|| peer_id.to_string());
            
            let status_str = peer_profile.as_ref()
                .map(|p| p.presence.as_str())
                .unwrap_or("offline");

            let display_messages = messages_to_display_batch(
                &messages,
                |m| ui_ctx.message_sender_display_name(m),
                |att_id| ui_ctx.get_attachment(att_id),
                |m| ui_ctx.reply_author_display_name(m),
                |m| ui_ctx.reply_parent_missing_for(m),
                |m| ui_ctx.reply_parent_media(m),
            );

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

            run_ui!(weak, |w| {
                let current_user_id = ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default();
                let slint_messages: Vec<_> = display_messages.iter().map(|d| d.to_slint_with_user(&current_user_id)).collect();
                w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
                w.set_loading_messages(false);
                w.set_channel_type("dm".into());
                w.set_is_dm(true);
                w.set_channel_name(title.into());
                w.set_topic(status_str.into());
                w.set_header_presence_status(status_str.into());
                if let Some((aw, ah, px)) = &header_avatar {
                    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(px, *aw, *ah);
                    w.set_channel_avatar_source(slint::Image::from_rgba8(buf));
                    w.set_channel_has_avatar(true);
                } else {
                    w.set_channel_has_avatar(false);
                    w.set_channel_avatar_source(slint::Image::default());
                }
                // Re-register GIFs for inline animation on every open. The
                // animation state is UI-thread-local and the message model is
                // rebuilt here, so without this a ServerDM gif renders its
                // static first frame and never animates on reload. Mirrors the
                // anon-DM / channel / group open paths.
                crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), ui_ctx.clone());
            });
        });
    });
}
