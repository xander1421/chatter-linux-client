//! Custom section callbacks for the sidebar.
//!
//! Wires section management (create, delete, rename, toggle, move, reorder,
//! sort mode, pin) and loads section data on startup.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::SectionId;
use chatter_core::domain::section::SectionSortMode;

use chatter_core::ui::sidebar_data::ChatRowSendData;

use crate::bin_ui::converters::ChatRowExt;
use crate::{ChatRowData, CustomSectionData, MainWindow, run_ui};

/// Send-safe intermediate for one section and the rows it owns.
struct SectionSendData {
    section_id: String,
    name: String,
    icon_emoji: String,
    is_expanded: bool,
    unread_count: i32,
    sort_mode: String,
    conversations: Vec<ChatRowSendData>,
}

impl SectionSendData {
    fn to_slint(self) -> CustomSectionData {
        let rows: Vec<ChatRowData> = self
            .conversations
            .into_iter()
            .map(ChatRowExt::to_slint)
            .collect();
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
            apply_sections(window, models);
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

    window.global::<crate::ModalHostState>().on_create_section(move |name: SharedString, emoji: SharedString| {
        let name_str = name.to_string();
        let emoji_str = emoji.to_string();
        let ui_ctx = ui_ctx.clone();
        let weak_clone = weak.clone();

        run_ui!(weak, |w| w.global::<crate::ModalHostState>().set_create_section_loading(true));

        tokio::spawn(async move {
            match ui_ctx.create_section(&name_str, &emoji_str) {
                Ok(section) => {
                    tracing::info!(section_id = %section.id, name = %name_str, "Created custom section");
                    run_ui!(weak_clone, |w| {
                        w.global::<crate::ModalHostState>().set_create_section_loading(false);
                        crate::bin_ui::features::back::close(&w, "create-section");
                    });
                    refresh_sections_ui(&weak_clone, &ui_ctx);
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to create section");
                    run_ui!(weak_clone, |w| w.global::<crate::ModalHostState>().set_create_section_loading(false));
                }
            }
        });
    });
}

fn wire_delete_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.global::<crate::SidebarPane>().on_delete_section_clicked(move |section_id: SharedString| {
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
                    super::rebuild_chat_list(&weak_clone, &ui_ctx);
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

        window
            .global::<crate::SidebarPane>()
            .on_rename_section_clicked(move |section_id: SharedString| {
                let section_id_str = section_id.to_string();
                if let Ok(sections) = ui_ctx.list_sections_with_conversations() {
                    if let Some(section) =
                        sections.iter().find(|s| s.id.to_string() == section_id_str)
                    {
                        let name: SharedString = section.name.clone().into();
                        let emoji: SharedString = section.icon_emoji.clone().into();
                        run_ui!(weak, |w| {
                            w.global::<crate::ModalHostState>()
                                .set_create_section_name(name);
                            w.global::<crate::ModalHostState>()
                                .set_create_section_emoji(emoji);
                        });
                    }
                }
            });
    }

    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.global::<crate::ModalHostState>().on_rename_section(
        move |section_id: SharedString, name: SharedString, emoji: SharedString| {
            let section_id_str = section_id.to_string();
            let name_str = name.to_string();
            let emoji_str = emoji.to_string();
            let ui_ctx = ui_ctx.clone();
            let weak_clone = weak.clone();

            tokio::spawn(async move {
                let Ok(id) = section_id_str.parse::<SectionId>() else {
                    return;
                };

                if let Err(e) = ui_ctx.rename_section(&id, &name_str) {
                    tracing::error!(error = %e, "Failed to rename section");
                    return;
                }

                if let Err(e) = ui_ctx.change_section_icon(&id, &emoji_str) {
                    tracing::error!(error = %e, "Failed to change icon");
                    return;
                }

                run_ui!(weak_clone, |w| crate::bin_ui::features::back::close(
                    &w,
                    "create-section"
                ));
                refresh_sections_ui(&weak_clone, &ui_ctx);
            });
        },
    );
}

fn wire_toggle_expanded(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window
        .global::<crate::SidebarPane>()
        .on_section_toggle_expanded(move |section_id: SharedString| {
            let section_id_str = section_id.to_string();
            let ui_ctx = ui_ctx.clone();
            let weak_clone = weak.clone();

            tokio::spawn(async move {
                let Ok(id) = section_id_str.parse::<SectionId>() else {
                    return;
                };

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

    window.global::<crate::SidebarPane>().on_move_section(move |section_id: SharedString, delta: i32| {
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

    window.global::<crate::SidebarPane>().on_set_section_sort_mode(move |section_id: SharedString, mode: SharedString| {
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

    window.global::<crate::SidebarPane>().on_set_conversation_pinned(move |conv_id: SharedString, pinned: bool| {
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
            super::rebuild_chat_list(&weak_clone, &ui_ctx);
        });
    });
}

fn wire_move_to_section(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let ui_ctx = ui_ctx.clone();
    let weak = window.as_weak();

    window.global::<crate::SidebarPane>().on_move_to_section(move |conv_id: SharedString, kind: crate::ConversationKind, section_id: SharedString| {
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
                    super::rebuild_chat_list(&weak_clone, &ui_ctx);
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

    window
        .global::<crate::SidebarPane>()
        .on_remove_from_section(move |conv_id: SharedString| {
            let conv_id_str = conv_id.to_string();
            let ui_ctx = ui_ctx.clone();
            let weak_clone = weak.clone();

            tokio::spawn(async move {
                match ui_ctx.remove_conversation_from_section(&conv_id_str) {
                    Ok(()) => {
                        refresh_sections_ui(&weak_clone, &ui_ctx);
                        super::rebuild_chat_list(&weak_clone, &ui_ctx);
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

    window
        .global::<crate::SidebarPane>()
        .on_section_conversation_selected(
            move |conv_id: SharedString, kind: crate::ConversationKind| {
                use crate::ConversationKind as K;
                let conv_id_str = conv_id.to_string();
                run_ui!(weak, |w| {
                    match kind {
                        K::Channel | K::SavedMessages => w
                            .global::<crate::SidebarPane>()
                            .invoke_channel_selected(conv_id_str.into()),
                        K::ServerDm | K::AnonDm => w
                            .global::<crate::SidebarPane>()
                            .invoke_dm_selected(conv_id_str.into()),
                        K::AnonGroup => w
                            .global::<crate::SidebarPane>()
                            .invoke_private_group_selected(conv_id_str.into()),
                        // Mesh conversations are never filed into folders.
                        K::Mesh | K::MeshDm => {}
                    }
                });
            },
        );
}

/// Rebuild the folder models. Queries run on the caller's (runtime) thread;
/// only the model swap runs on the UI thread. The rebuild clears every row's
/// selection flag, so the open conversation is re-marked after it.
pub(crate) fn refresh_sections_ui(weak: &slint::Weak<MainWindow>, ui_ctx: &Arc<ChatterClient>) {
    if let Ok(sections) = ui_ctx.list_sections_with_conversations() {
        let send_data = build_section_data(&sections, ui_ctx);
        let ui_ctx = ui_ctx.clone();
        run_ui!(weak, |w| {
            let models: Vec<_> = send_data.into_iter().map(|s| s.to_slint()).collect();
            apply_sections(&w, models);
            super::refresh_section_selection(&w, &ui_ctx);
        });
    }
}

/// Retain each section and its conversation model across badge/selection
/// refreshes. Only a real section insertion or removal creates delegates.
fn apply_sections(window: &MainWindow, sections: Vec<CustomSectionData>) {
    let model = window.global::<crate::SidebarPane>().get_custom_sections();
    let Some(vec) = model.as_any().downcast_ref::<VecModel<CustomSectionData>>() else {
        window
            .global::<crate::SidebarPane>()
            .set_custom_sections(ModelRc::new(VecModel::from(sections)));
        return;
    };
    let target_len = sections.len();
    let mut keys = (0..vec.row_count()).filter_map(|i| vec.row_data(i)
        .map(|section| section.section_id.to_string())).collect::<Vec<_>>();
    let mut positions = keys.iter().enumerate()
        .map(|(i, key)| (key.clone(), i)).collect::<std::collections::HashMap<_, _>>();
    for (target, mut section) in sections.into_iter().enumerate() {
        let key = section.section_id.to_string();
        let existing = positions.get(&key).copied().filter(|&i| i >= target);
        if let Some(from) = existing {
            let old = vec.row_data(from).expect("located section");
            let rows = old.conversations.clone();
            if let Some(row_vec) = rows.as_any().downcast_ref::<VecModel<ChatRowData>>() {
                            old.conversation_id == row.conversation_id
                let new_rows: Vec<_> = section.conversations.iter().collect();
                super::reconcile_chat_rows(row_vec, new_rows, false);
                section.conversations = rows;
            }
            if from == target {
                if old.name != section.name || old.icon_emoji != section.icon_emoji
                    || old.is_expanded != section.is_expanded
                    || old.unread_count != section.unread_count
                    || old.sort_mode != section.sort_mode
                {
                    vec.set_row_data(target, section);
                }
            } else {
                vec.remove(from);
                vec.insert(target, section);
                keys.remove(from);
                keys.insert(target, key.clone());
            }
        } else {
            vec.insert(target, section);
            keys.insert(target, key.clone());
        }
        for i in target..keys.len() {
            positions.insert(keys[i].clone(), i);
        }
    }
    while vec.row_count() > target_len {
        vec.remove(vec.row_count() - 1);
    }
}

fn build_section_data(
    sections: &[chatter_core::domain::section::CustomSection],
    ui_ctx: &Arc<ChatterClient>,
) -> Vec<SectionSendData> {
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
                .map(|conv| {
                    // Core shapes the row; only the avatar decode happens here,
                    // off the UI thread.
                    ui_ctx.section_row(conv)
                })
                .collect(),
        })
        .collect()
}
