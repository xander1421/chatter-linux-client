//! Private group selection and loading.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::ChannelId;

use crate::{MainWindow, PrivateGroupListItemData, run_ui};

use super::stop_audio_playback;
use std::str::FromStr;

/// Load and display messages for a private group.
/// This can be called both when a group is selected and when member profiles are updated.
pub fn load_private_group_messages(window: &MainWindow, ui_ctx: &Arc<ChatterClient>, group_id_str: &str) {
    let group_id = match AnonGroupId::from_str(group_id_str) {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("Invalid group ID '{}': {}", group_id_str, e);
            return;
        }
    };
    
    let anon_group_service = ui_ctx.anon_groups().clone();
    let user_service = ui_ctx.users().clone();
    let attachment_service = ui_ctx.attachments().clone();
    let auth_state = ui_ctx.auth_state().clone();
    let weak_window = window.as_weak();
    let group_id_str = group_id_str.to_string();
    
    tokio::spawn(async move {
        // Get group details
        let group = match anon_group_service.get_group(&group_id) {
            Ok(Some(g)) => g,
            Ok(None) => {
                tracing::error!("Private group {} not found", group_id);
                return;
            }
            Err(e) => {
                tracing::error!("Failed to get private group {}: {}", group_id, e);
                return;
            }
        };
        
        // Get member count for header display
        let member_count = anon_group_service
            .get_members(&group_id)
            .map(|m| m.len() as i32)
            .unwrap_or(0);
        
        // Get current user ID for is_own calculation
        let current_user_id = auth_state.get().current_user_id;
        let current_user_id_str = current_user_id.map(|id| id.to_string()).unwrap_or_default();
        
        // Load messages for this group
        let mut messages = match anon_group_service.get_messages_asc(&group_id, None, 50) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::error!("Failed to load messages for private group {}: {}", group_id, e);
                Vec::new()
            }
        };

        // Load reaction counts from DB and populate messages
        if let Ok(reaction_map) = anon_group_service.get_all_reaction_counts(&group_id) {
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
            let sender_id = msg.sender_id.as_ref();
            let mut sender_name = sender_id
                .map(|id| user_service.get_display_name(id))
                .unwrap_or_default();
            let mut sender_avatar = None;

            if let Some(sid) = sender_id {
                match anon_group_service.get_member(&group_id, sid) {
                    Ok(Some(member)) => {
                        if let Some(ref name) = member.display_name {
                            if !name.is_empty() {
                                sender_name = name.clone();
                            }
                        }
                        sender_avatar = member.avatar.clone();
                    }
                    _ => {}
                }
            }

            // Resolve attachments from stored IDs
            let resolved_attachments: Vec<chatter_core::domain::Attachment> = msg.attachments
                .iter()
                .filter_map(|att_id| attachment_service.get(att_id).ok().flatten())
                .collect();

            let mut display_data = crate::bin_ui::converters::anon_group_message_to_display_data(
                msg,
                &sender_name,
                sender_avatar,
                &resolved_attachments,
            );

            // Resolve reply-to sender name if this is a reply/quote
            if display_data.is_reply {
                if let Some(ref author_id) = msg.reply_to_author_id {
                    display_data.reply_to_sender_name = anon_group_service
                        .get_member_display_name(&group_id, author_id)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| user_service.get_display_name(author_id));
                }
            }

            display_messages.push(display_data);
        }

        // Pre-decode attachment thumbnails before UI thread
        for msg in &mut display_messages {
            msg.decode_attachment_thumbnails();
        }
        
        // Update UI to show selected group and messages
        let group_name = group.name.clone();
        let attachment_service_for_gifs = attachment_service.clone();
        run_ui!(weak_window, |w| {
            // Clear other selections
            w.set_selected_channel_id("".into());
            w.set_selected_dm_id("".into());
            w.set_selected_private_group_id(group_id_str.clone().into());
            
            // Clear unread count in sidebar
            crate::bin_ui::features::auth::inbox::domain_handler::update_unread_count(&w, &group_id_str, 0);

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

            // Set chat header info for private group
            w.set_channel_name(group_name.into());
            w.set_channel_type("private-group".into());
            w.set_is_dm(false);
            w.set_member_count(member_count);
            w.set_topic("".into());
            w.set_header_presence_status("offline".into());
            
            // Convert and set messages
            let slint_messages: Vec<_> = display_messages
                .iter()
                .map(|m| m.to_slint_with_user(&current_user_id_str))
                .collect();
            w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
            w.set_loading_messages(false);
            
            // Auto-register GIF animations
            crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), attachment_service_for_gifs);
        });
    });
}

pub(crate) fn wire_private_group_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak_window = window.as_weak();

    window.on_private_group_selected(move |group_id: SharedString| {
        let group_id_str = group_id.to_string();
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();

        // Stop any playing audio when switching
        stop_audio_playback();
        crate::bin_ui::gif_state::clear_inline_gifs();

        tokio::spawn(async move {
            // Parse group ID
            let group_id: AnonGroupId = match group_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid private group ID: {}", e);
                    return;
                }
            };

            // Set typed active conversation
            ui_ctx.chat_state().set_active_conversation(chatter_core::state::ActiveConversation::AnonGroup(group_id));

            // Load pinned messages
            let anon_group_service = ui_ctx.anon_groups().clone();
            let (msg_ids, preview) = match anon_group_service.list_pinned(&group_id) {
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
            let channel_id = group_id.to_string().parse().unwrap_or_else(|_| ChannelId::new());
            super::apply_pinned_ui(&weak, &ui_ctx.pinned_state(), channel_id, msg_ids, preview);

            // Mark messages as read
            let handle = chatter_core::services::conversation::ConversationHandle::new(
                chatter_core::state::ActiveConversation::AnonGroup(group_id),
                ui_ctx.clone(),
            );
            let _ = handle.mark_as_read().await;

            load_private_group_messages_internal(weak, group_id, ui_ctx).await;
        });
    });
}

/// Internal shared loading logic for private groups.
async fn load_private_group_messages_internal(
    weak: slint::Weak<MainWindow>,
    group_id: AnonGroupId,
    ui_ctx: Arc<ChatterClient>,
) {
    let anon_group_service = ui_ctx.anon_groups().clone();
    let user_service = ui_ctx.users().clone();
    let attachment_service = ui_ctx.attachments().clone();
    let auth_state = ui_ctx.auth_state().clone();
    
    // Get group info
    let group = match anon_group_service.get_group(&group_id) {
        Ok(Some(g)) => g,
        _ => {
            run_ui!(weak, |w| w.set_loading_messages(false));
            return;
        }
    };

    let member_count = anon_group_service
        .get_members(&group_id)
        .map(|m| m.len() as i32)
        .unwrap_or(0);

    let current_user_id_str = auth_state.get().current_user_id.map(|id| id.to_string()).unwrap_or_default();

    let mut messages = anon_group_service.get_messages_asc(&group_id, None, 50).unwrap_or_default();

    // Populate reactions
    if let Ok(reaction_map) = anon_group_service.get_all_reaction_counts(&group_id) {
        for msg in &mut messages {
            if let Some(counts) = reaction_map.get(&msg.sent_at) {
                msg.reaction_counts = counts
                    .iter()
                    .map(|(emoji, count)| chatter_core::domain::message::ReactionCount {
                        emoji: emoji.clone(),
                        count: *count,
                    })
                    .collect();
            }
        }
    }

    let mut display_messages = Vec::with_capacity(messages.len());
    for msg in &messages {
        let sender_id = msg.sender_id.as_ref();
        let mut sender_name = sender_id.map(|id| user_service.get_display_name(id)).unwrap_or_default();
        let mut sender_avatar = None;

        if let Some(sid) = sender_id {
            if let Ok(Some(member)) = anon_group_service.get_member(&group_id, sid) {
                if let Some(ref name) = member.display_name {
                    if !name.is_empty() { sender_name = name.clone(); }
                }
                sender_avatar = member.avatar.clone();
            }
        }

        let resolved_attachments: Vec<_> = msg.attachments
            .iter()
            .filter_map(|att_id| attachment_service.get(att_id).ok().flatten())
            .collect();

        let mut display_data = crate::bin_ui::converters::anon_group_message_to_display_data(
            msg,
            &sender_name,
            sender_avatar,
            &resolved_attachments,
        );

        if display_data.is_reply {
            if let Some(ref author_id) = msg.reply_to_author_id {
                display_data.reply_to_sender_name = anon_group_service
                    .get_member_display_name(&group_id, author_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| user_service.get_display_name(author_id));
            }
        }
        display_messages.push(display_data);
    }

    for msg in &mut display_messages {
        msg.decode_attachment_thumbnails();
    }

    let group_name = group.name.clone();
    let group_id_str = group_id.to_string();
    run_ui!(weak, |w| {
        w.set_selected_private_group_id(group_id_str.clone().into());
        w.set_selected_channel_id("".into());
        w.set_selected_dm_id("".into());
        
        // Clear unread count in sidebar
        crate::bin_ui::features::auth::inbox::domain_handler::update_unread_count(&w, &group_id_str, 0);

        w.set_channel_name(group_name.into());
        w.set_channel_type("private-group".into());
        w.set_is_dm(false);
        w.set_member_count(member_count);
        w.set_topic("".into());
        w.set_header_presence_status("offline".into());

        let slint_messages: Vec<_> = display_messages.iter().map(|m| m.to_slint_with_user(&current_user_id_str)).collect();
        w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
        w.set_loading_messages(false);
        
        crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), attachment_service);
    });
}

pub fn add_private_group_to_sidebar(window: &MainWindow, group_id: &str, name: &str, member_count: i32) {
    let group_id = group_id.to_string();
    let name = name.to_string();

    run_ui!(window.as_weak(), |w| {
        let existing = w.get_private_groups();
        let mut groups: Vec<PrivateGroupListItemData> = (0..existing.row_count())
            .filter_map(|i| existing.row_data(i))
            .collect();

        if groups.iter().any(|g| g.group_id.as_str() == group_id) {
            return;
        }

        groups.insert(0, PrivateGroupListItemData {
            group_id: group_id.into(),
            name: name.into(),
            member_count,
            unread_count: 0,
            last_message: SharedString::default(),
            timestamp: SharedString::default(),
            is_typing: false,
        });

        w.set_private_groups(ModelRc::new(VecModel::from(groups)));
    });
}
