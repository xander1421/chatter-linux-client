//! DM selection and anonymous DM loading.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::domain::channel::ChannelType;
use chatter_core::domain::ids::ChannelId;
use chatter_core::services::user::UserService;

use chatter_core::services::conversation::ConversationHandle;
use crate::MainWindow;

use super::channel::load_channel_into_ui;
use super::stop_audio_playback;

pub(crate) fn wire_dm_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx_clone = ui_ctx.clone();
    let chat_state = ui_ctx.chat_state().clone();
    let channel_service = ui_ctx.channels().clone();
    let message_service = ui_ctx.messages().clone();
    let user_service = ui_ctx.users().clone();
    let attachment_service = ui_ctx.attachments().clone();
    let auth_state = ui_ctx.auth_state().clone();
    let anon_dm_service = ui_ctx.anon_dms().clone();
    let pin_service = ui_ctx.pins().clone();
    let pinned_state = ui_ctx.pinned_state().clone();

    let weak_window = window.as_weak();

    window.on_dm_selected(move |dm_id: SharedString| {
        let dm_id_str = dm_id.to_string();
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx_clone.clone();
        let chat_state = chat_state.clone();
        let channel_service = channel_service.clone();
        let message_service = message_service.clone();
        let user_service = user_service.clone();
        let attachment_service = attachment_service.clone();
        let auth_state = auth_state.clone();
        let anon_dm_service = anon_dm_service.clone();
        let pin_service = pin_service.clone();
        let pinned_state = pinned_state.clone();

        // Stop any playing audio when switching channels
        stop_audio_playback();

        // Clear inline GIF animations to free memory
        crate::bin_ui::gif_state::clear_inline_gifs();

        // Look up peer_id BEFORE spawning (we're on UI thread here)
        let peer_id_for_dm = if !dm_id_str.starts_with("anon_dm:") {
            weak.upgrade().and_then(|w| {
                let conversations = w.get_dm_conversations();
                for i in 0..conversations.row_count() {
                    if let Some(item) = conversations.row_data(i) {
                        if item.channel_id.as_str() == dm_id_str {
                            return item.user_id.to_string().parse::<chatter_core::domain::UserId>().ok();
                        }
                    }
                }
                None
            })
        } else {
            None
        };

        tokio::spawn(async move {
            // dm_id is now the channel_id directly (from DMListItemData.channel_id)
            let weak_loading = weak.clone();
            let dm_id_for_ui = dm_id.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_loading.upgrade() {
                    w.set_loading_messages(true);
                    w.set_selected_dm_id(dm_id_for_ui);
                    w.set_selected_channel_id("".into());
                    w.set_selected_private_group_id("".into());
                    // Clear composer reply/edit state to prevent stale context
                    w.set_composer_is_replying(false);
                    w.set_composer_reply_to_sender("".into());
                    w.set_composer_reply_to_sender_name("".into());
                    w.set_composer_reply_to_timestamp("".into());
                    w.set_composer_reply_preview("".into());
                    w.set_composer_reply_has_thumbnail(false);
                    w.set_composer_reply_thumbnail(slint::Image::default());
                    w.set_composer_is_editing(false);
                    w.set_composer_edit_message_id("".into());
                    // Clear pinned bar immediately while loading new conversation's pins
                    w.set_has_pinned_messages(false);
                    w.set_pinned_message_preview("".into());
                    w.set_pinned_total_count(0);
                }
            });

            // Check if this is an anon DM (prefixed with "anon_dm:")
            if let Some(anon_dm_id_str) = dm_id_str.strip_prefix("anon_dm:") {
                // Handle anon DM selection
                let anon_dm_id: AnonDmId = match anon_dm_id_str.parse() {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::error!("Invalid anon DM ID: {}", e);
                        let weak_err = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_err.upgrade() {
                                w.set_loading_messages(false);
                            }
                        });
                        return;
                    }
                };

                // Set typed active conversation
                chat_state.set_active_conversation(chatter_core::state::ActiveConversation::AnonDm(anon_dm_id));

                // Load pinned messages for this anon DM
                let (msg_ids, preview) = match anon_dm_service.list_pinned(&anon_dm_id) {
                    Ok(pinned) if !pinned.is_empty() => {
                        let ids = pinned.iter().map(|m| m.id).collect();
                        let preview = pinned[0].as_text()
                            .filter(|t| !t.is_empty())
                            .map(|t| if t.len() > 80 { format!("{}...", &t[..77]) } else { t.to_string() })
                            .unwrap_or_else(|| "[Pinned message]".to_string());
                        (ids, preview)
                    }
                    _ => (vec![], String::new()),
                };
                let channel_id = anon_dm_id.to_string().parse()
                    .unwrap_or_else(|_| chatter_core::domain::ids::ChannelId::new());
                super::apply_pinned_ui(&weak, &pinned_state, channel_id, msg_ids, preview);

                // Send presence update and track activity when opening anon DM
                crate::bin_ui::features::auth::presence_integration::notify_chat_opened(
                    &ui_ctx,
                    &anon_dm_id,
                ).await;
                crate::bin_ui::features::auth::presence_integration::track_activity(&ui_ctx).await;

                load_anon_dm_into_ui(
                    weak,
                    anon_dm_id,
                    anon_dm_service,
                    user_service,
                    attachment_service,
                    auth_state,
                    ui_ctx.clone(),
                ).await;
                return;
            }

            // Regular DM - parse channel ID directly
            let channel_id: ChannelId = match dm_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid DM channel ID: {}", e);
                    return;
                }
            };

            // Update chat state
            chat_state.set_active_channel(channel_id);
            if let Some(peer_id) = peer_id_for_dm {
                chat_state.set_active_conversation(chatter_core::state::ActiveConversation::MailboxDm { channel_id, peer_id });
            } else {
                // Fallback if peer lookup fails (shouldn't happen for real DMs)
                chat_state.set_active_conversation(chatter_core::state::ActiveConversation::Channel(channel_id));
                tracing::warn!(channel_id = %channel_id, "Could not resolve peer_id for DM — falling back to Channel");
            }

            // Get current user ID for is_own detection
            let current_user_id = auth_state.get().current_user_id.map(|id| id.to_string());

            // Load pinned messages for this DM channel
            if let Ok(Ok(pinned_messages)) = tokio::time::timeout(
                crate::bin_ui::ui_helpers::UI_SERVICE_TIMEOUT,
                pin_service.load_pinned_for_channel(&channel_id),
            ).await {
                if !pinned_messages.is_empty() {
                    let message_ids: Vec<_> = pinned_messages.iter().map(|p| p.message_id).collect();
                    pinned_state.set_pinned_messages(channel_id, message_ids);
                    
                    if let Some(first) = pinned_messages.first() {
                        let preview = first.preview.clone();
                        let total_count = pinned_messages.len() as i32;
                        let should_show = pinned_state.should_show();
                        
                        let weak_pinned = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak_pinned.upgrade() {
                                w.set_has_pinned_messages(should_show);
                                w.set_pinned_message_preview(preview.into());
                                w.set_pinned_current_index(1);
                                w.set_pinned_total_count(total_count);
                            }
                        });
                    }
                } else {
                    pinned_state.clear();
                    let weak_pinned = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak_pinned.upgrade() {
                            w.set_has_pinned_messages(false);
                        }
                    });
                }
            }

            // Load channel and messages
            load_channel_into_ui(
                weak.clone(),
                &channel_id,
                channel_service.clone(),
                message_service.clone(),
                user_service.clone(),
                attachment_service.clone(),
                current_user_id,
            ).await;

            // Send read receipts for unread messages from the peer
            if auth_state.is_logged_in() {
                // Get the peer's user ID from the DM channel
                if let Ok(Some(channel)) = channel_service.get_channel_local(&channel_id) {
                    if channel.channel_type == ChannelType::DirectMessage {
                        // Get our user ID
                        let my_user_id = auth_state.get().current_user_id.clone();

                        // Get recent messages to find the peer and their message timestamps
                        if let Ok(messages) = message_service.get_messages_local(&channel_id, None, 50) {
                            // Find peer ID (sender that isn't us)
                            let peer_id = messages
                                .iter()
                                .filter_map(|m| m.sender_id.as_ref())
                                .find(|sender| my_user_id.as_ref() != Some(*sender))
                                .cloned();

                            if let Some(peer_id) = peer_id {
                                // Get sent_at timestamps (nanoseconds) of messages from the peer
                                let unread_timestamps: Vec<u64> = messages
                                    .iter()
                                    .filter(|m| m.sender_id.as_ref() == Some(&peer_id))
                                    .map(|m| m.sent_at)
                                    .collect();

                                if !unread_timestamps.is_empty() {
                                    let handle = ConversationHandle::new(
                                        chatter_core::state::ActiveConversation::MailboxDm { channel_id, peer_id },
                                        ui_ctx.clone(),
                                    );
                                    tokio::spawn(async move {
                                        if let Err(e) = handle.send_read_receipt(&unread_timestamps).await {
                                            tracing::warn!("Failed to send DM read receipts: {}", e);
                                        }
                                    });
                                }
                            }
                        }
                    }
                }
            }
        });
    });
}

/// Load an anonymous DM conversation into the UI.
async fn load_anon_dm_into_ui(
    window_weak: slint::Weak<MainWindow>,
    dm_id: AnonDmId,
    anon_dm_service: Arc<chatter_core::services::anon_dm::AnonDmService>,
    user_service: Arc<UserService>,
    attachment_service: Arc<chatter_core::services::attachment::AttachmentService>,
    auth_state: chatter_core::state::AuthState,
    ui_ctx: Arc<ChatterClient>,
) {
    // Get conversation info
    let dm = match anon_dm_service.get_conversation(&dm_id) {
        Ok(Some(dm)) => dm,
        Ok(None) => {
            tracing::error!("Anon DM conversation not found: {}", dm_id);
            let weak = window_weak;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_loading_messages(false);
                }
            });
            return;
        }
        Err(e) => {
            tracing::error!("Failed to load anon DM: {}", e);
            let weak = window_weak;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_loading_messages(false);
                }
            });
            return;
        }
    };

    // Get current user ID for is_own calculation
    let current_user_id = auth_state.get().current_user_id;
    let current_user_id_str = current_user_id.map(|id| id.to_string()).unwrap_or_default();

    // Get peer display name from conversation (revealed on accept/init)
    // Do NOT use user_service - these are strangers
    let peer_name = dm.peer_display_name.clone()
        .unwrap_or_else(|| format!("User {}", &dm.peer_id.to_string()[..8]));
    let peer_avatar = dm.peer_avatar.clone();
    let my_avatar = anon_dm_service.get_my_avatar();

    // Load messages
    let mut messages = match anon_dm_service.get_messages(&dm_id, None, 50) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::error!("Failed to load anon DM messages: {}", e);
            Vec::new()
        }
    };

    // Load reaction counts from DB and populate messages
    if let Ok(reaction_map) = anon_dm_service.get_all_reaction_counts(&dm_id) {
        for msg in &mut messages {
            if let Some(counts) = reaction_map.get(&msg.sent_at) {
                msg.reaction_counts = counts
                    .iter()
                    .map(|(emoji, count): &(String, u32)| chatter_core::domain::message::ReactionCount {
                        emoji: emoji.clone(),
                        count: *count,
                    })
                    .collect();
            }
        }
    }

    // Convert messages to display data
    let mut display_messages: Vec<crate::bin_ui::converters::MessageDisplayData> = Vec::with_capacity(messages.len());
    for msg in &messages {
        let sender_name = if let Some(sid) = msg.sender_id {
            let is_own = current_user_id.map(|uid| sid == uid).unwrap_or(false);
            if is_own {
                user_service.get_display_name(&sid)
            } else {
                // For anon DM peer, use conversation's peer_display_name (not user_repo)
                peer_name.clone()
            }
        } else {
            "Unknown".to_string()
        };

        // Determine which avatar to use
        let sender_avatar = if current_user_id.map(|uid| msg.sender_id == Some(uid)).unwrap_or(false) {
            my_avatar.clone()
        } else {
            peer_avatar.clone()
        };

        // Resolve attachments from stored IDs
        let attachment_ids = msg.attachments.clone();
        let resolved_attachments: Vec<chatter_core::domain::Attachment> = attachment_ids
            .iter()
            .filter_map(|att_id| {
                match attachment_service.get(att_id) {
                    Ok(Some(att)) => Some(att),
                    Ok(None) => {
                        tracing::warn!("Attachment not found: {:?}", att_id);
                        None
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load attachment {:?}: {}", att_id, e);
                        None
                    }
                }
            })
            .collect();

        let mut display_data = crate::bin_ui::converters::anon_dm_message_to_display_data(
            msg,
            &sender_name,
            sender_avatar,
            &resolved_attachments,
        );

        // Resolve reply-to sender name: user repo first, then anon DM peer name
        if display_data.is_reply {
            if let Some(ref author_id) = msg.reply_to_author_id {
                let reply_name = user_service.get_display_name(author_id);
                display_data.reply_to_sender_name = if reply_name == author_id.to_string() {
                    // Not in user repo — check if it's the DM peer
                    if *author_id == dm.peer_id {
                        peer_name.clone()
                    } else {
                        // Might be ourselves
                        current_user_id
                            .filter(|uid| *uid == *author_id)
                            .map(|uid| user_service.get_display_name(&uid))
                            .unwrap_or(reply_name)
                    }
                } else {
                    reply_name
                };
            }
        }

        display_messages.push(display_data);
    }

    // Reverse to show oldest first
    display_messages.reverse();

    // Pre-decode attachment thumbnails and sender avatars before UI thread
    for msg in &mut display_messages {
        msg.decode_attachment_thumbnails();
    }

    // Mark as read + send read receipts via SDK
    {
        let handle = chatter_core::services::conversation::ConversationHandle::new(
            chatter_core::state::ActiveConversation::AnonDm(dm_id),
            ui_ctx.clone(),
        );
        tokio::spawn(async move {
            if let Err(e) = handle.mark_as_read().await {
                tracing::warn!("Failed to mark anon DM as read: {}", e);
            }
        });
    }

    // Update UI
    let dm_id_str = dm_id.to_string();
    let message_count = display_messages.len();
    let attachment_service_for_gifs = attachment_service.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = window_weak.upgrade() {
            // Convert and set messages
            let slint_messages: Vec<_> = display_messages
                .iter()
                .map(|m| m.to_slint_with_user(&current_user_id_str))
                .collect();

            // Set chat header info for anonymous DM
            // The header UI will show the lock icon based on channel-type
            // Look up presence from sidebar model
            let dms = w.get_dm_conversations();
            let anon_dm_channel = format!("anon_dm:{}", dm_id_str);
            for i in 0..dms.row_count() {
                if let Some(item) = dms.row_data(i) {
                    if item.channel_id == anon_dm_channel {
                        w.set_header_presence_status(item.presence_status.clone());
                        break;
                    }
                }
            }
            w.set_channel_name(peer_name.clone().into());
            w.set_channel_type("anon-dm".into());
            w.set_is_dm(true);
            w.set_member_count(0); // Not applicable for DMs
            w.set_topic("".into()); // No topic for anon DMs
            w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
            w.set_loading_messages(false);

            // Auto-register GIF animations for inline playback
            crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), attachment_service_for_gifs);

            tracing::info!(
                "Selected anon DM {} with {} with {} messages",
                dm_id_str,
                peer_name,
                message_count
            );
        }
    });
}

