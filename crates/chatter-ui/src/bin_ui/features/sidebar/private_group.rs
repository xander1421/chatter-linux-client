//! Private group selection and loading.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::anon_group::AnonGroupId;
use chatter_core::domain::ids::ChannelId;

use crate::{MainWindow, PrivateGroupListItemData, run_ui};

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
    
    let ui_ctx = ui_ctx.clone();
    let auth_state = ui_ctx.auth_state().clone();
    let weak_window = window.as_weak();
    let group_id_str = group_id_str.to_string();

    tokio::spawn(async move {
        // Get group details
        let group = match ui_ctx.get_anon_group(&group_id) {
            Some(g) => g,
            None => {
                tracing::error!("Private group {} not found", group_id);
                return;
            }
        };

        // Get member count for header display
        let member_count = ui_ctx.get_anon_group_members(&group_id).len() as i32;

        // Get current user ID for is_own calculation
        let current_user_id = auth_state.get().current_user_id;
        let current_user_id_str = current_user_id.map(|id| id.to_string()).unwrap_or_default();

        // Load messages for this group (reaction counts populated in core).
        let messages = match ui_ctx.get_anon_group_messages_asc(&group_id, None, chatter_core::config::MESSAGE_PAGE_SIZE) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::error!("Failed to load messages for private group {}: {}", group_id, e);
                Vec::new()
            }
        };

        // Convert messages to display data
        let mut display_messages: Vec<crate::bin_ui::converters::MessageDisplayData> = Vec::with_capacity(messages.len());
        for msg in &messages {
            // Name resolution (conversation-scoped) lives in core; avatar bytes
            // are presentation and stay here.
            let sender_name = ui_ctx.message_sender_display_name(msg);
            let sender_avatar = msg.sender_id.as_ref()
                .and_then(|sid| ui_ctx.get_anon_group_member(&group_id, sid))
                .and_then(|member| member.avatar);

            // Resolve attachments from stored IDs
            let resolved_attachments: Vec<chatter_core::domain::Attachment> = msg.attachments
                .iter()
                .filter_map(|att_id| ui_ctx.get_attachment(att_id))
                .collect();

            let mut display_data = crate::bin_ui::converters::message_to_display_data(
                msg,
                &sender_name,
                &resolved_attachments,
                sender_avatar,
            );

            if display_data.is_reply {
                display_data.reply_to_sender_name = ui_ctx.reply_author_display_name(msg);
                display_data.reply_to_not_found = ui_ctx.reply_parent_missing_for(msg);
            }

            display_messages.push(display_data);
        }

        // Pre-decode attachment thumbnails before UI thread
        for msg in &mut display_messages {
            msg.decode_attachment_thumbnails();
        }
        
        // Update UI to show selected group and messages
        let group_name = group.name.clone();
        let ui_ctx_gifs = ui_ctx.clone();
        let ui_ctx_for_ui = ui_ctx.clone();
        run_ui!(weak_window, |w| {
            // Clear unread count in sidebar
            crate::bin_ui::features::auth::inbox::domain_handler::update_unread_count(&w, &ui_ctx_for_ui, &group_id_str, 0);

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
            w.set_channel_has_avatar(false);
            w.set_channel_avatar_source(slint::Image::default());

            // Convert and set messages
            let slint_messages: Vec<_> = display_messages
                .iter()
                .map(|m| m.to_slint_with_user(&current_user_id_str))
                .collect();
            w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
            w.set_loading_messages(false);
            
            // Auto-register GIF animations
            crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), ui_ctx_gifs);
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

        crate::bin_ui::gif_state::clear_inline_gifs();

        // Commit active-conversation state SYNCHRONOUSLY on the UI thread
        // (same race fix as dm.rs — see comment there).
        if let Ok(parsed) = group_id_str.parse::<AnonGroupId>() {
            ui_ctx.chat_state().set_active_conversation(chatter_core::state::ActiveConversation::AnonGroup(parsed));
        }

        tokio::spawn(async move {
            // Parse group ID
            let group_id: AnonGroupId = match group_id_str.parse() {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("Invalid private group ID: {}", e);
                    return;
                }
            };

            // Active conversation already committed pre-spawn.
            let ui_ctx_for_sel = ui_ctx.clone();
            run_ui!(weak, |w| super::refresh_selection(&w, &ui_ctx_for_sel));

            // Load pinned messages
            let pinned = ui_ctx.list_anon_group_pinned(&group_id);
            let (msg_ids, preview) = if pinned.is_empty() {
                (vec![], String::new())
            } else {
                let ids = pinned.iter().map(|m| m.id).collect();
                let preview = pinned[0].as_text()
                    .filter(|t| !t.is_empty())
                    .map(|t| if t.len() > 80 { format!("{}...", &t[..77]) } else { t.to_string() })
                    .unwrap_or_else(|| "[Pinned message]".to_string());
                (ids, preview)
            };
            let channel_id = group_id.to_string().parse().unwrap_or_else(|_| ChannelId::new());
            super::apply_pinned_ui(&weak, &ui_ctx.chat_state(), channel_id, msg_ids, preview);

            // Mark messages as read
            let handle = ui_ctx.conversation(chatter_core::state::ActiveConversation::AnonGroup(group_id));
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
    let auth_state = ui_ctx.auth_state().clone();

    // Get group info
    let group = match ui_ctx.get_anon_group(&group_id) {
        Some(g) => g,
        None => {
            run_ui!(weak, |w| w.set_loading_messages(false));
            return;
        }
    };

    let member_count = ui_ctx.get_anon_group_members(&group_id).len() as i32;

    let current_user_id_str = auth_state.get().current_user_id.map(|id| id.to_string()).unwrap_or_default();

    // Reaction counts populated in core.
    let messages = ui_ctx.get_anon_group_messages_asc(&group_id, None, chatter_core::config::MESSAGE_PAGE_SIZE).unwrap_or_default();

    let mut display_messages = Vec::with_capacity(messages.len());
    for msg in &messages {
        // Name resolution (conversation-scoped) lives in core; avatar bytes
        // are presentation and stay here.
        let sender_name = ui_ctx.message_sender_display_name(msg);
        let sender_avatar = msg.sender_id.as_ref()
            .and_then(|sid| ui_ctx.get_anon_group_member(&group_id, sid))
            .and_then(|member| member.avatar);

        let resolved_attachments: Vec<_> = msg.attachments
            .iter()
            .filter_map(|att_id| ui_ctx.get_attachment(att_id))
            .collect();

        let mut display_data = crate::bin_ui::converters::message_to_display_data(
            msg,
            &sender_name,
            &resolved_attachments,
            sender_avatar,
        );

        if display_data.is_reply {
            display_data.reply_to_sender_name = ui_ctx.reply_author_display_name(msg);
            display_data.reply_to_not_found = ui_ctx.reply_parent_missing_for(msg);
        }
        display_messages.push(display_data);
    }

    for msg in &mut display_messages {
        msg.decode_attachment_thumbnails();
    }

    let group_name = group.name.clone();
    let group_id_str = group_id.to_string();
    let ui_ctx_gifs = ui_ctx.clone();
    let ui_ctx_for_ui = ui_ctx.clone();
    run_ui!(weak, |w| {
        // Clear unread count in sidebar
        crate::bin_ui::features::auth::inbox::domain_handler::update_unread_count(&w, &ui_ctx_for_ui, &group_id_str, 0);

        w.set_channel_name(group_name.into());
        w.set_channel_type("private-group".into());
        w.set_is_dm(false);
        w.set_member_count(member_count);
        w.set_topic("".into());
        w.set_header_presence_status("offline".into());
        w.set_channel_has_avatar(false);
        w.set_channel_avatar_source(slint::Image::default());

        let slint_messages: Vec<_> = display_messages.iter().map(|m| m.to_slint_with_user(&current_user_id_str)).collect();
        w.set_messages(ModelRc::new(VecModel::from(slint_messages)));
        w.set_loading_messages(false);
        
        crate::bin_ui::gif_state::auto_register_gifs_from_messages(w.as_weak(), ui_ctx_gifs);
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
            is_selected: false,
        });

        w.set_private_groups(ModelRc::new(VecModel::from(groups)));
    });
}
