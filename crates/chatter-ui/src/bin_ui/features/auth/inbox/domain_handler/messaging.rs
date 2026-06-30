//! Message receive/send/fail, channel member, pin/unpin, and unread count handlers.

use std::sync::Arc;

use slint::{Model, ModelRc, VecModel, Weak};

use chatter_core::client::ChatterClient;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::message::{ConversationType, Message};
use chatter_core::domain::anon_dm::AnonDmId;
use chatter_core::domain::anon_group::AnonGroupId;
use crate::MainWindow;
use crate::MessageItemData;

/// Handle an incoming message (DM, channel, or anonymous).
///
/// Resolves sender name + attachments, inserts into the Slint message model
/// if the conversation is active, updates sidebar preview, and triggers
/// attachment auto-download.
pub(super) fn handle_message_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    message: Message,
) {
    let conversation_id = message.conversation_id;
    let sender_id = message.sender_id;
    let timestamp = message.sent_at;
    let conv_type = message.conversation_type.clone();

    // Resolve sender display name (UserService handles profile / anon DM peer
    // / anon group member fallback — see `UserService::get_display_name`).
    let sender_name = sender_id
        .map(|sid| ui_ctx.display_name(&sid))
        .unwrap_or_else(|| "Anonymous".to_string());

    // Resolve sender avatar (conversation-type specific).
    let sender_avatar = match conv_type {
        ConversationType::AnonDm => {
            let dm_id = AnonDmId::from_uuid(*conversation_id.as_uuid());
            let is_own = ui_ctx
                .session_current_user_id()
                .map(|uid| sender_id == Some(uid))
                .unwrap_or(false);
            if is_own {
                ui_ctx.my_avatar()
            } else {
                ui_ctx.get_anon_dm_conversation(&dm_id).and_then(|d| d.peer_avatar)
            }
        }
        ConversationType::AnonGroup => {
            let group_id = AnonGroupId::from_uuid(*conversation_id.as_uuid());
            let sid = sender_id.unwrap_or_else(UserId::new);
            ui_ctx.get_anon_group_member(&group_id, &sid).and_then(|m| m.avatar)
        }
        _ => None,
    };

    // Resolve attachments from attachment service
    let resolved_attachments: Vec<_> = message.attachments.iter()
        .filter_map(|att_id| ui_ctx.get_attachment(att_id))
        .collect();

    // Build display data on this thread (before moving to UI thread)
    // Uses unified converter that handles optional avatars
    let mut display_data = crate::bin_ui::converters::message_to_display_data(
        &message, &sender_name, &resolved_attachments, sender_avatar,
    );
    display_data.decode_attachment_thumbnails();

    // Resolve reply-to sender name (UserService fallback handles all types).
    if display_data.is_reply {
        if let Some(ref author_id) = message.reply_to_author_id {
            display_data.reply_to_sender_name = ui_ctx.display_name(author_id);
        }
        // Signal-Desktop parity: flag when the quoted parent is missing/deleted.
        if let Some(ref parent_id) = message.reply_to {
            display_data.reply_to_not_found = ui_ctx.reply_parent_missing(parent_id);
        }
    }

    let current_user_id = ui_ctx.auth_state().get().current_user_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| ui_ctx.session_current_user_id().map(|id| id.to_string()).unwrap_or_default());

    // Build sidebar preview via the shared core helper.
    let current_user = ui_ctx.auth_state().get().current_user_id;
    let preview = message.sidebar_preview(current_user.as_ref());
    let timestamp_str = super::super::utils::format_timestamp(timestamp);

    // Determine if this conversation is currently active
    let active_id = ui_ctx.chat_state().get().active_channel_id;
    let is_active = active_id.map(|id| id == conversation_id).unwrap_or(false);
    tracing::info!(
        msg_conv = %conversation_id,
        active_channel_id = ?active_id,
        is_active,
        ?conv_type,
        "[UI_RECV] handle_message_received"
    );

    // If the user is looking at this conversation, flip the incoming message
    // (and any previously-stored unreads) to read so the sidebar badge doesn't
    // accumulate. Unread count is computed from messages.is_read.
    if is_active {
        if let Err(e) = ui_ctx.mark_conversation_read(&conversation_id, &conv_type) {
            tracing::warn!(error = %e, "Failed to mark active conversation as read on incoming message");
        }
    }

    // Classify into the typed ActiveConversation via the core helper.
    let sidebar_conversation = ui_ctx.classify_active_conversation(&conversation_id, &conv_type, sender_id);

    // Trigger attachment auto-download in background
    let downloadable: Vec<_> = resolved_attachments.iter()
        .filter(|a| a.remote_url.is_some() && a.encryption_key.is_some())
        .cloned()
        .collect();

    if !downloadable.is_empty() {
        let ui_ctx_dl = ui_ctx.clone();
        let weak_dl = window.clone();
        let conv_id = conversation_id.to_string();
        tokio::spawn(async move {
            super::super::attachment_download::download_attachments(
                &downloadable,
                ui_ctx_dl,
                weak_dl,
                conv_id,
            ).await;
        });
    }

    let weak = window.clone();
    let ui_ctx_ui = ui_ctx.clone();
    let peer_id_opt = sender_id;

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        // Clear typing indicator for sender
        if let Some(sid) = peer_id_opt {
            super::super::typing::clear_typing_for_peer(&w, &sid.to_string());
        }

        if is_active {
            let was_at_bottom = w.get_is_at_bottom();

            // Build Slint message item
            let mut new_msg = display_data.to_slint_with_user(&current_user_id);
            
            // Only non-anon messages get auto-delivered status (anon has its own delivery logic)
            if conv_type != ConversationType::AnonDm && conv_type != ConversationType::AnonGroup {
                new_msg.delivery_status = "delivered".into();
            }

            // Deduplicate by (sender_id, timestamp_ns)
            let current_model = w.get_messages();
            let ts_str = new_msg.timestamp_ns.to_string();
            let sender_str = new_msg.sender_id.to_string();
            let already_exists = (0..current_model.row_count()).any(|i| {
                current_model.row_data(i).map(|m| {
                    m.sender_id.as_str() == sender_str && m.timestamp_ns.as_str() == ts_str
                }).unwrap_or(false)
            });

            tracing::info!(already_exists, existing = current_model.row_count(), "[UI_RECV] dedup check");
            if !already_exists {
                // Find sorted insert position (by timestamp, tiebreak by sender_id)
                let new_ts = ts_str.parse::<u64>().unwrap_or(0);
                let mut messages: Vec<MessageItemData> = (0..current_model.row_count())
                    .filter_map(|i| current_model.row_data(i))
                    .collect();

                let insert_pos = messages.iter().position(|m| {
                    let ts = m.timestamp_ns.to_string().parse::<u64>().unwrap_or(0);
                    match ts.cmp(&new_ts) {
                        std::cmp::Ordering::Greater => true,
                        std::cmp::Ordering::Equal => m.sender_id.as_str() > sender_str.as_str(),
                        std::cmp::Ordering::Less => false,
                    }
                }).unwrap_or(messages.len());

                if let Some(vec_model) = current_model.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                    vec_model.insert(insert_pos, new_msg);
                    tracing::info!(insert_pos, "[UI_RECV] inserted via VecModel.insert");
                } else {
                    messages.insert(insert_pos, new_msg);
                    w.set_messages(ModelRc::new(VecModel::from(messages)));
                    tracing::warn!(insert_pos, "[UI_RECV] downcast failed; replaced model");
                }

                crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                    weak.clone(),
                    ui_ctx_ui.clone(),
                );
            }

            if was_at_bottom {
                w.set_is_at_bottom(true);
            }
        }

        // Always update sidebar preview
        crate::bin_ui::features::sidebar::update_sidebar_preview(
            &w, &ui_ctx_ui, sidebar_conversation, &preview, &timestamp_str, true,
        );

        // Create DM sidebar entry if new peer (non-anon only)
        if conv_type == ConversationType::ServerDm || conv_type == ConversationType::MailboxDm {
            if let Some(peer_id) = peer_id_opt {
                if !crate::bin_ui::features::sidebar::sidebar_has_dm(&ui_ctx_ui, &peer_id) {
                    crate::bin_ui::features::sidebar::create_dm_sidebar_entry(
                        &w, &ui_ctx_ui, &peer_id, &sender_name, &preview, &timestamp_str,
                    );
                }
            }
        }
    });
}

/// Handle unread count updated event.
pub(super) fn handle_unread_count_updated(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: ChannelId,
    peer_id: Option<UserId>,
    unread_count: u32,
) {
    let weak = window.clone();
    let ui_ctx = ui_ctx.clone();
    let channel_id_str = channel_id.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Update sidebar unread badge
            super::helpers::update_unread_count(&w, &ui_ctx, &channel_id_str, unread_count);
        }
    });
    tracing::debug!(
        channel_id = %channel_id,
        ?peer_id,
        unread_count,
        "Updated unread count in UI"
    );
}

/// Handle DM bootstrap completed event.
///
/// Enables the composer and flips "sending" messages to "sent".
pub(super) fn handle_dm_bootstrap_completed(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    peer_id: UserId,
) {
    let weak = window.clone();
    let peer_id_str = peer_id.to_string();
    let ui_ctx_active = ui_ctx.clone();

    // Flush any pending messages for this peer
    let ui_ctx_flush = ui_ctx.clone();
    tokio::spawn(async move {
        ui_ctx_flush.flush_pending_dm_messages(&peer_id).await;
    });

    // Update UI: enable composer + flip delivery status
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        w.set_composer_placeholder("Type a message...".into());

        // Check if this peer's DM is currently active via the typed predicate.
        let is_active = ui_ctx_active.active_conversation().matches_dm_peer(&peer_id);
        let _ = &peer_id_str;

        if is_active {
            let messages = w.get_messages();
            for i in 0..messages.row_count() {
                if let Some(msg) = messages.row_data(i) {
                    if msg.is_own && msg.delivery_status.as_str() == "sending" {
                        let mut updated_msg = msg.clone();
                        updated_msg.delivery_status = "sent".into();
                        if let Some(model) = messages.as_any().downcast_ref::<slint::VecModel<MessageItemData>>() {
                            model.set_row_data(i, updated_msg);
                        }
                    }
                }
            }
        }

        tracing::debug!(peer_id = %peer_id_str, "DM bootstrap complete - composer enabled");
    });
}

/// Handle message sent event.
pub(super) fn handle_message_sent() {
    tracing::debug!("Message sent successfully");
    // UI updates handled by message state
}

/// Update delivery status checkmarks (✓ → ✓✓) in the active message list.
///
/// Called for both `MessagesDelivered` and `MessagesRead` DomainEvents.
pub(super) fn handle_delivery_status_update(
    window: &Weak<MainWindow>,
    message_timestamps: Vec<u64>,
    new_status: &'static str,
) {
    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        let messages = w.get_messages();
        let timestamp_strs: Vec<String> = message_timestamps.iter().map(|ts| ts.to_string()).collect();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.is_own && timestamp_strs.iter().any(|ts| msg.timestamp_ns.as_str() == ts.as_str()) {
                    let mut updated = msg.clone();
                    updated.delivery_status = new_status.into();
                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                        model.set_row_data(i, updated);
                    }
                }
            }
        }
    });
}

/// Update a reaction on a message in the active message list.
///
/// Matches by Signal-style (sender_id, sent_at_ns) composite key — the canonical
/// message identifier per Signal Protocol spec.
pub(super) fn handle_reaction_update(
    window: &Weak<MainWindow>,
    sender_id: &str,
    sent_at_ns: u64,
    emoji: &str,
    remove: bool,
) {
    let weak = window.clone();
    let sender_id = sender_id.to_string();
    let sent_at_ns_str = sent_at_ns.to_string();
    let emoji = emoji.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        crate::bin_ui::features::chat::update_reaction_in_ui(
            &w, &sender_id, &sent_at_ns_str, &emoji, remove,
        );
    });
}

/// Refresh the sidebar preview for the active conversation after a reaction change.
pub(super) fn refresh_sidebar_for_reaction(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    emoji: &str,
    remove: bool,
) {
    let weak = window.clone();
    let conversation = ui_ctx.chat_state().active_conversation();
    let emoji = emoji.to_string();

    if matches!(conversation, chatter_core::state::ActiveConversation::None) {
        return;
    }

    let ui_ctx_for_sidebar = ui_ctx.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        if remove {
            return;
        }
        let preview = format!("Reacted {}", emoji);
        let now = chrono::Local::now().format("%H:%M").to_string();
        crate::bin_ui::features::sidebar::update_sidebar_preview(
            &w, &ui_ctx_for_sidebar, conversation, &preview, &now, false,
        );
    });
}

/// Update a message's content after an edit.
///
/// Matches by timestamp_ns (Signal-style) first. Falls back to message_id (UUID)
/// only for legacy messages that may lack a timestamp.
pub(super) fn handle_message_edit(
    window: &Weak<MainWindow>,
    message_id: &str,
    target_sent_timestamp: u64,
    new_text: &str,
) {
    let weak = window.clone();
    let message_id = message_id.to_string();
    let ts_str = target_sent_timestamp.to_string();
    let new_text = new_text.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        let messages = w.get_messages();

        // Primary: match by timestamp_ns (Signal-style canonical identifier)
        if target_sent_timestamp > 0 {
            for i in 0..messages.row_count() {
                if let Some(msg) = messages.row_data(i) {
                    if msg.timestamp_ns.as_str() == ts_str {
                        let mut updated = msg.clone();
                        updated.content = new_text.into();
                        updated.is_edited = true;
                        if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                            model.set_row_data(i, updated);
                        }
                        return;
                    }
                }
            }
        }

        // Fallback: match by message_id (UUID) for legacy messages
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.message_id.as_str() == message_id {
                    let mut updated = msg.clone();
                    updated.content = new_text.clone().into();
                    updated.is_edited = true;
                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                        model.set_row_data(i, updated);
                    }
                    return;
                }
            }
        }
    });
}

/// Remove a message from the active message list (delete for everyone).
///
/// Matches by timestamp_ns (Signal-style) first. Falls back to message_id (UUID)
/// for legacy messages.
pub(super) fn handle_message_delete(
    window: &Weak<MainWindow>,
    message_id: &str,
    target_sent_timestamp: u64,
) {
    let weak = window.clone();
    let message_id = message_id.to_string();
    let ts_str = target_sent_timestamp.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        let messages = w.get_messages();

        let mut new_messages = Vec::new();
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                // Primary: match by timestamp_ns (Signal-style)
                let is_target = if target_sent_timestamp > 0 {
                    msg.timestamp_ns.as_str() == ts_str
                } else {
                    msg.message_id.as_str() == message_id
                };
                if !is_target {
                    new_messages.push(msg);
                }
            }
        }

        if new_messages.len() < messages.row_count() {
            w.set_messages(ModelRc::new(VecModel::from(new_messages)));
        }
    });
}

/// Handle message send failed event — show error toast in the UI.
pub(super) fn handle_message_send_failed(
    window: &Weak<MainWindow>,
    channel_id: ChannelId,
    error: String,
) {
    tracing::error!(
        channel_id = %channel_id,
        error = %error,
        "Message send failed"
    );

    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        w.set_toast_message("Failed to send message. Please try again.".into());
        w.set_toast_variant("error".into());
        w.set_toast_visible(true);
    });
}

// Channel member joined/left system messages are handled via
// handle_message_received() — the DomainEvent carries the Message directly.

/// Update the header member count if the given channel is currently active.
///
/// Called after ChannelMemberJoined/Left events. The local membership repo
/// is already updated by the time this runs, so we query it for the count.
pub(super) fn update_active_channel_member_count(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    channel_id: ChannelId,
) {
    let is_active = ui_ctx.chat_state().get().active_channel_id
        .map(|id| id == channel_id)
        .unwrap_or(false);

    if !is_active {
        return;
    }

    let member_count = ui_ctx.list_channel_members_local(&channel_id).len() as i32;

    let weak = window.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.set_member_count(member_count);
        }
    });
}
