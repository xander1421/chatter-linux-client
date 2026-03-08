//! Private group selection and loading.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::UIContext;
use chatter_linux::domain::anon_group::AnonGroupId;

use crate::{MainWindow, PrivateGroupListItemData};

use super::stop_audio_playback;

pub(crate) fn wire_private_group_selected(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let user_service = ui_ctx.user_service.clone();
    let attachment_service = ui_ctx.attachment_service.clone();
    let auth_state = ui_ctx.auth_state.clone();
    let weak_window = window.as_weak();

    window.on_private_group_selected(move |group_id: SharedString| {
        let group_id_str = group_id.to_string();
        let weak = weak_window.clone();
        let anon_group_service = anon_group_service.clone();
        let user_service = user_service.clone();
        let attachment_service = attachment_service.clone();
        let auth_state = auth_state.clone();

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
            let messages = match anon_group_service.get_messages(&group_id, None, 50) {
                Ok(msgs) => msgs,
                Err(e) => {
                    tracing::error!("Failed to load messages for private group {}: {}", group_id, e);
                    Vec::new()
                }
            };

            // Convert messages to display data
            let mut display_messages: Vec<crate::bin_ui::converters::MessageDisplayData> = Vec::with_capacity(messages.len());
            for msg in &messages {
                let mut sender_name = user_service.get_display_name(&msg.sender_id);
                if let Ok(Some(name)) = anon_group_service.get_member_display_name(&group_id, &msg.sender_id) {
                    if !name.is_empty() {
                        sender_name = name;
                    }
                }

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

                let display_data = crate::bin_ui::converters::anon_group_message_to_display_data(
                    msg,
                    &sender_name,
                    &resolved_attachments,
                );
                display_messages.push(display_data);
            }

            // Reverse to show oldest first (messages come from DB in DESC order)
            display_messages.reverse();

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

/// Load all private groups into the sidebar.
/// Call this after login to populate the private groups list.
///
/// Note: `initial_data.rs` already loads private groups with last message preview.
/// This function is kept for potential refresh use cases.
#[allow(dead_code)]
pub fn load_private_groups(window: &MainWindow, ui_ctx: &Arc<UIContext>) {
    let anon_group_service = ui_ctx.anon_group_service.clone();
    let weak_window = window.as_weak();

    tokio::spawn(async move {
        // Get all local private groups
        let groups = match anon_group_service.list_groups() {
            Ok(g) => g,
            Err(e) => {
                tracing::error!("Failed to load private groups: {}", e);
                return;
            }
        };

        if groups.is_empty() {
            return;
        }

        // Convert to UI data
        let group_data: Vec<PrivateGroupListItemData> = groups
            .into_iter()
            .map(|g| {
                let member_count = anon_group_service
                    .get_members(&g.id)
                    .map(|m| m.len())
                    .unwrap_or(0) as i32;

                PrivateGroupListItemData {
                    group_id: g.id.to_string().into(),
                    name: g.name.into(),
                    member_count,
                    unread_count: 0, // TODO: track unread count
                    last_message: SharedString::default(),
                    timestamp: SharedString::default(),
                    is_typing: false,
                }
            })
            .collect();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_window.upgrade() {
                w.set_private_groups(ModelRc::new(VecModel::from(group_data)));
                tracing::info!("Loaded {} private groups into sidebar", w.get_private_groups().row_count());
            }
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
