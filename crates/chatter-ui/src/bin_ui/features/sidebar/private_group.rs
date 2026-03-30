//! Private group selection and loading.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_group::AnonGroupId;

use crate::{MainWindow, PrivateGroupListItemData};

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
        let mut messages = match anon_group_service.get_messages(&group_id, None, 50) {
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
                        tracing::debug!(
                            "Found member {} in group {}: display_name={:?}",
                            sid,
                            group_id,
                            member.display_name
                        );
                        if let Some(ref name) = member.display_name {
                            if !name.is_empty() {
                                sender_name = name.clone();
                            }
                        }
                        sender_avatar = member.avatar.clone();
                    }
                    Ok(None) => {
                        tracing::warn!(
                            "Member {} not found in group {} member list",
                            sid,
                            group_id
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            "Failed to get member {} in group {}: {}",
                            sid,
                            group_id,
                            e
                        );
                    }
                }
            }

            // Resolve attachments from stored IDs
            let resolved_attachments: Vec<chatter_core::domain::Attachment> = msg.attachments
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

        // Reverse to show oldest first (messages come from DB in DESC order)
        display_messages.reverse();
        
        // Pre-decode attachment thumbnails before UI thread
        for msg in &mut display_messages {
            msg.decode_attachment_thumbnails();
        }
        
        // Update UI to show selected group and messages
        let group_name = group.name.clone();
        let attachment_service_for_gifs = attachment_service.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_window.upgrade() {
                // Clear other selections
                w.set_selected_channel_id("".into());
                w.set_selected_dm_id("".into());
                w.set_selected_private_group_id(group_id_str.into());
                
                // Set chat header info for private group
                // The header UI will show users icon and "Private" badge based on channel-type
                w.set_channel_name(group_name.into());
                w.set_channel_type("private-group".into());
                w.set_is_dm(false);
                w.set_member_count(member_count);
                w.set_topic("".into()); // No topic for private groups
                
                // Convert and set messages
                let slint_messages: Vec<_> = display_messages
                    .iter()
                    .map(|m| m.to_slint_with_user(&current_user_id_str))
                    .collect();
                w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
                w.set_loading_messages(false);
                
                // Auto-register GIF animations for inline playback
                crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), attachment_service_for_gifs);
                
                tracing::info!(
                    "Loaded private group: {} with {} members and {} messages",
                    group_id,
                    member_count,
                    display_messages.len()
                );
            }
        });
    });
}

pub(crate) fn wire_private_group_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let anon_group_service = ui_ctx.anon_groups().clone();
    let user_service = ui_ctx.users().clone();
    let attachment_service = ui_ctx.attachments().clone();
    let auth_state = ui_ctx.auth_state().clone();
    let pinned_state = ui_ctx.pinned_state().clone();
    let chat_state = ui_ctx.chat_state().clone();
    let weak_window = window.as_weak();

    window.on_private_group_selected(move |group_id: SharedString| {
        let group_id_str = group_id.to_string();
        let weak = weak_window.clone();
        let ui_ctx = ui_ctx.clone();
        let anon_group_service = anon_group_service.clone();
        let user_service = user_service.clone();
        let attachment_service = attachment_service.clone();
        let auth_state = auth_state.clone();
        let pinned_state = pinned_state.clone();
        let chat_state = chat_state.clone();

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
            chat_state.set_active_conversation(chatter_core::state::ActiveConversation::AnonGroup(group_id));

            // Load pinned messages for this group from DB
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
            let channel_id = group_id.to_string().parse()
                .unwrap_or_else(|_| chatter_core::domain::ids::ChannelId::new());
            super::apply_pinned_ui(&weak, &pinned_state, channel_id, msg_ids, preview);

            // Mark messages as read when opening the group
            let handle = chatter_core::services::conversation::ConversationHandle::new(
                chatter_core::state::ActiveConversation::AnonGroup(group_id),
                ui_ctx.clone(),
            );
            if let Err(e) = handle.mark_as_read().await {
                tracing::warn!(error = %e, "Failed to mark group messages as read");
            }

            // Get group info from local storage
            let group = match anon_group_service.get_group(&group_id) {
                Ok(Some(g)) => g,
                Ok(None) => {
                    tracing::error!("Private group not found: {}", group_id);
                    return;
                }
                Err(e) => {
                    tracing::error!("Failed to load private group: {}", e);
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
            let mut messages = match anon_group_service.get_messages(&group_id, None, 50) {
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
                            tracing::debug!(
                                "Found member {} in group {}: display_name={:?}",
                                sid,
                                group_id,
                                member.display_name
                            );
                            if let Some(ref name) = member.display_name {
                                if !name.is_empty() {
                                    sender_name = name.clone();
                                }
                            }
                            sender_avatar = member.avatar.clone();
                        }
                        Ok(None) => {
                            tracing::warn!(
                                "Member {} not found in group {} member list",
                                sid,
                                group_id
                            );
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to get member {} in group {}: {}",
                                sid,
                                group_id,
                                e
                            );
                        }
                    }
                }

                // Resolve attachments from stored IDs
                let resolved_attachments: Vec<chatter_core::domain::Attachment> = msg.attachments
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

            // Reverse to show oldest first (messages come from DB in DESC order)
            display_messages.reverse();

            // Pre-decode attachment thumbnails before UI thread
            for msg in &mut display_messages {
                msg.decode_attachment_thumbnails();
            }

            // Update UI to show selected group and messages
            let group_name = group.name.clone();
            let attachment_service_for_gifs = attachment_service.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    // Clear other selections
                    w.set_selected_channel_id("".into());
                    w.set_selected_dm_id("".into());
                    w.set_selected_private_group_id(group_id.to_string().into());

                    // Set chat header info for private group
                    // The header UI will show users icon and "Private" badge based on channel-type
                    w.set_channel_name(group_name.into());
                    w.set_channel_type("private-group".into());
                    w.set_is_dm(false);
                    w.set_member_count(member_count);
                    w.set_topic("".into()); // No topic for private groups

                    // Convert and set messages
                    let slint_messages: Vec<_> = display_messages
                        .iter()
                        .map(|m| m.to_slint_with_user(&current_user_id_str))
                        .collect();
                    w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
                    w.set_loading_messages(false);

                    // Auto-register GIF animations for inline playback
                    crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), attachment_service_for_gifs);

                    tracing::info!(
                        "Selected private group: {} with {} members and {} messages",
                        group_id,
                        member_count,
                        display_messages.len()
                    );
                }
            });
        });
    });
}

/// Add a single private group to the sidebar.
/// Call this after creating a new group.
pub fn add_private_group_to_sidebar(window: &MainWindow, group_id: &str, name: &str, member_count: i32) {
    let weak_window = window.as_weak();
    let group_id = group_id.to_string();
    let name = name.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak_window.upgrade() {
            let existing = w.get_private_groups();
            let mut groups: Vec<PrivateGroupListItemData> = (0..existing.row_count())
                .filter_map(|i| existing.row_data(i))
                .collect();

            // Avoid duplicates
            if groups.iter().any(|g| g.group_id.as_str() == group_id) {
                return;
            }

            // Add new group at the beginning
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
        }
    });
}
