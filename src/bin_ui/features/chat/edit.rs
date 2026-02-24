//! Edit message callbacks for DM and channel conversations.
//! Uses Signal-style (sender_id, timestamp_ns) for message identification.

use slint::{ComponentHandle, SharedString};

use chatter_linux::app::AppContext;
use chatter_linux::domain::UserId;
use chatter_linux::services::{build_edit_content, build_edit_content_with_channel};

use super::helpers::{get_peer_id_from_dm_channel, send_channel_content, send_dm_content};
use crate::MainWindow;

/// Wire edit-message callback - enters edit mode with original message text.
/// Uses Signal-style (sender_id, timestamp_ns) for message identification.
pub fn wire_edit_message(window: &MainWindow, ctx: &AppContext) {
    let message_repo = ctx.message_repo.clone();
    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns) - Signal-style identification
    window.on_edit_message(move |sender_id: SharedString, timestamp_ns: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let message_repo = message_repo.clone();
        let weak = weak_window.clone();

        // Parse sender_id for Signal-style lookup
        let target_sender_id: UserId = match sender_id_str.parse() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for edit");
                return;
            }
        };

        // Parse timestamp for Signal-style lookup
        let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
            Ok(ts) => ts,
            Err(e) => {
                tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for edit");
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
                    "Message not found for edit (Signal-style lookup)"
                );
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to look up message for edit");
                return;
            }
        };

        // Extract original text
        use chatter_linux::domain::message::MessageContent;
        let original_text = match &message.content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Mixed(m) => m.text.clone().unwrap_or_default(),
            _ => {
                tracing::debug!("Cannot edit non-text message content");
                return;
            }
        };

        // Enter edit mode - store Signal-style IDs for use in send_edit
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_composer_text(original_text.into());
                // Store sender_id:timestamp_ns as composite key for send_edit
                let composite_key = format!("{}:{}", sender_id_str, timestamp_ns_str);
                w.set_composer_edit_message_id(composite_key.into());
                w.set_composer_is_editing(true);
                tracing::debug!(
                    sender_id = %sender_id_str,
                    timestamp = %timestamp_ns_str,
                    "Entered edit mode (Signal-style)"
                );
            }
        });
    });
}

/// Wire send-edit callback - sends EditMessage via sealed sender (DM) or sender key (channel).
/// Uses Signal-style (sender_id, timestamp_ns) stored in composer_edit_message_id as "sender_id:timestamp_ns".
pub fn wire_send_edit(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();
    let group_fanout = ctx.get_group_fanout();

    let weak_window = window.as_weak();

    // send_edit callback receives (message_id_or_composite_key, new_text)
    // The message_id is now a composite "sender_id:timestamp_ns" key
    window.on_send_edit(move |composite_key: SharedString, new_text: SharedString| {
        let composite_key_str = composite_key.to_string();
        let new_text_str = new_text.to_string();

        // Parse composite key "sender_id:timestamp_ns"
        let parts: Vec<&str> = composite_key_str.splitn(2, ':').collect();
        let (sender_id_str, timestamp_ns_str) = if parts.len() == 2 {
            (parts[0].to_string(), parts[1].to_string())
        } else {
            tracing::error!(composite_key = %composite_key_str, "Invalid composite key for send_edit");
            return;
        };

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

        // DM edit via sealed sender
        if !selected_dm_id.is_empty() {
            let peer_user_id = match peer_user_id {
                Some(id) => id,
                None => return,
            };

            let mailbox_manager = mailbox_manager.clone();
            let database = database.clone();
            let message_service = message_service.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for edit message");

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
                            tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for edit");
                            return;
                        }
                    };

                    // Parse timestamp for Signal-style lookup
                    let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                        Ok(ts) => ts,
                        Err(e) => {
                            tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for edit");
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
                                "Message not found for edit (Signal-style lookup)"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to look up message for edit");
                            return;
                        }
                    };

                    // Build edit content
                    let content = build_edit_content(target_sent_timestamp, &new_text_str);

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database.clone(),
                        message_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!(error = %e, "Failed to send DM edit");
                        return;
                    }

                    // Update locally
                    use chatter_linux::domain::message::MessageContent;
                    let mut updated_message = message.clone();
                    updated_message.content = MessageContent::Text(new_text_str.clone());
                    updated_message.is_edited = true;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!(error = %e, "Failed to update message locally");
                    } else {
                        tracing::debug!(
                            sender_id = %sender_id_str,
                            timestamp = %timestamp_ns_str,
                            peer = %peer_id,
                            "Edited DM message (Signal-style)"
                        );
                    }

                    // Exit edit mode and clear composer
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_composer_is_editing(false);
                            w.set_composer_edit_message_id("".into());
                            w.set_composer_text("".into());
                        }
                    });
                });
            });
        } else {
            // Channel edit via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();
            let group_fanout = group_fanout.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel edit message");

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
                            tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for channel edit");
                            return;
                        }
                    };

                    // Parse timestamp for Signal-style lookup
                    let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                        Ok(ts) => ts,
                        Err(e) => {
                            tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for channel edit");
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
                                "Message not found for channel edit (Signal-style lookup)"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to look up message for channel edit");
                            return;
                        }
                    };

                    // Build edit content with channel context
                    let content = build_edit_content_with_channel(
                        target_sent_timestamp,
                        &new_text_str,
                        Some(&channel_id.to_string()),
                    );

                    if let Err(e) = send_channel_content(
                        content,
                        &channel_id,
                        message_service,
                        group_fanout,
                    )
                    .await
                    {
                        tracing::error!(error = %e, "Failed to send channel edit");
                        return;
                    }

                    // Update locally
                    use chatter_linux::domain::message::MessageContent;
                    let mut updated_message = message.clone();
                    updated_message.content = MessageContent::Text(new_text_str.clone());
                    updated_message.is_edited = true;
                    if let Err(e) = message_repo.update(&updated_message) {
                        tracing::error!(error = %e, "Failed to update message locally");
                    } else {
                        tracing::debug!(
                            sender_id = %sender_id_str,
                            timestamp = %timestamp_ns_str,
                            channel = %channel_id,
                            "Edited channel message (Signal-style)"
                        );
                    }

                    // Exit edit mode and clear composer
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_composer_is_editing(false);
                            w.set_composer_edit_message_id("".into());
                            w.set_composer_text("".into());
                        }
                    });
                });
            });
        }
    });
}

/// Wire cancel-edit callback - exits edit mode without sending.
pub fn wire_cancel_edit(window: &MainWindow) {
    let weak_window = window.as_weak();

    window.on_cancel_edit(move || {
        let _ = slint::invoke_from_event_loop({
            let weak = weak_window.clone();
            move || {
                if let Some(w) = weak.upgrade() {
                    w.set_composer_is_editing(false);
                    w.set_composer_edit_message_id("".into());
                    w.set_composer_text("".into());
                    tracing::debug!("Cancelled edit mode");
                }
            }
        });
    });
}
