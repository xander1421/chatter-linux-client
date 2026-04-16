//! Custom section callbacks for the sidebar.
//!
//! Wires section management (create, delete, rename, toggle, move) and
//! loads section data on startup.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::ChannelType;
use chatter_core::domain::ids::SectionId;
use chatter_core::domain::message::ConversationType;

use chatter_core::services::channel::ChannelService;

use crate::bin_ui::converters::{avatar_bytes_to_image, channel_to_slint, get_initials};
use crate::{CustomSectionData, MainWindow, SectionConversationData, run_ui};

/// Send-safe intermediate for `SectionConversationData`.
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

    match section_service.list_sections_with_conversations() {
        Ok(sections) => {
            let (section_models, send_data) = build_section_data(&sections, &section_service);

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

    window.on_create_section(move |name: SharedString, emoji: SharedString| {
        let name_str = name.to_string();
        let emoji_str = emoji.to_string();
        let section_service = ui_ctx.sections().clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            match section_service.create_section(&name_str, &emoji_str) {
                Ok(section) => {
                    tracing::info!(section_id = %section.id, name = %name_str, "Created custom section");
                    run_ui!(weak_clone, |w| w.set_active_modal("none".into()));
                    refresh_sections_ui(&weak_clone, &section_service);
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
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<uuid::Uuid>() else {
                tracing::error!(section_id = %section_id_str, "Invalid section ID");
                return;
            };
            let id = SectionId(id);

            match section_service.delete_section(&id) {
                Ok(()) => {
                    tracing::info!(section_id = %section_id_str, "Deleted custom section");
                    refresh_sections_ui(&weak_clone, &section_service);
                }
                Err(e) => {
                    tracing::error!(error = %e, section_id = %section_id_str, "Failed to delete section");
                }
            }
        });
    });
}

fn wire_rename_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    {
        let section_service = ui_ctx.clone().sections().clone();
        let weak = window.as_weak();

        window.on_rename_section_clicked(move |section_id: SharedString| {
            let section_id_str = section_id.to_string();
            if let Ok(sections) = section_service.list_sections_with_conversations() {
                if let Some(section) = sections.iter().find(|s| s.id.to_string() == section_id_str) {
                    let name: SharedString = section.name.clone().into();
                    let emoji: SharedString = section.icon_emoji.clone().into();
                    run_ui!(weak, |w| {
                        w.set_create_section_name(name);
                        w.set_create_section_emoji(emoji);
                    });
                }
            }
        });
    }

    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_rename_section(move |section_id: SharedString, name: SharedString, emoji: SharedString| {
        let section_id_str = section_id.to_string();
        let name_str = name.to_string();
        let emoji_str = emoji.to_string();
        let section_service = ui_ctx.sections().clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<uuid::Uuid>() else { return };
            let id = SectionId(id);

            if let Err(e) = section_service.rename_section(&id, &name_str) {
                tracing::error!(error = %e, "Failed to rename section");
                return;
            }

            if let Err(e) = section_service.change_icon(&id, &emoji_str) {
                tracing::error!(error = %e, "Failed to change icon");
                return;
            }

            run_ui!(weak_clone, |w| w.set_active_modal("none".into()));
            refresh_sections_ui(&weak_clone, &section_service);
        });
    });
}

fn wire_toggle_expanded(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_section_toggle_expanded(move |section_id: SharedString| {
        let section_id_str = section_id.to_string();
        let section_service = ui_ctx.sections().clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<uuid::Uuid>() else { return };
            let id = SectionId(id);

            if let Ok(sections) = section_service.list_sections_with_conversations() {
                if let Some(section) = sections.iter().find(|s| s.id == id) {
                    let _ = section_service.toggle_expanded(&id, !section.is_expanded);
                    refresh_sections_ui(&weak_clone, &section_service);
                }
            }
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
        let attachment_service = ui_ctx.attachments().clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            let Ok(sid) = section_id_str.parse::<uuid::Uuid>() else { return };
            let Ok(conv_type) = ConversationType::from_str_value(&conv_type_str) else { return };

            if let Ok(()) = section_service.move_conversation_to_section(&conv_id_str, conv_type, &SectionId(sid)) {
                refresh_sections_ui(&weak_clone, &section_service);
                refresh_channel_list(&weak_clone, &channel_service, &attachment_service);
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
        let attachment_service = ui_ctx.attachments().clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            if let Ok(()) = section_service.remove_conversation_from_section(&conv_id_str) {
                refresh_sections_ui(&weak_clone, &section_service);
                refresh_channel_list(&weak_clone, &channel_service, &attachment_service);
            }
        });
    });
}

fn wire_section_conversation_selected(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();

    window.on_section_conversation_selected(move |conv_id: SharedString, conv_type: SharedString| {
        let conv_id_str = conv_id.to_string();
        let conv_type_str = conv_type.to_string();
        run_ui!(weak, |w| {
            match conv_type_str.as_str() {
                "channel" | "saved_messages" => w.invoke_channel_selected(conv_id_str.into()),
                "server_dm" | "mailbox_dm" | "anon_dm" => w.invoke_dm_selected(conv_id_str.into()),
                "anon_group" => w.invoke_private_group_selected(conv_id_str.into()),
                _ => {}
            }
        });
    });
}

fn refresh_sections_ui(
    weak: &slint::Weak<MainWindow>,
    section_service: &Arc<chatter_core::services::section_service::SectionService>,
) {
    if let Ok(sections) = section_service.list_sections_with_conversations() {
        let (section_models, send_data) = build_section_data(&sections, section_service);
        run_ui!(weak, |w| {
            let conversation_models: Vec<_> = send_data.into_iter().map(|d| d.to_slint()).collect();
            w.set_custom_sections(ModelRc::new(VecModel::from(section_models)));
            w.set_section_conversations(ModelRc::new(VecModel::from(conversation_models)));
        });
    }
}

fn refresh_channel_list(
    weak: &slint::Weak<MainWindow>,
    channel_service: &Arc<ChannelService>,
    attachment_service: &Arc<chatter_core::services::attachment::AttachmentService>,
) {
    if let Ok(channels) = channel_service.get_channels_mru() {
        // Filter on this thread, then convert on UI thread — slint::Image is !Send.
        let filtered: Vec<_> = channels
            .into_iter()
            .filter(|c| matches!(c.channel_type, ChannelType::Public | ChannelType::Private))
            .collect();
        let attachments = attachment_service.clone();
        run_ui!(weak, |w| {
            let channel_list: Vec<_> = filtered.iter().map(|c| channel_to_slint(c, &attachments)).collect();
            w.set_channels(ModelRc::new(VecModel::from(channel_list)));
        });
    }
}

fn build_section_data(
    sections: &[chatter_core::domain::section::CustomSection],
    section_service: &Arc<chatter_core::services::section_service::SectionService>,
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
                section_service,
            ));
        }
    }

    (section_models, conversation_models)
}

fn enrich_conversation_send(
    conversation_id: &str,
    conversation_type: &ConversationType,
    section_id: &SectionId,
    section_service: &Arc<chatter_core::services::section_service::SectionService>,
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

    if let Ok(Some(meta)) = section_service.get_conversation_metadata(conversation_id, conversation_type) {
        data.display_name = meta.name.clone();
        data.initials = get_initials(&meta.name);
        data.unread_count = meta.unread_count as i32;
    }

    data
}
