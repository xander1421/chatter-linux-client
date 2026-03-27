//! Message receive/send/fail, channel member, pin/unpin, and unread count handlers.

use std::sync::Arc;

use slint::{Model, ModelRc, VecModel, Weak};

use chatter_core::client::ChatterClient;
use chatter_core::domain::channel::ChannelType;
use chatter_core::domain::ids::{ChannelId, UserId};
use chatter_core::domain::message::Message;
use crate::MainWindow;
use crate::MessageItemData;

/// Handle an incoming message (DM or channel).
///
/// Resolves sender name + attachments, inserts into the Slint message model
/// if the conversation is active, updates sidebar preview, and triggers
/// attachment auto-download.
pub(super) fn handle_message_received(
    window: &Weak<MainWindow>,
    ui_ctx: &Arc<ChatterClient>,
    message: Message,
) {
    let channel_id = message.channel_id;
    let sender_id = message.sender_id;
    let timestamp = message.sent_at;

    // Resolve sender display name (profile → user → UUID fallback)
    let sender_name = sender_id
        .and_then(|sid| {
            ui_ctx.users()
                .get_profile_local(&sid).ok().flatten()
                .map(|p| p.display_name.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| {
                    ui_ctx.users()
                        .get_user_local(&sid).ok().flatten()
                        .map(|u| u.username.clone())
                        .filter(|n| !n.is_empty())
                })
                .or_else(|| Some(sid.to_string()))
        })
        .unwrap_or_else(|| "Anonymous".to_string());

    // Resolve attachments from attachment service
    let resolved_attachments: Vec<_> = message.attachments.iter()
        .filter_map(|att_id| ui_ctx.attachments().get(att_id).ok().flatten())
        .collect();

    // Build display data on this thread (before moving to UI thread)
    let mut display_data = crate::bin_ui::converters::message_to_display_data(
        &message, &sender_name, &resolved_attachments,
    );
    display_data.decode_attachment_thumbnails();

    let current_user_id = ui_ctx.auth_state().get().current_user_id
        .map(|id| id.to_string())
        .unwrap_or_default();

    // Build sidebar preview
    let text = message.as_text().unwrap_or_default();
    let att_count = message.attachments.len();
    let preview = if text.is_empty() && att_count > 0 {
        format!("\u{1F4CE} {} attachment{}", att_count, if att_count == 1 { "" } else { "s" })
    } else if text.chars().count() > 50 {
        let truncated: String = text.chars().take(47).collect();
        format!("{}...", truncated)
    } else {
        text.to_string()
    };
    let timestamp_str = super::super::utils::format_timestamp(timestamp);

    // Determine if this conversation is currently active
    let is_active = ui_ctx.chat_state().get().active_channel_id
        .map(|id| id == channel_id)
        .unwrap_or(false);

    // Determine conversation type for sidebar update
    let conversation = if let Ok(Some(channel)) = ui_ctx.channels().get_channel_local(&channel_id) {
        if channel.channel_type == ChannelType::DirectMessage {
            if let Some(sid) = sender_id {
                chatter_core::state::ActiveConversation::MailboxDm { channel_id, peer_id: sid }
            } else {
                chatter_core::state::ActiveConversation::Channel(channel_id)
            }
        } else {
            chatter_core::state::ActiveConversation::Channel(channel_id)
        }
    } else {
        chatter_core::state::ActiveConversation::Channel(channel_id)
    };

    // Trigger attachment auto-download in background
    let downloadable: Vec<_> = resolved_attachments.iter()
        .filter(|a| a.remote_url.is_some() && a.encryption_key.is_some())
        .cloned()
        .collect();

    if !downloadable.is_empty() {
        let ui_ctx_dl = ui_ctx.clone();
        let weak_dl = window.clone();
        let conv_id = channel_id.to_string();
        tokio::spawn(async move {
            super::super::attachment_download::download_attachments(
                &downloadable,
                ui_ctx_dl.attachments().clone(),
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
            new_msg.delivery_status = "delivered".into();

            // Deduplicate by (sender_id, timestamp_ns)
            let current_model = w.get_messages();
            let ts_str = new_msg.timestamp_ns.to_string();
            let sender_str = new_msg.sender_id.to_string();
            let already_exists = (0..current_model.row_count()).any(|i| {
                current_model.row_data(i).map(|m| {
                    m.sender_id.as_str() == sender_str && m.timestamp_ns.as_str() == ts_str
                }).unwrap_or(false)
            });

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
                } else {
                    messages.insert(insert_pos, new_msg);
                    w.set_messages(ModelRc::new(VecModel::from(messages)));
                }

                crate::bin_ui::gif_state::auto_register_gifs_from_messages(
                    weak.clone(),
                    ui_ctx_ui.attachments().clone(),
                );
            }

            if was_at_bottom {
                w.set_message_scroll_position(-99999.0);
                w.set_is_at_bottom(true);
            }
        }

        // Always update sidebar preview
        crate::bin_ui::features::sidebar::update_sidebar_preview(
            &w, conversation, &preview, &timestamp_str, true,
        );

        // Create DM sidebar entry if new peer
        if let Some(peer_id) = peer_id_opt {
            if !crate::bin_ui::features::sidebar::sidebar_has_dm(&w, &peer_id) {
                crate::bin_ui::features::sidebar::create_dm_sidebar_entry(
                    &w, &ui_ctx_ui, &peer_id, &sender_name, &preview, &timestamp_str,
                );
            }
        }
    });
}

/// Handle unread count updated event.
pub(super) fn handle_unread_count_updated(
    window: &Weak<MainWindow>,
    channel_id: ChannelId,
    peer_id: Option<UserId>,
    unread_count: u32,
) {
    let weak = window.clone();
    let channel_id_str = channel_id.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            // Update sidebar unread badge
            super::helpers::update_unread_count(&w, &channel_id_str, unread_count);
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

    // Flush any pending messages for this peer
    let dm_bootstrap = ui_ctx.dm_bootstrap().clone();
    tokio::spawn(async move {
        dm_bootstrap.flush_pending_ui_messages(&peer_id).await;
    });

    // Update UI: enable composer + flip delivery status
    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };

        w.set_composer_placeholder("Type a message...".into());

        // Check if this peer's DM is currently active
        let selected_dm = w.get_selected_dm_id().to_string();
        let peer_channel_id = crate::bin_ui::features::chat::get_dm_channel_from_peer(&w, &peer_id_str);
        let is_active = peer_channel_id.as_deref() == Some(selected_dm.as_str())
            || selected_dm == peer_id_str;

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
pub(super) fn handle_reaction_update(
    window: &Weak<MainWindow>,
    message_id: &str,
    emoji: &str,
    remove: bool,
    _timestamp: u64,
) {
    let weak = window.clone();
    let message_id = message_id.to_string();
    let emoji = emoji.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        let messages = w.get_messages();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.message_id.as_str() == message_id {
                    let mut updated = msg.clone();
                    let new_summary = super::super::utils::update_reaction_summary(
                        &msg.reaction_summary.to_string(), &emoji, remove,
                    );
                    updated.reaction_summary = new_summary.clone().into();
                    updated.has_reactions = !new_summary.is_empty();
                    updated.reactions = ModelRc::new(VecModel::from(
                        crate::bin_ui::converters::parse_reaction_summary_to_entries(&new_summary),
                    ));
                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>() {
                        model.set_row_data(i, updated);
                    }
                    return;
                }
            }
        }
    });
}

/// Update a message's content after an edit.
pub(super) fn handle_message_edit(
    window: &Weak<MainWindow>,
    message_id: &str,
    new_text: &str,
) {
    let weak = window.clone();
    let message_id = message_id.to_string();
    let new_text = new_text.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        let messages = w.get_messages();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.message_id.as_str() == message_id {
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
    });
}

/// Remove a message from the active message list (delete for everyone).
pub(super) fn handle_message_delete(
    window: &Weak<MainWindow>,
    message_id: &str,
) {
    let weak = window.clone();
    let message_id = message_id.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(w) = weak.upgrade() else { return };
        let messages = w.get_messages();

        let mut new_messages = Vec::new();
        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.message_id.as_str() != message_id {
                    new_messages.push(msg);
                }
            }
        }

        if new_messages.len() < messages.row_count() {
            w.set_messages(ModelRc::new(VecModel::from(new_messages)));
        }
    });
}

/// Handle message send failed event.
pub(super) fn handle_message_send_failed(channel_id: ChannelId, error: String) {
    tracing::error!(
        channel_id = %channel_id,
        error = %error,
        "Message send failed"
    );
    // TODO: Show error notification in UI
}

// Channel member joined/left system messages are handled via
// handle_message_received() — the DomainEvent carries the Message directly.
