//! DM selection and anonymous DM loading.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_dm::AnonDmId;
use chatter_linux::domain::channel::ChannelType;
use chatter_linux::domain::ids::ChannelId;
use chatter_linux::services::user::UserService;

use crate::bin_ui::features::chat::send_dm_read_receipts;
use crate::MainWindow;

use super::channel::load_channel_into_ui;
use super::stop_audio_playback;

pub(crate) fn wire_dm_selected(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let chat_state = ui_ctx.chat_state.clone();
    let channel_service = ui_ctx.channel_service.clone();
    let message_service = ui_ctx.message_service.clone();
    let user_service = ui_ctx.user_service.clone();
    let attachment_service = ui_ctx.attachment_service.clone();
    let auth_state = ui_ctx.auth_state.clone();
    let mailbox_manager = ui_ctx.mailbox_manager.clone();
    let runtime_handle = ui_ctx.runtime_handle.clone();
    let anon_dm_service = ui_ctx.anon_dm_service.clone();

    let weak_window = window.as_weak();

    window.on_dm_selected(move |dm_id: SharedString| {
        let dm_id_str = dm_id.to_string();
        let weak = weak_window.clone();
        let chat_state = chat_state.clone();
        let channel_service = channel_service.clone();
        let message_service = message_service.clone();
        let user_service = user_service.clone();
        let attachment_service = attachment_service.clone();
        let auth_state = auth_state.clone();
        let mailbox_manager = mailbox_manager.clone();
        let runtime_handle = runtime_handle.clone();
        let anon_dm_service = anon_dm_service.clone();

        // Stop any playing audio when switching channels
        stop_audio_playback();

        // Clear inline GIF animations to free memory
        crate::bin_ui::gif_state::clear_inline_gifs();

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

                load_anon_dm_into_ui(
                    weak,
                    anon_dm_id,
                    anon_dm_service,
                    user_service,
                    attachment_service,
                    auth_state,
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

            // Get current user ID for is_own detection
            let current_user_id = auth_state.get().current_user_id.map(|id| id.to_string());

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
                                    send_dm_read_receipts(
                                        peer_id,
                                        unread_timestamps,
                                        message_service,
                                        mailbox_manager,
                                        runtime_handle,
                                    );
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
    anon_dm_service: Arc<chatter_linux::services::anon_dm_service::AnonDmService>,
    user_service: Arc<UserService>,
    attachment_service: Arc<chatter_linux::services::attachment::AttachmentService>,
    auth_state: chatter_linux::state::AuthState,
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

    // Load messages
    let messages = match anon_dm_service.get_messages(&dm_id, None, 50) {
        Ok(msgs) => msgs,
        Err(e) => {
            tracing::error!("Failed to load anon DM messages: {}", e);
            Vec::new()
        }
    };

    // Convert messages to display data
    let mut display_messages: Vec<crate::bin_ui::converters::MessageDisplayData> = Vec::with_capacity(messages.len());
    for msg in &messages {
        let sender_name = user_service.get_display_name(&msg.sender_id);

        // Resolve attachments from stored IDs
        let attachment_ids = msg.attachment_ids();
        let resolved_attachments: Vec<chatter_linux::domain::Attachment> = attachment_ids
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

        let display_data = crate::bin_ui::converters::anon_dm_message_to_display_data(
            msg,
            &sender_name,
            &resolved_attachments,
        );
        display_messages.push(display_data);
    }

    // Reverse to show oldest first
    display_messages.reverse();

    // Mark conversation as read locally
    if let Err(e) = anon_dm_service.mark_as_read(&dm_id) {
        tracing::warn!("Failed to mark anon DM as read: {}", e);
    }

    // Send read receipts for messages from the peer
    // Collect sent_at timestamps (sender's canonical timestamp for proper ordering)
    let unread_timestamps: Vec<u64> = messages
        .iter()
        .filter(|m| current_user_id.map(|uid| m.sender_id != uid).unwrap_or(true))
        .map(|m| m.sent_at)
        .collect();

    if !unread_timestamps.is_empty() {
        let service = anon_dm_service.clone();
        let dm_id_for_receipt = dm_id;
        tokio::spawn(async move {
            if let Err(e) = service.send_read_receipt(&dm_id_for_receipt, unread_timestamps).await {
                tracing::warn!("Failed to send anon DM read receipts: {}", e);
            } else {
                tracing::debug!(
                    "Sent read receipts for anon DM {} messages",
                    dm_id_for_receipt
                );
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

/// Add an anonymous DM to the sidebar dynamically.
pub fn add_anon_dm_to_sidebar(
    window: &MainWindow,
    dm_id: &str,
    peer_id: &str,
    peer_name: &str,
    last_message: &str,
    timestamp: &str,
    is_pending: bool,
) {
    let weak_window = window.as_weak();
    let dm_id = dm_id.to_string();
    let peer_id = peer_id.to_string();
    let peer_name = peer_name.to_string();
    let last_message = last_message.to_string();
    let timestamp = timestamp.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_window.upgrade() {
            let existing = w.get_dm_conversations();
            let mut dms: Vec<crate::DMListItemData> = (0..existing.row_count())
                .filter_map(|i| existing.row_data(i))
                .collect();

            let channel_id = format!("anon_dm:{}", dm_id);
            
            // Avoid duplicates
            if dms.iter().any(|d| d.channel_id.as_str() == channel_id) {
                return;
            }

            // Incognito icon for private, postbox for pending incoming
            let display_name = if is_pending {
                format!("\u{1F4EE} {}", peer_name)
            } else {
                format!("\u{1F475} {}", peer_name)
            };

            // Add new DM at the beginning
            dms.insert(0, crate::DMListItemData {
                channel_id: channel_id.into(),
                user_id: peer_id.into(),
                display_name: display_name.into(),
                avatar_url: "".into(),
                avatar_source: slint::Image::default(),
                has_avatar: false,
                initials: crate::bin_ui::converters::get_initials(&peer_name).into(),
                presence_status: if is_pending { "pending".into() } else { "offline".into() },
                last_message: last_message.into(),
                timestamp: timestamp.into(),
                unread_count: if is_pending { 1 } else { 0 },
                is_typing: false,
            });

            w.set_dm_conversations(ModelRc::new(VecModel::from(dms)));
        }
    });
}
