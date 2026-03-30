//! Custom section callbacks for the sidebar.
//!
//! Wires section management (create, delete, rename, toggle, move) and
//! loads section data on startup.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::SectionId;
use chatter_core::domain::message::ConversationType;

use crate::bin_ui::converters::{avatar_bytes_to_image, get_initials};
use crate::{CustomSectionData, MainWindow, SectionConversationData};

/// Send-safe intermediate for `SectionConversationData`.
///
/// `slint::Image` is not `Send`, so this holds raw avatar bytes and
/// is converted to `SectionConversationData` on the UI thread.
struct SectionConvSendData {
    conversation_id: String,
    section_id: String,
    conversation_type: String,
    display_name: String,
    avatar_bytes: Option<Vec<u8>>,
    initials: String,
    last_message: String,
    timestamp: String,
    unread_count: i32,
}

impl SectionConvSendData {
    fn to_slint(self) -> SectionConversationData {
        let (avatar_source, has_avatar) = self
            .avatar_bytes
            .as_deref()
            .and_then(avatar_bytes_to_image)
            .map(|img| (img, true))
            .unwrap_or_default();

        SectionConversationData {
            conversation_id: self.conversation_id.into(),
            section_id: self.section_id.into(),
            conversation_type: self.conversation_type.into(),
            display_name: self.display_name.into(),
            avatar_source,
            has_avatar,
            initials: self.initials.into(),
            last_message: self.last_message.into(),
            timestamp: self.timestamp.into(),
            unread_count: self.unread_count,
        }
    }
}

/// Load all custom sections from the database and set them on the window.
pub(crate) fn load_sections(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let section_service = ui_ctx.sections().clone();
    let channel_service = ui_ctx.channels().clone();
    let anon_dm_service = ui_ctx.anon_dms().clone();
    let anon_group_service = ui_ctx.anon_groups().clone();

    match section_service.list_sections_with_conversations() {
        Ok(sections) => {
            let (section_models, send_data) = build_section_data(
                &sections,
                &channel_service,
                &anon_dm_service,
                &anon_group_service,
            );

            // On startup we're on the UI thread, so convert directly.
            let conversation_models: Vec<_> =
                send_data.into_iter().map(|d| d.to_slint()).collect();

            window.set_custom_sections(ModelRc::new(VecModel::from(section_models)));
            window.set_section_conversations(ModelRc::new(VecModel::from(conversation_models)));
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to load custom sections");
        }
    }
}

/// Wire all section-related callbacks.
pub(crate) fn wire_section_callbacks(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_create_section(window, ui_ctx);
    wire_delete_section(window, ui_ctx);
    wire_rename_section(window, ui_ctx);
    wire_toggle_expanded(window, ui_ctx);
    wire_move_to_section(window, ui_ctx);
    wire_remove_from_section(window, ui_ctx);
    wire_section_conversation_selected(window, ui_ctx);
}

fn wire_create_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_create_section_clicked(move || {
        let section_service = ui_ctx.sections().clone();
        let channel_service = ui_ctx.channels().clone();
        let anon_dm_service = ui_ctx.anon_dms().clone();
        let anon_group_service = ui_ctx.anon_groups().clone();
        let weak = weak.clone();

        tokio::spawn(async move {
            match section_service.create_section("New Section", "\u{1F4C1}") {
                Ok(section) => {
                    tracing::info!(section_id = %section.id, "Created custom section");
                    refresh_sections_ui(
                        &weak,
                        &section_service,
                        &channel_service,
                        &anon_dm_service,
                        &anon_group_service,
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create section");
                }
            }
        });
    });
}

fn wire_delete_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_delete_section_clicked(move |section_id: SharedString| {
        let section_id_str = section_id.to_string();
        let section_service = ui_ctx.sections().clone();
        let channel_service = ui_ctx.channels().clone();
        let anon_dm_service = ui_ctx.anon_dms().clone();
        let anon_group_service = ui_ctx.anon_groups().clone();
        let weak = weak.clone();

        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<uuid::Uuid>() else {
                tracing::error!(section_id = %section_id_str, "Invalid section ID");
                return;
            };
            let id = SectionId(id);

            match section_service.delete_section(&id) {
                Ok(()) => {
                    tracing::info!(section_id = %section_id_str, "Deleted custom section");
                    refresh_sections_ui(
                        &weak,
                        &section_service,
                        &channel_service,
                        &anon_dm_service,
                        &anon_group_service,
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, section_id = %section_id_str, "Failed to delete section");
                }
            }
        });
    });
}

fn wire_rename_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let _ui_ctx = ui_ctx.clone();
    let _weak = window.as_weak();

    window.on_rename_section_clicked(move |section_id: SharedString| {
        // The rename callback from Slint currently only passes the section_id.
        // The UI handles showing the rename dialog and will call a separate
        // mechanism when the user confirms. Log for now.
        tracing::debug!(section_id = %section_id, "Rename section requested");
    });
}

fn wire_toggle_expanded(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_section_toggle_expanded(move |section_id: SharedString| {
        let section_id_str = section_id.to_string();
        let section_service = ui_ctx.sections().clone();
        let channel_service = ui_ctx.channels().clone();
        let anon_dm_service = ui_ctx.anon_dms().clone();
        let anon_group_service = ui_ctx.anon_groups().clone();
        let weak = weak.clone();

        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<uuid::Uuid>() else {
                tracing::error!(section_id = %section_id_str, "Invalid section ID for toggle");
                return;
            };
            let id = SectionId(id);

            // Read current state and toggle
            let sections = match section_service.list_sections_with_conversations() {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to list sections for toggle");
                    return;
                }
            };

            let Some(section) = sections.iter().find(|s| s.id == id) else {
                tracing::warn!(section_id = %section_id_str, "Section not found for toggle");
                return;
            };

            let new_expanded = !section.is_expanded;
            if let Err(e) = section_service.toggle_expanded(&id, new_expanded) {
                tracing::error!(error = %e, "Failed to toggle section expanded state");
                return;
            }

            refresh_sections_ui(
                &weak,
                &section_service,
                &channel_service,
                &anon_dm_service,
                &anon_group_service,
            );
        });
    });
}

fn wire_move_to_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_move_to_section(move |conv_id: SharedString, conv_type: SharedString, section_id: SharedString| {
        let conv_id_str = conv_id.to_string();
        let conv_type_str = conv_type.to_string();
        let section_id_str = section_id.to_string();
        let section_service = ui_ctx.sections().clone();
        let channel_service = ui_ctx.channels().clone();
        let anon_dm_service = ui_ctx.anon_dms().clone();
        let anon_group_service = ui_ctx.anon_groups().clone();
        let weak = weak.clone();

        tokio::spawn(async move {
            let Ok(sid) = section_id_str.parse::<uuid::Uuid>() else {
                tracing::error!(section_id = %section_id_str, "Invalid section ID for move");
                return;
            };
            let sid = SectionId(sid);

            let Ok(conv_type) = ConversationType::from_str_value(&conv_type_str) else {
                tracing::error!(conv_type = %conv_type_str, "Invalid conversation type for move");
                return;
            };

            match section_service.move_conversation_to_section(&conv_id_str, conv_type, &sid) {
                Ok(()) => {
                    tracing::info!(
                        conversation_id = %conv_id_str,
                        section_id = %section_id_str,
                        "Moved conversation to section"
                    );
                    refresh_sections_ui(
                        &weak,
                        &section_service,
                        &channel_service,
                        &anon_dm_service,
                        &anon_group_service,
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to move conversation to section");
                }
            }
        });
    });
}

fn wire_remove_from_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_remove_from_section(move |conv_id: SharedString| {
        let conv_id_str = conv_id.to_string();
        let section_service = ui_ctx.sections().clone();
        let channel_service = ui_ctx.channels().clone();
        let anon_dm_service = ui_ctx.anon_dms().clone();
        let anon_group_service = ui_ctx.anon_groups().clone();
        let weak = weak.clone();

        tokio::spawn(async move {
            match section_service.remove_conversation_from_section(&conv_id_str) {
                Ok(()) => {
                    tracing::info!(conversation_id = %conv_id_str, "Removed conversation from section");
                    refresh_sections_ui(
                        &weak,
                        &section_service,
                        &channel_service,
                        &anon_dm_service,
                        &anon_group_service,
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to remove conversation from section");
                }
            }
        });
    });
}

fn wire_section_conversation_selected(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let _ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_section_conversation_selected(move |conv_id: SharedString, conv_type: SharedString| {
        let conv_id_str = conv_id.to_string();
        let conv_type_str = conv_type.to_string();
        let weak = weak.clone();

        // Route to the correct existing selection callback based on conversation type.
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };

            match conv_type_str.as_str() {
                "channel" | "saved_messages" => {
                    w.invoke_channel_selected(conv_id_str.into());
                }
                "server_dm" | "mailbox_dm" | "anon_dm" => {
                    w.invoke_dm_selected(conv_id_str.into());
                }
                "anon_group" => {
                    w.invoke_private_group_selected(conv_id_str.into());
                }
                _ => {
                    tracing::warn!(conv_type = %conv_type_str, "Unknown conversation type in section");
                }
            }
        });
    });
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Build Send-safe section and conversation data from domain objects.
fn build_section_data(
    sections: &[chatter_core::domain::section::CustomSection],
    channel_service: &Arc<chatter_core::services::channel::ChannelService>,
    anon_dm_service: &Arc<chatter_core::services::anon_dm::AnonDmService>,
    anon_group_service: &Arc<chatter_core::services::anon_group::AnonGroupService>,
) -> (Vec<CustomSectionData>, Vec<SectionConvSendData>) {
    let mut section_models = Vec::with_capacity(sections.len());
    let mut conversation_models = Vec::new();

    for section in sections {
        section_models.push(CustomSectionData {
            section_id: section.id.to_string().into(),
            name: section.name.clone().into(),
            icon_emoji: section.icon_emoji.clone().into(),
            is_expanded: section.is_expanded,
            unread_count: section.unread_count as i32,
        });

        for conv in &section.conversations {
            conversation_models.push(enrich_conversation_send(
                &conv.conversation_id,
                &conv.conversation_type,
                &section.id,
                channel_service,
                anon_dm_service,
                anon_group_service,
            ));
        }
    }

    (section_models, conversation_models)
}

/// Refresh the section UI models by re-reading from the database.
fn refresh_sections_ui(
    weak: &slint::Weak<MainWindow>,
    section_service: &Arc<chatter_core::services::section_service::SectionService>,
    channel_service: &Arc<chatter_core::services::channel::ChannelService>,
    anon_dm_service: &Arc<chatter_core::services::anon_dm::AnonDmService>,
    anon_group_service: &Arc<chatter_core::services::anon_group::AnonGroupService>,
) {
    let sections = match section_service.list_sections_with_conversations() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "Failed to refresh sections");
            return;
        }
    };

    let (section_models, send_data) =
        build_section_data(&sections, channel_service, anon_dm_service, anon_group_service);

    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            let conversation_models: Vec<_> =
                send_data.into_iter().map(|d| d.to_slint()).collect();
            w.set_custom_sections(ModelRc::new(VecModel::from(section_models)));
            w.set_section_conversations(ModelRc::new(VecModel::from(conversation_models)));
        }
    });
}

/// Enrich a section conversation with display data (Send-safe version).
fn enrich_conversation_send(
    conversation_id: &str,
    conversation_type: &ConversationType,
    section_id: &SectionId,
    channel_service: &Arc<chatter_core::services::channel::ChannelService>,
    anon_dm_service: &Arc<chatter_core::services::anon_dm::AnonDmService>,
    anon_group_service: &Arc<chatter_core::services::anon_group::AnonGroupService>,
) -> SectionConvSendData {
    let mut data = SectionConvSendData {
        conversation_id: conversation_id.to_string(),
        section_id: section_id.to_string(),
        conversation_type: conversation_type.as_str().to_string(),
        display_name: String::new(),
        avatar_bytes: None,
        initials: String::new(),
        last_message: String::new(),
        timestamp: String::new(),
        unread_count: 0,
    };

    match conversation_type {
        ConversationType::Channel
        | ConversationType::SavedMessages
        | ConversationType::ServerDm
        | ConversationType::MailboxDm => {
            if let Ok(id) = conversation_id.parse() {
                if let Ok(Some(channel)) = channel_service.get_channel_local(&id) {
                    data.display_name = channel.name.clone();
                    data.initials = get_initials(&channel.name);
                    data.unread_count = channel.unread_count as i32;
                }
            }
        }
        ConversationType::AnonDm => {
            if let Ok(id) = conversation_id.parse() {
                if let Ok(Some(dm)) = anon_dm_service.get_conversation(&id) {
                    let name = dm
                        .peer_display_name
                        .as_deref()
                        .unwrap_or("Anonymous");
                    data.display_name = name.to_string();
                    data.initials = get_initials(name);
                    data.unread_count = dm.unread_count as i32;
                    data.avatar_bytes = dm.peer_avatar.clone();
                }
            }
        }
        ConversationType::AnonGroup => {
            if let Ok(id) = conversation_id.parse() {
                if let Ok(Some(group)) = anon_group_service.get_group(&id) {
                    data.display_name = group.name.clone();
                    data.initials = get_initials(&group.name);
                }
            }
        }
    }

    data
}
