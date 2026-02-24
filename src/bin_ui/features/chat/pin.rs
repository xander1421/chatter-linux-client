//! Pin message callback for DM and channel conversations.

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::{Attachment, FileCategory, Message, UserId};
use chatter_linux::services::{build_pin_content, build_pin_content_with_channel};
use chatter_linux::storage::AttachmentRepository;

use super::helpers::{get_peer_id_from_dm_channel, send_channel_content, send_dm_content};
use crate::MainWindow;

/// Generate a preview text for a pinned message.
/// Prioritizes text content, falls back to attachment description.
fn generate_pinned_preview(message: &Message, attachments: &[Attachment]) -> String {
    // First try to get text content
    if let Some(text) = message.as_text() {
        if !text.is_empty() {
            return if text.len() > 80 {
                format!("{}...", &text[..77])
            } else {
                text.to_string()
            };
        }
    }

    // If no text, describe attachments
    if !attachments.is_empty() {
        if attachments.len() == 1 {
            let att = &attachments[0];
            let category = att.metadata.category();
            let emoji = match category {
                FileCategory::Image => "📷",
                FileCategory::Video => "🎬",
                FileCategory::Audio => "🎵",
                FileCategory::Document => "📄",
                FileCategory::Code => "📝",
                FileCategory::Archive => "📦",
                FileCategory::Other => "📎",
            };
            let type_name = match category {
                FileCategory::Image => "Photo",
                FileCategory::Video => "Video",
                FileCategory::Audio => "Voice message",
                FileCategory::Document => "Document",
                FileCategory::Code => "Code",
                FileCategory::Archive => "Archive",
                FileCategory::Other => "File",
            };
            return format!("{} {}", emoji, type_name);
        } else {
            return format!("📎 {} attachments", attachments.len());
        }
    }

    // Fallback for attachment IDs without resolved metadata
    if !message.attachments.is_empty() {
        if message.attachments.len() == 1 {
            return "📎 Attachment".to_string();
        } else {
            return format!("📎 {} attachments", message.attachments.len());
        }
    }

    // Empty message
    String::new()
}

/// Resolve attachments from message attachment IDs.
fn resolve_attachments(message: &Message, attachment_repo: &AttachmentRepository) -> Vec<Attachment> {
    message
        .attachments
        .iter()
        .filter_map(|att_id| attachment_repo.get(att_id).ok().flatten())
        .collect()
}

/// Wire pin-message callback for DM and channel conversations.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_pin_message(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let message_repo = ctx.message_repo.clone();
    let attachment_repo = ctx.attachment_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();
    let group_fanout = ctx.get_group_fanout();
    let pinned_state = ctx.pinned_state.clone();

    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_pin_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        tracing::info!(
            sender_id = %sender_id_str,
            timestamp_ns = %timestamp_ns_str,
            "UI callback: pin/unpin triggered (Signal-style lookup)"
        );

        // Get selected DM channel ID and look up peer
        let (selected_dm_id, peer_user_id) = weak_window
            .upgrade()
            .map(|w| {
                let dm_id = w.get_selected_dm_id().to_string();
                let peer_id = if !dm_id.is_empty() {
                    get_peer_id_from_dm_channel(&w, &dm_id)
                } else {
                    None
                };
                (dm_id, peer_id)
            })
            .unwrap_or_default();

        let auth_state = auth_state.clone();
        let message_repo = message_repo.clone();
        let weak = weak_window.clone();

        // DM pin via sealed sender
        if !selected_dm_id.is_empty() {
            let peer_user_id = match peer_user_id {
                Some(id) => id,
                None => return,
            };

            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let message_service = message_service.clone();
            let pinned_state = pinned_state.clone();
            let attachment_repo = attachment_repo.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for pin message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let peer_id: UserId = match peer_user_id.parse() {
                        Ok(id) => id,
                        Err(_) => return,
                    };

                    // Parse sender_id for Signal-style lookup
                    let target_sender_id: UserId = match sender_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for pin");
                            return;
                        }
                    };

                    // Parse timestamp for Signal-style lookup
                    let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                        Ok(ts) => ts,
                        Err(e) => {
                            tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for pin");
                            return;
                        }
                    };

                    // Signal-style lookup: (sender_id, sent_at)
                    let message = match message_repo.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!(
                                sender_id = %target_sender_id,
                                timestamp = target_sent_timestamp,
                                "Message not found for pin (Signal-style lookup)"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to look up message for pin");
                            return;
                        }
                    };

                    let msg_id = message.id;
                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();

                    // Toggle pin state
                    let unpin = message.is_pinned;
                    let action = if unpin { "unpin" } else { "pin" };

                    tracing::debug!(
                        sender_id = %sender_id_str,
                        timestamp = target_sent_timestamp,
                        peer = %peer_id,
                        author = %target_author_aci,
                        action = %action,
                        "Sending DM pin/unpin"
                    );

                    // Build pin content
                    let content = build_pin_content(
                        &target_author_aci,
                        target_sent_timestamp,
                        unpin,
                    );

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database.clone(),
                        message_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!(error = %e, "Failed to send DM pin");
                        return;
                    }

                    tracing::info!(
                        sender_id = %sender_id_str,
                        timestamp = %timestamp_ns_str,
                        action = %action,
                        "DM pin/unpin sent successfully"
                    );

                    // Update locally
                    let new_pinned_state = !unpin;
                    let mut updated_message = message.clone();
                    updated_message.is_pinned = new_pinned_state;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!(error = %e, "Failed to update pin status locally");
                    } else {
                        tracing::debug!(
                            sender_id = %sender_id_str,
                            pinned = new_pinned_state,
                            "Updated pin status locally"
                        );

                        // Update pinned state for header bar
                        if new_pinned_state {
                            pinned_state.add_pinned_message(msg_id);
                        } else {
                            pinned_state.remove_pinned_message(&msg_id);
                        }
                        let pinned_count = pinned_state.get().pinned_message_ids.len();

                        // Generate preview with attachment support
                        let attachments = resolve_attachments(&message, &attachment_repo);
                        let preview_text = generate_pinned_preview(&message, &attachments);
                        tracing::debug!(preview = %preview_text, "Generated pinned preview");

                        // Update UI to reflect pin state change using Signal-style lookup
                        let sender_id_for_ui = sender_id_str.clone();
                        let timestamp_for_ui = timestamp_ns_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                // Update message model using Signal-style (sender_id, timestamp_ns)
                                let current_model = w.get_messages();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .map(|mut m| {
                                        if m.sender_id.as_str() == sender_id_for_ui.as_str()
                                            && m.timestamp_ns.as_str() == timestamp_for_ui.as_str()
                                        {
                                            m.is_pinned = new_pinned_state;
                                            tracing::info!(
                                                sender_id = %sender_id_for_ui,
                                                timestamp = %timestamp_for_ui,
                                                pinned = new_pinned_state,
                                                "UI updated: toggled pin state"
                                            );
                                        }
                                        m
                                    })
                                    .collect();
                                w.set_messages(ModelRc::new(VecModel::from(messages)));

                                // Update pinned header bar
                                w.set_has_pinned_messages(pinned_count > 0);
                                if new_pinned_state {
                                    tracing::info!(
                                        count = pinned_count,
                                        preview = %preview_text,
                                        "UI updated: pinned header bar shown"
                                    );
                                    w.set_pinned_message_preview(preview_text.into());
                                    w.set_pinned_current_index(pinned_count as i32);
                                    w.set_pinned_total_count(pinned_count as i32);
                                }
                            }
                        });
                    }
                });
            });
        } else {
            // Channel pin via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();
            let group_fanout = group_fanout.clone();
            let pinned_state = pinned_state.clone();
            let attachment_repo = attachment_repo.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel pin message");

                rt.block_on(async {
                    if !auth_state.is_logged_in() {
                        return;
                    }

                    let channel_id = match chat_state.get().active_channel_id {
                        Some(id) => id,
                        None => return,
                    };

                    // Parse sender_id for Signal-style lookup
                    let target_sender_id: UserId = match sender_id_str.parse() {
                        Ok(id) => id,
                        Err(e) => {
                            tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for channel pin");
                            return;
                        }
                    };

                    // Parse timestamp for Signal-style lookup
                    let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                        Ok(ts) => ts,
                        Err(e) => {
                            tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for channel pin");
                            return;
                        }
                    };

                    // Signal-style lookup: (sender_id, sent_at)
                    let message = match message_repo.get_by_sender_timestamp(&target_sender_id, target_sent_timestamp) {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            tracing::error!(
                                sender_id = %target_sender_id,
                                timestamp = target_sent_timestamp,
                                "Message not found for channel pin (Signal-style lookup)"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to look up message for channel pin");
                            return;
                        }
                    };

                    let msg_id = message.id;
                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();

                    // Toggle pin state
                    let unpin = message.is_pinned;
                    let action = if unpin { "unpin" } else { "pin" };

                    tracing::debug!(
                        sender_id = %sender_id_str,
                        timestamp = target_sent_timestamp,
                        channel = %channel_id,
                        author = %target_author_aci,
                        action = %action,
                        "Sending channel pin/unpin"
                    );

                    // Build pin content with channel context
                    let channel_id_str = channel_id.to_string();
                    let content = build_pin_content_with_channel(
                        &target_author_aci,
                        target_sent_timestamp,
                        unpin,
                        Some(&channel_id_str),
                    );

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        group_fanout,
                    )
                    .await
                    {
                        tracing::error!(error = %e, "Failed to send channel pin");
                        return;
                    }

                    tracing::info!(
                        sender_id = %sender_id_str,
                        timestamp = %timestamp_ns_str,
                        channel = %channel_id,
                        action = %action,
                        "Channel pin/unpin sent successfully"
                    );

                    // Update locally
                    let new_pinned_state = !unpin;
                    let mut updated_message = message.clone();
                    updated_message.is_pinned = new_pinned_state;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!(error = %e, "Failed to update pin status locally");
                    } else {
                        tracing::debug!(
                            sender_id = %sender_id_str,
                            channel = %channel_id,
                            pinned = new_pinned_state,
                            "Updated channel pin status locally"
                        );

                        // Update pinned state for header bar
                        if new_pinned_state {
                            pinned_state.add_pinned_message(msg_id);
                        } else {
                            pinned_state.remove_pinned_message(&msg_id);
                        }
                        let pinned_count = pinned_state.get().pinned_message_ids.len();

                        // Generate preview with attachment support
                        let attachments = resolve_attachments(&message, &attachment_repo);
                        let preview_text = generate_pinned_preview(&message, &attachments);
                        tracing::debug!(preview = %preview_text, "Generated channel pinned preview");

                        // Update UI to reflect pin state change using Signal-style lookup
                        let sender_id_for_ui = sender_id_str.clone();
                        let timestamp_for_ui = timestamp_ns_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                // Update message model using Signal-style (sender_id, timestamp_ns)
                                let current_model = w.get_messages();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .map(|mut m| {
                                        if m.sender_id.as_str() == sender_id_for_ui.as_str()
                                            && m.timestamp_ns.as_str() == timestamp_for_ui.as_str()
                                        {
                                            m.is_pinned = new_pinned_state;
                                            tracing::info!(
                                                sender_id = %sender_id_for_ui,
                                                timestamp = %timestamp_for_ui,
                                                pinned = new_pinned_state,
                                                "UI updated: toggled channel pin state"
                                            );
                                        }
                                        m
                                    })
                                    .collect();
                                w.set_messages(ModelRc::new(VecModel::from(messages)));

                                // Update pinned header bar
                                w.set_has_pinned_messages(pinned_count > 0);
                                if new_pinned_state {
                                    tracing::info!(
                                        count = pinned_count,
                                        preview = %preview_text,
                                        "UI updated: channel pinned header bar shown"
                                    );
                                    w.set_pinned_message_preview(preview_text.into());
                                    w.set_pinned_current_index(pinned_count as i32);
                                    w.set_pinned_total_count(pinned_count as i32);
                                }
                            }
                        });
                    }
                });
            });
        }
    });
}
