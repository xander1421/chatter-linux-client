//! Custom section callbacks for the sidebar.
//!
//! Wires section management (create, delete, rename, toggle, move, reorder,
//! sort mode, pin) and loads section data on startup.

use std::sync::Arc;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::SectionId;
use chatter_core::domain::message::ConversationType;
use chatter_core::domain::section::SectionSortMode;

use chatter_core::services::attachment::DecodedImagePixels;

use crate::bin_ui::converters::{DMListItemExt, channel_to_slint, get_initials};
use crate::{CustomSectionData, MainWindow, SectionConversationData, run_ui};

/// Send-safe intermediate for `SectionConversationData`.
struct SectionConvSendData {
    conversation_id: String,
    conversation_type: ConversationType,
    display_name: String,
    avatar_decoded: Option<DecodedImagePixels>,
    initials: String,
    unread_count: i32,
    is_pinned: bool,
    can_leave: bool,
    can_delete: bool,
    extras: chatter_core::client::SectionRowExtras,
}

impl SectionConvSendData {
    fn to_slint(self) -> SectionConversationData {
        let (avatar_source, has_avatar) = match self.avatar_decoded {
            Some(pixels) => {
                let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &pixels.rgba, pixels.width, pixels.height,
                );
                (slint::Image::from_rgba8(buffer), true)
            }
            None => (slint::Image::default(), false),
        };

        SectionConversationData {
            conversation_id: self.conversation_id.into(),
            conversation_type: crate::bin_ui::converters::conversation_kind(&self.conversation_type),
            display_name: self.display_name.into(),
            avatar_source,
            has_avatar,
            initials: self.initials.into(),
            last_message: self.extras.last_message.into(),
            timestamp: self.extras.timestamp.into(),
            presence_status: self.extras.presence_status.into(),
            private_channel: self.extras.private_channel,
            unread_count: self.unread_count,
            is_pinned: self.is_pinned,
            can_leave: self.can_leave,
            can_delete: self.can_delete,
            is_selected: false,
        }
    }
}

/// Send-safe intermediate for one section and the rows it owns.
struct SectionSendData {
    section_id: String,
    name: String,
    icon_emoji: String,
    is_expanded: bool,
    unread_count: i32,
    sort_mode: String,
    conversations: Vec<SectionConvSendData>,
}

impl SectionSendData {
    fn to_slint(self) -> CustomSectionData {
        let rows: Vec<SectionConversationData> =
            self.conversations.into_iter().map(|c| c.to_slint()).collect();
        CustomSectionData {
            section_id: self.section_id.into(),
            name: self.name.into(),
            icon_emoji: self.icon_emoji.into(),
            is_expanded: self.is_expanded,
            unread_count: self.unread_count,
            sort_mode: self.sort_mode.into(),
            conversations: ModelRc::new(VecModel::from(rows)),
        }
    }
}

/// Load all custom sections from the database and set them on the window.
pub(crate) fn load_sections(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    match ui_ctx.list_sections_with_conversations() {
        Ok(sections) => {
            let send_data = build_section_data(&sections, ui_ctx);
            // On startup we're on the UI thread, so convert directly.
            let models: Vec<_> = send_data.into_iter().map(|s| s.to_slint()).collect();
            window.set_custom_sections(ModelRc::new(VecModel::from(models)));
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
    wire_move_section(window, ui_ctx);
    wire_set_sort_mode(window, ui_ctx);
    wire_set_conversation_pinned(window, ui_ctx);
}

fn wire_create_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_create_section(move |name: SharedString, emoji: SharedString| {
        let name_str = name.to_string();
        let emoji_str = emoji.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        run_ui!(weak, |w| w.set_create_section_loading(true));

        tokio::spawn(async move {
            match ui_ctx.create_section(&name_str, &emoji_str) {
                Ok(section) => {
                    tracing::info!(section_id = %section.id, name = %name_str, "Created custom section");
                    run_ui!(weak_clone, |w| {
                        w.set_create_section_loading(false);
                        crate::bin_ui::features::back::close(&w, "create-section");
                    });
                    refresh_sections_ui(&weak_clone, &ui_ctx);
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create section");
                    run_ui!(weak_clone, |w| w.set_create_section_loading(false));
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
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<SectionId>() else {
                tracing::error!(section_id = %section_id_str, "Invalid section ID");
                return;
            };

            match ui_ctx.delete_section(&id) {
                Ok(()) => {
                    tracing::info!(section_id = %section_id_str, "Deleted custom section");
                    refresh_sections_ui(&weak_clone, &ui_ctx);
                    // The folder's conversations become loose again.
                    refresh_conversation_lists(&weak_clone, &ui_ctx);
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
        let ui_ctx = ui_ctx.clone();
        let weak = window.as_weak();

        window.on_rename_section_clicked(move |section_id: SharedString| {
            let section_id_str = section_id.to_string();
            if let Ok(sections) = ui_ctx.list_sections_with_conversations() {
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
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<SectionId>() else { return };

            if let Err(e) = ui_ctx.rename_section(&id, &name_str) {
                tracing::error!(error = %e, "Failed to rename section");
                return;
            }

            if let Err(e) = ui_ctx.change_section_icon(&id, &emoji_str) {
                tracing::error!(error = %e, "Failed to change icon");
                return;
            }

            run_ui!(weak_clone, |w| crate::bin_ui::features::back::close(&w, "create-section"));
            refresh_sections_ui(&weak_clone, &ui_ctx);
        });
    });
}

fn wire_toggle_expanded(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_section_toggle_expanded(move |section_id: SharedString| {
        let section_id_str = section_id.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<SectionId>() else { return };

            if let Ok(sections) = ui_ctx.list_sections_with_conversations() {
                if let Some(section) = sections.iter().find(|s| s.id == id) {
                    let _ = ui_ctx.toggle_section_expanded(&id, !section.is_expanded);
                    refresh_sections_ui(&weak_clone, &ui_ctx);
                }
            }
        });
    });
}

/// Move a folder one place up or down.
///
/// `SectionService::reorder_sections` was fully implemented with zero callers —
/// the back half of a drag-to-reorder nobody finished. This is its caller.
/// Deliberately not drag-and-drop: nothing in this UI has ever had a DropArea,
/// and dragging a row inside a scrolling list is the worst gesture to debut on
/// a phone. Two menu entries do the same job with no new gesture vocabulary.
fn wire_move_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_move_section(move |section_id: SharedString, delta: i32| {
        let section_id_str = section_id.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<SectionId>() else {
                tracing::error!(section_id = %section_id_str, "move section: unparseable section id");
                return;
            };
            match ui_ctx.move_section(&id, delta) {
                Ok(()) => refresh_sections_ui(&weak_clone, &ui_ctx),
                Err(e) => tracing::error!(error = %e, section_id = %section_id_str, delta, "Failed to move section"),
            }
        });
    });
}

/// Choose how one folder orders the conversations inside it.
fn wire_set_sort_mode(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_set_section_sort_mode(move |section_id: SharedString, mode: SharedString| {
        let section_id_str = section_id.to_string();
        let mode = SectionSortMode::from_str_value(mode.as_str());
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            let Ok(id) = section_id_str.parse::<SectionId>() else { return };
            match ui_ctx.set_section_sort_mode(&id, mode) {
                Ok(()) => refresh_sections_ui(&weak_clone, &ui_ctx),
                Err(e) => tracing::error!(error = %e, section_id = %section_id_str, "Failed to set section sort mode"),
            }
        });
    });
}

/// Pin or unpin a conversation. Local-only by design — no server call.
fn wire_set_conversation_pinned(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_set_conversation_pinned(move |conv_id: SharedString, pinned: bool| {
        let conv_id_str = conv_id.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            if !ui_ctx.set_conversation_pinned(&conv_id_str, pinned) {
                return;
            }
            tracing::info!(conversation_id = %conv_id_str, pinned, "conversation pin toggled (local only)");
            // Pin order is core's; every list that renders it has to be rebuilt.
            refresh_sections_ui(&weak_clone, &ui_ctx);
            refresh_conversation_lists(&weak_clone, &ui_ctx);
        });
    });
}

fn wire_move_to_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_move_to_section(move |conv_id: SharedString, kind: crate::ConversationKind, section_id: SharedString| {
        let conv_id_str = conv_id.to_string();
        let conv_type = crate::bin_ui::converters::conversation_type(kind);
        let section_id_str = section_id.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            let Ok(sid) = section_id_str.parse::<SectionId>() else {
                tracing::error!(section_id = %section_id_str, "move to folder: unparseable section id");
                return;
            };
            match ui_ctx.move_conversation_to_section(&conv_id_str, conv_type, &sid) {
                Ok(()) => {
                    refresh_sections_ui(&weak_clone, &ui_ctx);
                    refresh_conversation_lists(&weak_clone, &ui_ctx);
                }
                Err(e) => tracing::error!(
                    error = %e, conversation_id = %conv_id_str, section_id = %section_id_str,
                    "Failed to move conversation into folder"
                ),
            }
        });
    });
}

fn wire_remove_from_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.on_remove_from_section(move |conv_id: SharedString| {
        let conv_id_str = conv_id.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        tokio::spawn(async move {
            match ui_ctx.remove_conversation_from_section(&conv_id_str) {
                Ok(()) => {
                    refresh_sections_ui(&weak_clone, &ui_ctx);
                    refresh_conversation_lists(&weak_clone, &ui_ctx);
                }
                Err(e) => tracing::error!(
                    error = %e, conversation_id = %conv_id_str,
                    "Failed to remove conversation from folder"
                ),
            }
        });
    });
}

fn wire_section_conversation_selected(window: &MainWindow, _ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();

    window.on_section_conversation_selected(move |conv_id: SharedString, kind: crate::ConversationKind| {
        use crate::ConversationKind as K;
        let conv_id_str = conv_id.to_string();
        run_ui!(weak, |w| {
            match kind {
                K::Channel | K::SavedMessages => w.invoke_channel_selected(conv_id_str.into()),
                K::ServerDm | K::AnonDm => w.invoke_dm_selected(conv_id_str.into()),
                K::AnonGroup => w.invoke_private_group_selected(conv_id_str.into()),
                // Mesh conversations are never filed into folders.
                K::Mesh | K::MeshDm => {}
            }
        });
    });
}

/// Rebuild the folder models. Queries run on the caller's (runtime) thread;
/// only the model swap runs on the UI thread. The rebuild clears every row's
/// selection flag, so the open conversation is re-marked after it.
pub(crate) fn refresh_sections_ui(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    if let Ok(sections) = ui_ctx.list_sections_with_conversations() {
        let send_data = build_section_data(&sections, ui_ctx);
        let ui_ctx = ui_ctx.clone();
        run_ui!(weak, |w| {
            let models: Vec<_> = send_data.into_iter().map(|s| s.to_slint()).collect();
            w.set_custom_sections(ModelRc::new(VecModel::from(models)));
            super::refresh_section_selection(&w, &ui_ctx);
        });
    }
}

/// Rebuild the loose Channels, Messages and Private-group lists.
///
/// All three exclude anything that lives in a folder and the first two put
/// pinned rows first, so any change to folder membership or to a pin has to
/// redraw them. Private groups were left out of this rebuild, so filing one
/// into a folder left the loose row on screen next to its copy in the folder.
pub(crate) fn refresh_conversation_lists(
    weak: &slint::Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
) {
    // Sidebar-inclusion rules (Public/Private, anon-mode aware, folder-aware,
    // pinned-first) live in core.
    let filtered = ui_ctx.sidebar_channels();
    let mut dms = ui_ctx.list_dm_sidebar();
    for dm in dms.iter_mut() {
        dm.pre_decode_avatar();
    }
    let groups = super::private_group::private_group_rows(ui_ctx);
    let ui_ctx_convert = ui_ctx.clone();
    run_ui!(weak, |w| {
        let channel_list: Vec<_> = filtered.iter().map(|c| channel_to_slint(c, &ui_ctx_convert)).collect();
        w.set_channels(ModelRc::new(VecModel::from(channel_list)));
        let dm_list: Vec<_> = dms.into_iter().map(|d| d.to_slint()).collect();
        w.set_dm_conversations(ModelRc::new(VecModel::from(dm_list)));
        w.set_private_groups(ModelRc::new(VecModel::from(groups)));
        crate::bin_ui::features::sidebar::refresh_selection(&w, &ui_ctx_convert);
        // The rows just took fresh counts; the Chats tab badge counts the
        // same rows, so it is recomputed with them or it goes stale (a
        // badge of 1 over two unread rows).
        crate::bin_ui::features::auth::inbox::domain_handler::recompute_total_unread(&w, &ui_ctx_convert);
    });
}

fn build_section_data(
    sections: &[chatter_core::domain::section::CustomSection],
    ui_ctx: &Arc<ChatterClient>,
) -> Vec<SectionSendData> {
    // Channel icons are raw bytes on the Channel record (BLOB since v54), not
    // a CDN URL — fetch them once, keyed by id, for enrich to decode per row.
    let channel_icons: std::collections::HashMap<String, Vec<u8>> = ui_ctx
        .get_channels_mru()
        .into_iter()
        .filter_map(|c| c.icon.filter(|b| !b.is_empty()).map(|b| (c.id.to_string(), b)))
        .collect();

    sections
        .iter()
        .map(|section| SectionSendData {
            section_id: section.id.to_string(),
            name: section.name.clone(),
            icon_emoji: section.icon_emoji.clone(),
            is_expanded: section.is_expanded,
            unread_count: section.unread_count as i32,
            sort_mode: section.sort_mode.as_str().to_string(),
            conversations: section
                .conversations
                .iter()
                .map(|conv| enrich_conversation_send(conv, ui_ctx, &channel_icons))
                .collect(),
        })
        .collect()
}

fn enrich_conversation_send(
    conv: &chatter_core::domain::section::SectionConversation,
    ui_ctx: &Arc<ChatterClient>,
    channel_icons: &std::collections::HashMap<String, Vec<u8>>,
) -> SectionConvSendData {
    // Name / unread / pin already came back with the section — core resolved
    // them in one query. This used to be a `get_conversation_metadata` per row.
    let mut display_name = conv.display_name.clone();
    if display_name.is_empty() {
        // A channel-free DM has no unified-conversation row until its first
        // message lands; fall back to the peer profile so the row is not blank.
        if let Ok(peer_id) = conv.conversation_id.parse() {
            display_name = ui_ctx.display_name(&peer_id);
        }
    }

    let mut data = SectionConvSendData {
        conversation_id: conv.conversation_id.clone(),
        conversation_type: conv.conversation_type.clone(),
        initials: get_initials(&display_name),
        display_name,
        avatar_decoded: None,
        unread_count: conv.unread_count as i32,
        is_pinned: conv.is_pinned,
        can_leave: matches!(conv.conversation_type, ConversationType::Channel)
            && conv.conversation_id.parse().is_ok_and(|id| ui_ctx.can_leave_channel(&id)),
        can_delete: matches!(conv.conversation_type, ConversationType::Channel)
            && conv.conversation_id.parse().is_ok_and(|id| ui_ctx.is_former_channel(&id)),
        // Preview, time, avatar, presence: the loose row's, from core.
        extras: ui_ctx.section_row_extras(conv),
    };

    // Channels carry their icon as raw bytes on the Channel record, not a URL;
    // a friend DM's avatar comes with the row's extras.
    let avatar_bytes = match conv.conversation_type {
        ConversationType::Channel => channel_icons.get(&conv.conversation_id).map(Vec::as_slice),
        _ => data.extras.avatar_bytes.as_deref(),
    };
    data.avatar_decoded = avatar_bytes
        .and_then(|bytes| image::load_from_memory(bytes).ok())
        .map(|img| {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            DecodedImagePixels { width: w, height: h, rgba: rgba.into_raw() }
        });

    data
}
