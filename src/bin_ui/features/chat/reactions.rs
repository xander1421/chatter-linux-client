//! Reaction callbacks for DM and channel conversations.

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use chatter_linux::app::AppContext;
use chatter_linux::domain::UserId;
use chatter_linux::services::{build_reaction_content, build_reaction_content_with_channel};

use super::helpers::{get_peer_id_from_dm_channel, send_channel_content, send_dm_content};
use crate::bin_ui::features::auth::update_reaction_summary;
use crate::MainWindow;

/// Wire reaction selected callback for DM and channel conversations.
///
/// Uses Signal-style (sender_id, timestamp_ns) tuple for message identification
/// instead of unreliable server message IDs.
pub fn wire_reaction_selected(window: &MainWindow, ctx: &AppContext) {
    let auth_state = ctx.auth_state.clone();
    let mailbox_manager = ctx.mailbox_manager.clone();
    let database = ctx.database.clone();
    let message_repo = ctx.message_repo.clone();
    let chat_state = ctx.chat_state.clone();
    let message_service = ctx.message_service();
    let group_fanout = ctx.get_group_fanout();

    let weak_window = window.as_weak();

    // Callback signature: (sender_id, timestamp_ns, message_id, emoji)
    window.on_reaction_selected(move |sender_id: SharedString, timestamp_ns: SharedString, message_id: SharedString, emoji: SharedString| {
        let sender_id_str = sender_id.to_string();
        let timestamp_ns_str = timestamp_ns.to_string();
        let message_id_str = message_id.to_string();
        let emoji_str = emoji.to_string();

        tracing::info!(
            sender_id = %sender_id_str,
            timestamp_ns = %timestamp_ns_str,
            message_id = %message_id_str,
            emoji = %emoji_str,
            "UI callback: reaction selected (Signal-style lookup)"
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

        // DM reaction via sealed sender
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
                    .expect("Failed to create runtime for reaction send");

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
                            tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for reaction");
                            return;
                        }
                    };

                    // Parse timestamp for Signal-style lookup
                    let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                        Ok(ts) => ts,
                        Err(e) => {
                            tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for reaction");
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
                                "Message not found for reaction (Signal-style lookup)"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to look up message for reaction");
                            return;
                        }
                    };

                    // Get the message author (target_author_aci) - should match sender_id
                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();

                    tracing::debug!(
                        sender_id = %sender_id_str,
                        timestamp = target_sent_timestamp,
                        peer = %peer_id,
                        emoji = %emoji_str,
                        author = %target_author_aci,
                        "Sending DM reaction"
                    );

                    // Build reaction content
                    // remove=false means adding a reaction
                    let content = build_reaction_content(
                        &emoji_str,
                        false,
                        &target_author_aci,
                        target_sent_timestamp,
                    );

                    if let Err(e) = send_dm_content(
                        content,
                        &peer_id,
                        database,
                        message_service,
                        mailbox_manager,
                    )
                    .await
                    {
                        tracing::error!(error = %e, "Failed to send DM reaction");
                    } else {
                        tracing::info!(
                            sender_id = %sender_id_str,
                            timestamp = %timestamp_ns_str,
                            emoji = %emoji_str,
                            peer = %peer_id,
                            "DM reaction sent successfully"
                        );

                        // Update UI to reflect reaction using Signal-style (sender_id, timestamp_ns)
                        let emoji_for_update = emoji_str.clone();
                        let sender_id_for_ui = sender_id_str.clone();
                        let timestamp_for_ui = timestamp_ns_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let current_model = w.get_messages();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .map(|mut m| {
                                        // Signal-style UI lookup: match by (sender_id, timestamp_ns)
                                        if m.sender_id.as_str() == sender_id_for_ui.as_str()
                                            && m.timestamp_ns.as_str() == timestamp_for_ui.as_str()
                                        {
                                            let new_summary = update_reaction_summary(
                                                &m.reaction_summary.to_string(),
                                                &emoji_for_update,
                                                false, // adding, not removing
                                            );
                                            m.reaction_summary = new_summary.clone().into();
                                            m.has_reactions = !new_summary.is_empty();
                                            tracing::info!(
                                                sender_id = %sender_id_for_ui,
                                                timestamp = %timestamp_for_ui,
                                                emoji = %emoji_for_update,
                                                summary = %new_summary,
                                                "UI updated: added reaction"
                                            );
                                        }
                                        m
                                    })
                                    .collect();
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                            }
                        });
                    }
                });
            });
        } else {
            // Channel reaction via sender key
            let chat_state = chat_state.clone();
            let message_service = message_service.clone();
            let group_fanout = group_fanout.clone();

            tokio::task::spawn_blocking(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create runtime for channel reaction send");

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
                            tracing::error!(error = %e, sender_id = %sender_id_str, "Invalid sender ID for channel reaction");
                            return;
                        }
                    };

                    // Parse timestamp for Signal-style lookup
                    let target_sent_timestamp: u64 = match timestamp_ns_str.parse() {
                        Ok(ts) => ts,
                        Err(e) => {
                            tracing::error!(error = %e, timestamp = %timestamp_ns_str, "Invalid timestamp for channel reaction");
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
                                "Message not found for channel reaction (Signal-style lookup)"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to look up message for channel reaction");
                            return;
                        }
                    };

                    let target_author_aci = message
                        .sender_id
                        .map(|id| id.to_string())
                        .unwrap_or_default();

                    tracing::debug!(
                        sender_id = %sender_id_str,
                        timestamp = target_sent_timestamp,
                        channel = %channel_id,
                        emoji = %emoji_str,
                        author = %target_author_aci,
                        "Sending channel reaction"
                    );

                    // Build reaction content with channel context
                    let content = build_reaction_content_with_channel(
                        &emoji_str,
                        false,
                        &target_author_aci,
                        target_sent_timestamp,
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
                        tracing::error!(error = %e, "Failed to send channel reaction");
                    } else {
                        tracing::info!(
                            sender_id = %sender_id_str,
                            timestamp = %timestamp_ns_str,
                            emoji = %emoji_str,
                            channel = %channel_id,
                            "Channel reaction sent successfully"
                        );

                        // Update UI to reflect reaction using Signal-style (sender_id, timestamp_ns)
                        let emoji_for_update = emoji_str.clone();
                        let sender_id_for_ui = sender_id_str.clone();
                        let timestamp_for_ui = timestamp_ns_str.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                let current_model = w.get_messages();
                                let messages: Vec<_> = (0..current_model.row_count())
                                    .filter_map(|i| current_model.row_data(i))
                                    .map(|mut m| {
                                        // Signal-style UI lookup: match by (sender_id, timestamp_ns)
                                        if m.sender_id.as_str() == sender_id_for_ui.as_str()
                                            && m.timestamp_ns.as_str() == timestamp_for_ui.as_str()
                                        {
                                            let new_summary = update_reaction_summary(
                                                &m.reaction_summary.to_string(),
                                                &emoji_for_update,
                                                false, // adding, not removing
                                            );
                                            m.reaction_summary = new_summary.clone().into();
                                            m.has_reactions = !new_summary.is_empty();
                                            tracing::info!(
                                                sender_id = %sender_id_for_ui,
                                                timestamp = %timestamp_for_ui,
                                                emoji = %emoji_for_update,
                                                summary = %new_summary,
                                                "UI updated: added channel reaction"
                                            );
                                        }
                                        m
                                    })
                                    .collect();
                                w.set_messages(ModelRc::new(VecModel::from(messages)));
                            }
                        });
                    }
                });
            });
        }
    });
}
