//! Chat header actions: the settings (kebab) button and Invite.

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc};

use chatter_core::client::ChatterClient;
use chatter_core::state::ActiveConversation;

use crate::MainWindow;

/// Wire the chat header's settings and invite buttons.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_chat_settings_clicked(window, ui_ctx);
    wire_invite_member(window, ui_ctx);
}

fn wire_chat_settings_clicked(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_chat_settings_clicked(move || {
            let conversation = ui_ctx.active_conversation();

            if let ActiveConversation::AnonGroup(group_id) = conversation {
                // Private group settings - move DB queries to background thread
                let ui_ctx = ui_ctx.clone();
                let weak = weak_window.clone();
                let opened = ActiveConversation::AnonGroup(group_id);
                if !crate::bin_ui::features::modals::settings_open_allowed(ui_ctx.active_conversation(), opened) {
                    return;
                }
                let group_id_str = group_id.to_string();
                if let Some(w) = weak.upgrade() {
                    let already = w
                        .global::<crate::ModalHostState>()
                        .get_channel_settings_id()
                        .as_str()
                        == group_id_str
                        && w.global::<crate::ModalHostState>()
                            .get_modal_stack()
                            .iter()
                            .any(|s| s.as_str() == "channel-settings");
                    if !already {
                        crate::bin_ui::features::modals::show_channel_settings_shell(
                            &w,
                            &group_id_str,
                            "private-group",
                        );
                    }
                }

                std::thread::spawn(move || {
                    // Shared with the live refresh on membership events
                    // (`refresh_members_if_open`), so both read the same roster.
                    let Some(roster) = ui_ctx.private_group_settings_roster(group_id) else {
                        let ui_ctx = ui_ctx.clone();
                        let weak = weak.clone();
                        let id = group_id_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(w) = weak.upgrade() else { return };
                            let current_id = w
                                .global::<crate::ModalHostState>()
                                .get_channel_settings_id();
                            let on_stack = w
                                .global::<crate::ModalHostState>()
                                .get_modal_stack()
                                .iter()
                                .any(|s| s.as_str() == "channel-settings");
                            if !crate::bin_ui::features::modals::settings_refresh_allowed(
                                ui_ctx.active_conversation(),
                                opened,
                                current_id.as_str(),
                                &id,
                                on_stack,
                            ) {
                                return;
                            }
                            crate::bin_ui::features::modals::present_settings_error(
                                &w,
                                &id,
                                "private-group",
                                "Private group not found",
                            );
                        });
                        return;
                    };
                    let muted = ui_ctx.is_conversation_muted(opened).unwrap_or(false);
                    let notification_preference = ui_ctx
                        .conversation_notification_preference(opened)
                        .unwrap_or_default();
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(w) = weak.upgrade() else { return };
                        let current_id = w
                            .global::<crate::ModalHostState>()
                            .get_channel_settings_id();
                        let on_stack = w
                            .global::<crate::ModalHostState>()
                            .get_modal_stack()
                            .iter()
                            .any(|s| s.as_str() == "channel-settings");
                        if !crate::bin_ui::features::modals::settings_refresh_allowed(
                            ui_ctx.active_conversation(),
                            opened,
                            current_id.as_str(),
                            &group_id_str,
                            on_stack,
                        ) {
                            return;
                        }
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_id(group_id_str.as_str().into());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_name(roster.name.as_str().into());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_name_draft(roster.name.as_str().into());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_description("".into());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_description_draft("".into());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_ai_enabled(ui_ctx.ai_enabled_for(&group_id_str));
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_muted(muted);
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_notification_pref(
                                crate::bin_ui::features::modals::preference_label(notification_preference).into(),
                            );
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_type("private-group".into());
                        crate::bin_ui::features::modals::apply_private_group_roster(&w, roster);
                        // Clear a previous channel's ban list.
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_can_view_bans(false);
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_banned_members(ModelRc::default());
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_loading(false);
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_error("".into());
                        tracing::debug!("Opened settings for private group {}", group_id);
                    });
                });
                return;
            }

            // A DM's settings screen is the peer's profile — block, verify, safety
            // number, remove friend all live there, the same way Telegram hangs a
            // DM's settings off the contact page. The kebab used to route to
            // channel settings instead, which for a channel-free (Signal-style)
            // server DM missed in `get_channel_local` and silently did nothing:
            // the button was on screen in every DM and had never worked.
            if let ActiveConversation::ServerDm { peer_id } = conversation {
                let weak = weak_window.clone();
                let ui_ctx = ui_ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        crate::bin_ui::features::modals::open_user_profile(&w, &peer_id, Some(&ui_ctx));
                        crate::bin_ui::features::back::push(&w, "user-profile");
                    }
                });
                return;
            }

            // Regular channel settings
            let channel_id = match conversation {
                ActiveConversation::Channel(id) => id,
                _ => {
                    tracing::warn!("Cannot open channel settings: no active channel");
                    return;
                }
            };

            crate::bin_ui::features::modals::open_channel_settings(weak_window.clone(), ui_ctx.clone(), channel_id);
        });
}

fn wire_invite_member(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_invite_member(move || {
            let conversation = ui_ctx.active_conversation();
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx.clone();

            match conversation {
                ActiveConversation::AnonGroup(group_id) => {
                    crate::bin_ui::features::modals::open_private_group_invite_modal(
                        weak, ui_ctx, group_id,
                    );
                }
                ActiveConversation::Channel(channel_id) => {
                    crate::bin_ui::features::modals::open_invite_member_modal(
                        weak, ui_ctx, channel_id,
                    );
                }
                _ => {
                    tracing::debug!("Invite not available for this conversation type");
                }
            }
        });
}
