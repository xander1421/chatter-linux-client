//! Header callbacks.
//!
//! Handles channel header actions (search, pinned, settings, load more).

use std::sync::Arc;

use slint::{ComponentHandle, Model, ModelRc};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::MessageId;
use chatter_core::state::ActiveConversation;

use crate::MainWindow;
use crate::bin_ui::converters::messages_to_display_batch;

thread_local! {
    static PENDING_VISIBLE_THUMBNAILS: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

thread_local! {
    static PENDING_JUMP: std::cell::RefCell<Option<(ActiveConversation, String)>> = const { std::cell::RefCell::new(None) };
}

/// Queue navigation until the source conversation's model has been installed.
pub(crate) fn jump_to_conversation_message(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    conversation: ActiveConversation,
    message_id: &str,
) {
    if ui_ctx.active_conversation() == conversation
        && !w.global::<crate::ChatPane>().get_loading_messages()
    {
        jump_to_message(w, ui_ctx, message_id);
        return;
    }
    PENDING_JUMP
        .with(|pending| *pending.borrow_mut() = Some((conversation, message_id.to_owned())));
    super::sidebar::select_conversation(w, conversation);
}

/// Call after installing a conversation's messages and clearing loading.
pub(crate) fn finish_pending_jump(w: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    if w.global::<crate::ChatPane>().get_loading_messages() {
        return;
    }
    let target = PENDING_JUMP.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending
            .as_ref()
            .is_some_and(|(conversation, _)| *conversation == ui_ctx.active_conversation())
        {
            pending.take()
        } else {
            None
        }
    });
    if let Some((_, message_id)) = target {
        jump_to_message(w, ui_ctx, &message_id);
    }
}

/// A manual switch away cancels pending navigation rather than resurrecting it.
pub(crate) fn cancel_pending_jump_for(conversation: ActiveConversation) {
    PENDING_JUMP.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending
            .as_ref()
            .is_some_and(|(target, _)| *target != conversation)
        {
            *pending = None;
        }
    });
}

/// Wire header callbacks.
pub fn wire(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    wire_pinned_navigation(window, ui_ctx);
    wire_chat_settings_clicked(window, ui_ctx);
    wire_invite_member(window, ui_ctx);
    wire_load_more(window, ui_ctx);
    wire_visible_media(window, ui_ctx);
}

fn wire_visible_media(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak = window.as_weak();
    let client = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_visible_media_rows(move |first, last| {
            if let Some(w) = weak.upgrade() {
                refresh_visible_media(&w, &client, first, last);
            }
        });
}

/// Resolve previews only for placeholder images in the viewport. Older pages
/// can have a downloaded original but no sidecar thumbnail in their metadata.
/// Opening the viewer derives and stores a thumbnail, so this also runs when
/// the viewer closes to refresh the row without reopening the conversation.
pub(crate) fn refresh_visible_media(
    window: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    first: i32,
    last: i32,
) {
    if window.global::<crate::ChatPane>().get_loading_messages() || last < first || last < 0 {
        return;
    }
    let conversation = ui_ctx.active_conversation();
    if conversation.channel_id().is_none() {
        return;
    }
    let owner = super::history_owner::HistoryOwner::capture(ui_ctx, conversation);
    let model = window.global::<crate::ChatPane>().get_messages();
    let mut ids = Vec::new();
    for index in first.max(0) as usize..=(last as usize).min(model.row_count().saturating_sub(1)) {
        let Some(row) = model.row_data(index) else {
            continue;
        };
        for attachment in row.attachments.iter() {
            if attachment.is_image && !attachment.has_thumbnail {
                let id = attachment.id.to_string();
                let inserted = PENDING_VISIBLE_THUMBNAILS
                    .with(|pending| pending.borrow_mut().insert(id.clone()));
                if inserted {
                    ids.push(id);
                }
            }
        }
    }

    for id in ids {
        let weak = window.as_weak();
        let client = ui_ctx.clone();
        let owner = owner.clone();
        crate::rt_handle().spawn(async move {
            let parsed = id.parse::<chatter_core::domain::ids::AttachmentId>();
            let pixels = if let Ok(attachment_id) = parsed {
                let metadata = client.get_attachment_metadata(&attachment_id);
                let cached = metadata
                    .as_ref()
                    .and_then(|attachment| attachment.metadata.thumbnail.clone())
                    .filter(|bytes| !bytes.is_empty());
                let local = metadata.is_some_and(|attachment| {
                    attachment.content_location
                        == chatter_core::domain::attachment::ContentLocation::OnFilesystem
                });
                let bytes = if cached.is_some() {
                    cached
                } else if local {
                    // This derives a durable thumbnail from the authenticated
                    // downloaded original when an old sidecar is absent.
                    let work_client = client.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = work_client.decrypt_and_get_image_info(&attachment_id);
                        work_client
                            .get_attachment_metadata(&attachment_id)
                            .and_then(|attachment| attachment.metadata.thumbnail)
                    })
                    .await
                    .ok()
                    .flatten()
                } else {
                    client.ensure_attachment_thumbnail(&attachment_id).await
                };
                if let Some(bytes) = bytes {
                    let decode_id = id.clone();
                    tokio::task::spawn_blocking(move || {
                        crate::bin_ui::decode_cache::thumbnail(&decode_id, bytes.len(), || {
                            image::load_from_memory(&bytes).ok()
                        })
                    })
                    .await
                    .ok()
                    .flatten()
                } else {
                    None
                }
            } else {
                None
            };
            let _ = slint::invoke_from_event_loop(move || {
                PENDING_VISIBLE_THUMBNAILS.with(|pending| {
                    pending.borrow_mut().remove(&id);
                });
                let Some(w) = weak.upgrade() else { return };
                if !owner.is_current() {
                    return;
                }
                let Some(pixels) = pixels else { return };
                let model = w.global::<crate::ChatPane>().get_messages();
                for row_index in 0..model.row_count() {
                    let Some(row) = model.row_data(row_index) else {
                        continue;
                    };
                    for attachment_index in 0..row.attachments.row_count() {
                        let Some(mut attachment) = row.attachments.row_data(attachment_index)
                        else {
                            continue;
                        };
                        if attachment.id.as_str() == id && !attachment.has_thumbnail {
                            attachment.thumbnail = slint::Image::from_rgba8(pixels.clone());
                            attachment.has_thumbnail = true;
                            if let Some(attachments) = row
                                .attachments
                                .as_any()
                                .downcast_ref::<slint::VecModel<crate::MessageAttachmentData>>()
                            {
                                attachments.set_row_data(attachment_index, attachment);
                            }
                        }
                    }
                }
            });
        });
    }
}

fn wire_pinned_navigation(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    // Previous pinned message
    let weak_window = window.as_weak();
    let ui_ctx_prev = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_pinned_prev_clicked(move || {
            ui_ctx_prev.pinned_navigate_prev();
            update_pinned_ui_from_loaded_messages(
                weak_window.clone(),
                ui_ctx_prev.clone(),
                ui_ctx_prev.active_conversation(),
            );
        });

    // Next pinned message
    let weak_window = window.as_weak();
    let ui_ctx_next = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_pinned_next_clicked(move || {
            ui_ctx_next.pinned_navigate_next();
            update_pinned_ui_from_loaded_messages(
                weak_window.clone(),
                ui_ctx_next.clone(),
                ui_ctx_next.active_conversation(),
            );
        });

    // Dismiss pinned bar
    let ui_ctx_dismiss = ui_ctx.clone();
    let weak_window = window.as_weak();

    window
        .global::<crate::ChatPane>()
        .on_pinned_dismiss_clicked(move || {
            ui_ctx_dismiss.pinned_dismiss();
            if let Some(w) = weak_window.upgrade() {
                w.global::<crate::ChatPane>().set_has_pinned_messages(false);
            }
            tracing::debug!("Pinned bar dismissed");
        });

    // Header menu opens the complete core-ordered pin list, not just loaded rows.
    let weak_window = window.as_weak();
    let ui_ctx_show = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_pinned_show_clicked(move || {
            let weak = weak_window.clone();
            let ui_ctx = ui_ctx_show.clone();
            let conversation = ui_ctx.active_conversation();
            let revision = ui_ctx.auth_snapshot().session_revision;
            tokio::spawn(async move {
                let mut items = ui_ctx.pinned_display_items(conversation).await;
                for item in &mut items {
                    item.preview = pinned_plaintext_preview(
                        &ui_ctx,
                        conversation,
                        &item.message_id,
                        &item.preview,
                    );
                }
                #[cfg(target_os = "android")]
                let items = {
                    let client = ui_ctx.clone();
                    tokio::task::spawn_blocking(move || {
                        for item in items.iter_mut().filter(|i| i.thumbnail.is_empty()).take(4) {
                            item.thumbnail =
                                pinned_video_thumbnail(&client, conversation, &item.message_id);
                        }
                        items
                    })
                    .await
                    .unwrap_or_default()
                };
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(w) = weak.upgrade() else {
                        return;
                    };
                    if ui_ctx.auth_snapshot().session_revision != revision {
                        return;
                    }
                    if ui_ctx.active_conversation() != conversation {
                        return;
                    }
                    let rows: Vec<crate::PinnedItem> = items
                        .into_iter()
                        .map(|item| crate::PinnedItem {
                            message_id: item.message_id.to_string().into(),
                            preview: item.preview.into(),
                            thumbnail: crate::bin_ui::converters::avatar_bytes_to_image(
                                &item.thumbnail,
                            )
                            .unwrap_or_default(),
                        })
                        .collect();
                    w.global::<crate::ChatPane>()
                        .set_pinned_items(ModelRc::new(slint::VecModel::from(rows)));
                    super::back::push(&w, "pinned-messages");
                });
            });
        });
    let weak_window = window.as_weak();
    let ui_ctx_selected = ui_ctx.clone();
    window
        .global::<crate::ChatPane>()
        .on_pinned_item_selected(move |message_id| {
            let Some(w) = weak_window.upgrade() else {
                return;
            };
            super::back::close(&w, "pinned-messages");
            jump_to_message(&w, &ui_ctx_selected, &message_id);
        });

    // Go to pinned message (scroll to it in chat)
    let weak_window = window.as_weak();
    let ui_ctx_goto = ui_ctx.clone();

    window
        .global::<crate::ChatPane>()
        .on_pinned_go_to_message(move || {
            if let (Some(message_id), Some(w)) = (
                ui_ctx_goto.pinned_current_message_id(),
                weak_window.upgrade(),
            ) {
                jump_to_message(&w, &ui_ctx_goto, &message_id.to_string());
            }
        });
}

/// Resolve the full text before stripping/truncating; core previews can already
/// end halfway through a formatting marker. Non-text captions remain literal.
fn pinned_plaintext_preview(
    ui_ctx: &ChatterClient,
    conversation: ActiveConversation,
    message_id: &MessageId,
    caption: &str,
) -> String {
    let message = match conversation {
        ActiveConversation::AnonGroup(_) => ui_ctx.get_anon_group_message(
            &chatter_core::domain::anon_group::AnonGroupMessageId::from_string(
                message_id.to_string(),
            ),
        ),
        _ => ui_ctx.get_message_local(message_id),
    };
    crate::bin_ui::converters::pin_preview::preview_or_caption(
        message.as_ref().and_then(|message| message.as_text()),
        caption,
    )
}

/// Missing old video pins can derive a local frame before history has been loaded.
#[cfg(target_os = "android")]
fn pinned_video_thumbnail(
    client: &ChatterClient,
    conversation: ActiveConversation,
    id: &MessageId,
) -> Vec<u8> {
    let message = match conversation {
        ActiveConversation::AnonGroup(_) => client.get_anon_group_message(
            &chatter_core::domain::anon_group::AnonGroupMessageId::from_string(id.to_string()),
        ),
        _ => client.get_message_local(id),
    };
    let Some(message) = message else {
        return Vec::new();
    };
    for id in message.attachments.iter().take(16) {
        let Some(attachment) = client.get_attachment_metadata(id) else {
            continue;
        };
        if !attachment.metadata.is_video() {
            continue;
        }
        if let Some(bytes) = attachment.metadata.thumbnail.filter(|b| !b.is_empty()) {
            return bytes;
        }
        return client
            .decrypt_and_extract_video_thumbnail(id)
            .map(|result| result.image_bytes)
            .unwrap_or_default();
    }
    Vec::new()
}

pub(crate) fn refresh_pinned_ui(w: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    update_pinned_ui_from_loaded_messages(
        w.as_weak(),
        ui_ctx.clone(),
        ui_ctx.active_conversation(),
    );
}

/// Update pinned UI by reading the preview from loaded UI messages first,
/// falling back to PinService (regular channels) or AnonGroupService (private groups).
fn update_pinned_ui_from_loaded_messages(
    weak_window: slint::Weak<MainWindow>,
    ui_ctx: Arc<ChatterClient>,
    conversation: ActiveConversation,
) {
    let state = ui_ctx.chat_snapshot();
    let current_index = state.pinned.current_index as i32;
    let total_count = state.pinned.pinned_message_ids.len() as i32;
    let current_msg_id = ui_ctx.pinned_current_message_id();

    // First try to get preview from currently loaded UI messages (works for all types)
    let msg_id_str = current_msg_id.map(|id| id.to_string()).unwrap_or_default();
    let ui_preview = weak_window.upgrade().and_then(|w| {
        let messages = w.global::<crate::ChatPane>().get_messages();
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.message_id.as_str() == msg_id_str {
                    let preview = crate::bin_ui::features::auth::inbox::utils::generate_preview_from_ui_message(&msg);
                    if !preview.is_empty() {
                        let thumbnail = msg.attachments.iter()
                            .find(|a| (a.is_image || a.is_video) && a.has_thumbnail)
                            .map(|a| a.thumbnail).unwrap_or_default();
                        let missing_video = msg.attachments.iter().any(|a| a.is_video && !a.has_thumbnail);
                        return Some((preview, thumbnail, missing_video));
                    }
                }
            }
        }
        None
    });

    if let Some((preview, thumbnail, missing_video)) = ui_preview {
        let derive_video =
            cfg!(target_os = "android") && missing_video && thumbnail.size().width == 0;
        // Already have preview from UI, update directly (already on UI thread from callback)
        if let Some(w) = weak_window.upgrade() {
            w.global::<crate::ChatPane>()
                .set_pinned_current_index(current_index + 1);
            w.global::<crate::ChatPane>()
                .set_pinned_total_count(total_count);
            w.global::<crate::ChatPane>()
                .set_pinned_message_preview(preview.into());
            w.global::<crate::ChatPane>()
                .set_pinned_thumbnail(thumbnail);
            w.global::<crate::ChatPane>()
                .set_has_pinned_messages(total_count > 0);
        }
        if !derive_video {
            return;
        }
    }

    // Fall back to DB lookup in background thread
    let revision = ui_ctx.auth_snapshot().session_revision;
    std::thread::spawn(move || {
        let preview = match current_msg_id {
            Some(msg_id) => pinned_plaintext_preview(
                &ui_ctx,
                conversation,
                &msg_id,
                &ui_ctx.pinned_message_preview(&conversation, &msg_id),
            ),
            None => String::new(),
        };

        let thumbnail = current_msg_id
            .and_then(|id| ui_ctx.get_pinned_message(&id))
            .map(|pin| pin.thumbnail)
            .unwrap_or_default();
        #[cfg(target_os = "android")]
        let thumbnail = if thumbnail.is_empty() {
            current_msg_id
                .map(|id| pinned_video_thumbnail(&ui_ctx, conversation, &id))
                .unwrap_or_default()
        } else {
            thumbnail
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak_window.upgrade() {
                if ui_ctx.active_conversation() != conversation
                    || ui_ctx.pinned_current_message_id() != current_msg_id
                    || ui_ctx.auth_snapshot().session_revision != revision
                {
                    return;
                }
                w.global::<crate::ChatPane>().set_pinned_thumbnail(
                    crate::bin_ui::converters::avatar_bytes_to_image(&thumbnail)
                        .unwrap_or_default(),
                );
                w.global::<crate::ChatPane>()
                    .set_pinned_current_index(current_index + 1);
                w.global::<crate::ChatPane>()
                    .set_pinned_total_count(total_count);
                w.global::<crate::ChatPane>()
                    .set_pinned_message_preview(preview.into());
                w.global::<crate::ChatPane>()
                    .set_has_pinned_messages(total_count > 0);
            }
        });
    });
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
                if !super::modals::settings_open_allowed(ui_ctx.active_conversation(), opened) {
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
                        super::modals::show_channel_settings_shell(
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
                            if !super::modals::settings_refresh_allowed(
                                ui_ctx.active_conversation(),
                                opened,
                                current_id.as_str(),
                                &id,
                                on_stack,
                            ) {
                                return;
                            }
                            super::modals::present_settings_error(
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
                        if !super::modals::settings_refresh_allowed(
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
                                super::modals::preference_label(notification_preference).into(),
                            );
                        w.global::<crate::ModalHostState>()
                            .set_channel_settings_type("private-group".into());
                        super::modals::apply_private_group_roster(&w, roster);
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
                        super::modals::open_user_profile(&w, &peer_id, Some(&ui_ctx));
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

            super::modals::open_channel_settings(weak_window.clone(), ui_ctx.clone(), channel_id);
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

fn wire_load_more(window: &MainWindow, ui_ctx: &Arc<ChatterClient>) {
    let weak_window = window.as_weak();
    let ui_ctx = ui_ctx.clone();

    window.global::<crate::ChatPane>().on_load_more(move || {
        if let Some(w) = weak_window.upgrade() {
            let oldest = w
                .global::<crate::ChatPane>()
                .get_messages()
                .row_data(0)
                .and_then(|m| m.message_id.parse::<MessageId>().ok());
            if let Some(oldest) = oldest {
                load_history(&w, &ui_ctx, HistoryLoad::Older(oldest));
            }
        }
    });
}

/// Row of the open list holding `message_id`.
fn row_of(w: &MainWindow, message_id: &str) -> Option<usize> {
    let messages = w.global::<crate::ChatPane>().get_messages();
    (0..messages.row_count()).find(|&i| {
        messages
            .row_data(i)
            .is_some_and(|m| m.message_id.as_str() == message_id)
    })
}

/// Scroll the open conversation to `message_id` and flash it: a quote's
/// parent, a search hit, the pinned message. One that is older than what is
/// loaded has the history back to it loaded first. UI thread.
pub(crate) fn jump_to_message(w: &MainWindow, ui_ctx: &Arc<ChatterClient>, message_id: &str) {
    let Some(row) = row_of(w, message_id) else {
        match message_id.parse() {
            Ok(id) => load_history(w, ui_ctx, HistoryLoad::Reaching(id)),
            Err(_) => {
                tracing::debug!(message_id, "Jump target is not a stored message");
                report_jump_unavailable(w);
            }
        }
        return;
    };
    w.global::<crate::ChatPane>()
        .set_scroll_to_message_index(row as i32);
    w.global::<crate::ChatPane>()
        .set_highlighted_message_id(message_id.into());

    // A brief pulse, not a permanent highlight. The index goes back with it:
    // the list acts on it changing, so left in place a second jump to the
    // same row would do nothing.
    let weak = w.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(1500), move || {
        if let Some(w) = weak.upgrade() {
            w.global::<crate::ChatPane>()
                .set_highlighted_message_id(slint::SharedString::new());
            w.global::<crate::ChatPane>()
                .set_scroll_to_message_index(-1);
        }
    });
}

/// A jump whose target is not in this device's history (deleted, expired, a
/// thread reply, never synced here) says so instead of doing nothing. Local
/// history only: there is no server fetch behind a jump. UI thread.
fn report_jump_unavailable(w: &MainWindow) {
    w.set_toast_message("That message isn't available on this device.".into());
    w.set_toast_variant("error".into());
    w.set_toast_visible(true);
}

/// `report_jump_unavailable` from the history thread, for the request that
/// is still current.
fn report_jump_unavailable_later(
    weak: slint::Weak<MainWindow>,
    owner: super::history_owner::HistoryOwner,
) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            if owner.is_current() {
                report_jump_unavailable(&w);
            }
        }
    });
}

/// Older history requested by the list, or a jump target that is not loaded.
enum HistoryLoad {
    /// One page older than the oldest loaded row, inserted above it.
    Older(MessageId),
    /// The newest rows grown back to this message, installed in place of the
    /// list, then jumped to.
    Reaching(MessageId),
}

/// Load history into the open list. The oldest loaded row is the paging
/// cursor — the list itself, not a copy of where it was last time. UI thread.
///
/// A jump is not a prepend. Inserting rows in the same turn as the seek
/// races the list's own bookkeeping (message_list.slint `settle`: a pass
/// that sees `grown != 0` drops `jump-row`, then restores the old top row),
/// and a jump to a search hit older than the open page stayed at the bottom
/// (device ui135, desktop Foxtrot, "p135-007"). It installs the page core
/// grew back to the target as a fresh list instead, the way a conversation
/// opens, and jumps once that list exists.
fn load_history(w: &MainWindow, ui_ctx: &Arc<ChatterClient>, load: HistoryLoad) {
    // Bind this request before spawning; a later manual switch must never
    // reinterpret the target as belonging to the newly opened conversation.
    let conversation = ui_ctx.active_conversation();
    let Some(channel_id) = conversation.channel_id() else {
        return;
    };
    let Some(conversation_type) = ui_ctx.active_conversation_type() else {
        return;
    };
    let jump = matches!(load, HistoryLoad::Reaching(_));
    let Some(owner) = super::history_owner::HistoryOwner::admit_older(ui_ctx, conversation) else {
        if jump {
            w.set_toast_message("History is still loading. Try again in a moment.".into());
            w.set_toast_variant("info".into());
            w.set_toast_visible(true);
        }
        return;
    };
    // A fresh page is on its way: the previous failure's Retry chip goes.
    w.global::<crate::ChatPane>().set_history_load_failed(false);
    let weak = w.as_weak();
    let ui_ctx = ui_ctx.clone();

    std::thread::spawn(move || {
        if !owner.is_current() {
            ui_ctx.set_message_loading(channel_id, false);
            return;
        }
        let messages = match &load {
            HistoryLoad::Older(before) => {
                ui_ctx.get_older_messages_local_asc(&channel_id, &conversation_type, before)
            }
            HistoryLoad::Reaching(target) => {
                ui_ctx.get_messages_local_reaching(&channel_id, &conversation_type, target)
            }
        };
        ui_ctx.set_message_loading(channel_id, false);
        let messages = match messages {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::error!("Failed to load more messages: {}", e);
                if jump {
                    report_jump_unavailable_later(weak, owner);
                } else {
                    // Ordinary paging: the list keeps its rows and anchor and
                    // shows a Retry chip (message_list.slint `load-failed`).
                    // Only for the conversation that asked.
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            if owner.is_current() {
                                w.global::<crate::ChatPane>().set_history_load_failed(true);
                            }
                        }
                    });
                }
                return;
            }
        };
        if messages.is_empty() {
            tracing::debug!("No older messages for channel: {}", channel_id);
            if jump {
                report_jump_unavailable_later(weak, owner);
            }
            return;
        }

        let messages = chatter_core::services::disappearing::visible_history_messages(messages);
        if !owner.is_current() {
            return;
        }
        // Convert older messages to display data
        let mut display_messages = messages_to_display_batch(
            &messages,
            |ms| ui_ctx.message_sender_display_names(ms),
            |m| ui_ctx.message_sender_avatar(m),
            crate::bin_ui::converters::attachment_display_resolver(&ui_ctx),
            |m| ui_ctx.reply_author_display_name(m),
            |m| ui_ctx.reply_parent_missing_for(m),
            |m| ui_ctx.reply_parent_media(m),
            |m| ui_ctx.mention_members(&m.conversation_type, &m.conversation_id),
        );
        crate::bin_ui::converters::apply_reply_counts(
            &mut display_messages,
            &ui_ctx.thread_reply_counts(&channel_id, &conversation_type),
        );

        // Get current user ID for is_own calculation
        let current_user_id = ui_ctx
            .session_current_user_id()
            .map(|id| id.to_string())
            .unwrap_or_default();

        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            // The bytes remain private to their original session and history request.
            if !owner.is_current() {
                return;
            }
            // Convert display data to Slint types (must be done on UI thread)
            let slint_messages: Vec<_> = display_messages
                .iter()
                .map(|d| d.to_slint_with_user(&current_user_id))
                .collect();

            let HistoryLoad::Reaching(target) = load else {
                let current_model = w.global::<crate::ChatPane>().get_messages();
                let current_ids = current_model.iter().map(|msg| msg.message_id.to_string());

                // Prepend older messages (older go at top), skipping
                // duplicates. Inserted in place: rows added above the
                // first visible one shift the list's index without
                // moving what is on screen, so history loads without
                // a jump.
                let older = ui_ctx.unseen_history_rows(
                    slint_messages,
                    current_ids,
                    |msg: &crate::MessageItemData| msg.message_id.as_str(),
                );
                tracing::debug!("Loaded {} older messages for channel", older.len());
                crate::bin_ui::features::chat::edit_messages(&w, |model| {
                    for (i, msg) in older.into_iter().enumerate() {
                        model.insert(i, msg);
                    }
                });
                return;
            };
            install_and_jump(&w, &ui_ctx, owner, slint_messages, target.to_string());
        });
    });
}

/// Replace the open list with `rows` and jump to `target` in it.
///
/// `loading-messages` is raised for the swap so the list (which lives under
/// `!loading`, message_list.slint) is torn down and built fresh on the new
/// model — no previous row count for the jump to be measured against. It is
/// lowered on a later event-loop turn, not in this one: a property set and
/// reset in the same turn is never observed, and the old list would survive.
/// The jump then lands on a fresh list, as it does for a search hit that
/// opens its conversation (`finish_pending_jump`). UI thread.
fn install_and_jump(
    w: &MainWindow,
    ui_ctx: &Arc<ChatterClient>,
    owner: super::history_owner::HistoryOwner,
    rows: Vec<crate::MessageItemData>,
    target: String,
) {
    let pane = w.global::<crate::ChatPane>();
    pane.set_loading_messages(true);
    pane.set_messages(ModelRc::new(slint::VecModel::from(rows)));
    let weak = w.as_weak();
    let ui_ctx = ui_ctx.clone();
    slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
        let Some(w) = weak.upgrade() else { return };
        if !owner.is_current() {
            return;
        }
        w.global::<crate::ChatPane>().set_loading_messages(false);
        if row_of(&w, &target).is_some() {
            jump_to_message(&w, &ui_ctx, &target);
        } else {
            report_jump_unavailable(&w);
        }
    });
}
