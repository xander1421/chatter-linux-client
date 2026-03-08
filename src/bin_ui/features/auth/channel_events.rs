//! Channel event stream handling for UI updates.
//!
//! Subscribes to channel events (member joins, leaves, etc.) and updates
//! the UI with system messages.

use std::sync::Arc;

use slint::{Model, Weak};
use tokio_stream::StreamExt;
use tracing::{debug, error, info, warn};

use chatter_linux::app::UIContext;
use chatter_linux::domain::channel::{MemberRole, Membership, NotificationPreference};
use chatter_linux::domain::ids::{ChannelId, UserId};
use chatter_linux::network::grpc::channel_client::ChannelClient;
use chatter_linux::proto::chat_v1::channel_event;
use chatter_linux::services::ChannelEventService;

use crate::MainWindow;

/// Spawn a task that streams channel events and updates the UI.
///
/// Returns a handle to the spawned task.
pub fn spawn_channel_events_handler(
    weak_window: Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    channel_event_service: Arc<ChannelEventService>,
) -> tokio::task::JoinHandle<()> {
    let ui_ctx = ui_ctx.clone();

    tokio::spawn(async move {
        // Small delay to ensure connection is stable
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        loop {
            match run_channel_events_stream(
                &weak_window,
                &ui_ctx,
                &channel_event_service,
            )
            .await
            {
                Ok(()) => {
                    info!("Channel events stream ended normally");
                    break;
                }
                Err(e) => {
                    warn!("Channel events stream error: {}. Reconnecting in 5s...", e);
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            }
        }
    })
}

/// Run the channel events stream until it ends or errors.
async fn run_channel_events_stream(
    weak_window: &Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    channel_event_service: &Arc<ChannelEventService>,
) -> Result<(), String> {
    let mut channel_client = ChannelClient::new(
        ui_ctx.grpc_pool.channel(),
        ui_ctx.grpc_pool.auth_clone(),
    );

    // Stream events for all channels (pass None for channel_id)
    let mut stream = channel_client
        .stream_channel_events(None)
        .await
        .map_err(|e| format!("Failed to start channel events stream: {}", e))?;

    info!("Channel events stream started");

    while let Some(result) = stream.next().await {
        match result {
            Ok(event) => {
                if let Some(event_data) = event.event {
                    handle_channel_event(
                        event_data,
                        &event.channel_id,
                        event.timestamp,
                        weak_window,
                        ui_ctx,
                        channel_event_service,
                    )
                    .await;
                }
            }
            Err(e) => {
                error!("Channel event stream error: {}", e);
                return Err(format!("Stream error: {}", e));
            }
        }
    }

    Ok(())
}

/// Handle a single channel event.
async fn handle_channel_event(
    event: channel_event::Event,
    channel_id_str: &str,
    timestamp: u64,
    weak_window: &Weak<MainWindow>,
    ui_ctx: &Arc<UIContext>,
    channel_event_service: &Arc<ChannelEventService>,
) {
    match event {
        channel_event::Event::MemberAdded(member) => {
            let user_id_str = member.user_id.clone();
            debug!(
                channel_id = %channel_id_str,
                user_id = %user_id_str,
                "Received MemberAdded event"
            );

            // Create system message for the join
            if let (Ok(channel_id), Ok(user_id)) = (
                channel_id_str.parse::<ChannelId>(),
                user_id_str.parse::<UserId>(),
            ) {
                // Build membership from proto
                let membership = build_membership_from_proto(&channel_id, &member);

                // Use service to handle member joined (creates system message + syncs membership)
                match channel_event_service.handle_member_joined(&channel_id, &user_id, &membership) {
                    Ok((_message, display_name)) => {
                        // Update UI
                        update_ui_with_system_message(
                            weak_window,
                            &channel_id,
                            &format!("{} joined the channel", display_name),
                            timestamp,
                        );
                    }
                    Err(e) => {
                        warn!("Failed to handle member joined: {}", e);
                    }
                }

                // Send our SKDM to the new member so they can decrypt our messages
                let channel_id_clone = channel_id;
                let new_member_id = user_id;
                let signal_svc = ui_ctx.signal_service.clone();
                let msg_svc = ui_ctx.message_service.clone();

                tokio::spawn(async move {
                    // Check if we have a sender key for this channel
                    match signal_svc.has_sender_key(&channel_id_clone).await {
                        Ok(true) => {
                            // Create our SKDM for this channel
                            match signal_svc.create_sender_key(&channel_id_clone).await {
                                Ok(skdm) => {
                                    // Send SKDM to the new member pairwise
                                    if let Err(e) = msg_svc
                                        .send_skdm_to_member(&channel_id_clone, &new_member_id, &skdm)
                                        .await
                                    {
                                        warn!(
                                            channel_id = %channel_id_clone,
                                            new_member = %new_member_id,
                                            error = %e,
                                            "Failed to send SKDM to new member"
                                        );
                                    } else {
                                        info!(
                                            channel_id = %channel_id_clone,
                                            new_member = %new_member_id,
                                            "Sent SKDM to new channel member"
                                        );
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        channel_id = %channel_id_clone,
                                        error = %e,
                                        "Failed to create SKDM for new member"
                                    );
                                }
                            }
                        }
                        Ok(false) => {
                            debug!(
                                channel_id = %channel_id_clone,
                                "No sender key for channel, skipping SKDM send to new member"
                            );
                        }
                        Err(e) => {
                            warn!(
                                channel_id = %channel_id_clone,
                                error = %e,
                                "Failed to check sender key existence"
                            );
                        }
                    }
                });
            }
        }
        channel_event::Event::MemberRemovedId(user_id_str) => {
            debug!(
                channel_id = %channel_id_str,
                user_id = %user_id_str,
                "Received MemberRemoved event"
            );

            if let (Ok(channel_id), Ok(user_id)) = (
                channel_id_str.parse::<ChannelId>(),
                user_id_str.parse::<UserId>(),
            ) {
                // Use service to handle member removed (creates system message + removes membership)
                match channel_event_service.handle_member_removed(&channel_id, &user_id) {
                    Ok((_message, display_name)) => {
                        // Mark channel for sender key rotation (forward secrecy)
                        if let Err(e) = ui_ctx.signal_service.mark_channel_for_rotation(&channel_id) {
                            warn!(
                                channel_id = %channel_id,
                                error = %e,
                                "Failed to mark channel for sender key rotation"
                            );
                        } else {
                            info!(
                                channel_id = %channel_id,
                                departed_user = %user_id,
                                "Marked channel for sender key rotation after member left"
                            );
                        }

                        update_ui_with_system_message(
                            weak_window,
                            &channel_id,
                            &format!("{} left the channel", display_name),
                            timestamp,
                        );
                    }
                    Err(e) => {
                        warn!("Failed to handle member removed: {}", e);
                    }
                }
            }
        }
        channel_event::Event::ChannelUpdated(proto_channel) => {
            debug!(
                channel_id = %channel_id_str,
                channel_name = %proto_channel.name,
                "Received ChannelUpdated event"
            );

            // Update channel via service
            if let Ok(channel_id) = channel_id_str.parse::<ChannelId>() {
                // Fetch and update channel via service (syncs to local storage)
                if let Err(e) = ui_ctx.channel_service.get_channel(&channel_id).await {
                    warn!("Failed to sync channel update: {}", e);
                }

                // Update sidebar UI
                let weak = weak_window.clone();
                let channel_name = proto_channel.name.clone();
                let channel_id_str = channel_id.to_string();

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak.upgrade() {
                        // Update channel list in sidebar
                        let channels = window.get_channels();
                        for i in 0..channels.row_count() {
                            if let Some(ch) = channels.row_data(i) {
                                if ch.channel_id.as_str() == channel_id_str {
                                    use slint::VecModel;
                                    let mut updated_ch = ch.clone();
                                    updated_ch.name = channel_name.clone().into();
                                    if let Some(model) = channels.as_any().downcast_ref::<VecModel<crate::ChannelListItemData>>() {
                                        model.set_row_data(i, updated_ch);
                                    }
                                    break;
                                }
                            }
                        }

                        // Update header if this is the selected channel
                        if window.get_selected_channel_id().as_str() == channel_id_str {
                            window.set_channel_name(channel_name.into());
                        }
                    }
                });
            }
        }
        channel_event::Event::ChannelDeleted(deleted_channel_id) => {
            info!(
                channel_id = %deleted_channel_id,
                "Received ChannelDeleted event"
            );

            // Parse channel ID
            if let Ok(channel_id) = deleted_channel_id.parse::<ChannelId>() {
                // Delete channel via service
                if let Err(e) = ui_ctx.channel_service.delete_channel(&channel_id).await {
                    warn!("Failed to delete channel: {}", e);
                }

                // Remove from sidebar UI
                let weak = weak_window.clone();
                let deleted_id = deleted_channel_id.clone();

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(window) = weak.upgrade() {
                        // Remove from channel list
                        let channels = window.get_channels();
                        let channel_list: Vec<crate::ChannelListItemData> = (0..channels.row_count())
                            .filter_map(|i| channels.row_data(i))
                            .filter(|ch| ch.channel_id.as_str() != deleted_id)
                            .collect();

                        use slint::{ModelRc, VecModel};
                        window.set_channels(ModelRc::new(VecModel::from(channel_list.clone())));

                        // Also check DM list
                        let dms = window.get_dm_conversations();
                        let dm_list: Vec<crate::DMListItemData> = (0..dms.row_count())
                            .filter_map(|i| dms.row_data(i))
                            .filter(|dm| dm.channel_id.as_str() != deleted_id)
                            .collect();
                        window.set_dm_conversations(ModelRc::new(VecModel::from(dm_list)));

                        // If this was the selected channel, clear selection
                        if window.get_selected_channel_id().as_str() == deleted_id
                            || window.get_selected_dm_id().as_str() == deleted_id
                        {
                            window.set_selected_channel_id("".into());
                            window.set_selected_dm_id("".into());
                            window.set_channel_name("".into());
                            window.set_messages(ModelRc::new(VecModel::from(Vec::<crate::MessageItemData>::new())));

                            // Select first available channel if any
                            if !channel_list.is_empty() {
                                window.set_selected_channel_id(channel_list[0].channel_id.clone());
                                window.set_channel_name(channel_list[0].name.clone());
                            }
                        }
                    }
                });
            }
        }
        _ => {
            // Other events (invitations, moderation, etc.)
            debug!("Received other channel event type");
        }
    }
}

/// Update the UI with a system message for the current channel.
fn update_ui_with_system_message(
    weak_window: &Weak<MainWindow>,
    channel_id: &ChannelId,
    message_text: &str,
    timestamp: u64,
) {
    let channel_id = channel_id.clone();
    let message_text = message_text.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            // Only add to UI if this is the currently active channel
            let current_channel = window.get_selected_channel_id();
            if current_channel.as_str() == channel_id.to_string() {
                use crate::MessageItemData;
                use slint::{ModelRc, SharedString, VecModel};

                // Create system message item
                let system_message = MessageItemData {
                    message_id: uuid::Uuid::new_v4().to_string().into(),
                    sender_id: SharedString::default(), // No sender for system messages
                    sender_name: SharedString::default(),
                    sender_initials: SharedString::default(),
                    sender_avatar_url: SharedString::default(),
                    content: message_text.into(),
                    timestamp: super::inbox_events::format_timestamp(timestamp).into(),
                    timestamp_ns: timestamp.to_string().into(),
                    is_own: false,
                    is_edited: false,
                    is_deleted: false,
                    is_system: true,
                    is_ai: false,
                    has_attachments: false,
                    attachment_count: 0,
                    attachments: ModelRc::new(VecModel::from(Vec::new())),
                    has_reactions: false,
                    reaction_summary: SharedString::default(),
                    reply_count: 0,
                    reply_count_text: SharedString::default(),
                    is_thread_reply: false,
                    show_sender: false,
                    delivery_status: SharedString::default(),
                    is_pinned: false,
                    is_call: false,
                    call_type: SharedString::default(),
                    call_duration: SharedString::default(),
                };

                // Append to messages list
                let current_model = window.get_messages();
                let mut messages: Vec<_> = (0..current_model.row_count())
                    .filter_map(|i| current_model.row_data(i))
                    .collect();
                messages.push(system_message);
                window.set_messages(ModelRc::new(VecModel::from(messages)));
            }
        }
    });
}

/// Build a Membership domain object from a proto ChannelMember.
fn build_membership_from_proto(
    channel_id: &ChannelId,
    member: &chatter_linux::proto::chat_v1::channel::ChannelMember,
) -> Membership {
    let role = match member.role {
        1 => MemberRole::Member,
        2 => MemberRole::Moderator,
        3 => MemberRole::Admin,
        4 => MemberRole::Owner,
        _ => MemberRole::Member,
    };

    let user_id = member.user_id.parse::<UserId>().unwrap_or_else(|_| {
        UserId::new()
    });

    Membership {
        channel_id: channel_id.clone(),
        user_id,
        role,
        joined_at: chrono::DateTime::from_timestamp_millis(member.joined_at as i64)
            .unwrap_or_else(chrono::Utc::now),
        last_read_message_id: None,
        is_muted: member.muted,
        muted_until: None,
        notification_preference: NotificationPreference::All,
    }
}
