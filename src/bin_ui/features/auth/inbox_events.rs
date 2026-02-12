//! Inbox event handling for UI updates.
//!
//! Processes incoming DM and channel events to update the UI in real-time.

use slint::{Model, ModelRc, VecModel};

use chatter_linux::inbox::InboxEvent;
use chatter_linux::storage::UserRepository;

use crate::bin_ui::converters::get_initials;
use crate::{DMListItemData, MainWindow, MessageItemData};

/// Update a reaction summary string when a reaction is added or removed.
///
/// The summary format is "👍 3  ❤️ 2" (emoji space count, double-space separated).
pub fn update_reaction_summary(current: &str, emoji: &str, remove: bool) -> String {
    use std::collections::HashMap;

    // Parse current summary into map of emoji -> count
    let mut reactions: HashMap<String, i32> = HashMap::new();

    for part in current.split("  ") {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Format is "emoji count" or just "emoji" (count=1)
        let mut chars = part.chars();
        let mut emoji_chars = String::new();
        let mut count_str = String::new();

        // Collect emoji (may be multiple chars for complex emoji)
        for c in chars.by_ref() {
            if c.is_ascii_digit() || c == ' ' {
                if c.is_ascii_digit() {
                    count_str.push(c);
                }
                break;
            }
            emoji_chars.push(c);
        }
        // Collect remaining digits
        for c in chars {
            if c.is_ascii_digit() {
                count_str.push(c);
            }
        }

        let count = count_str.parse::<i32>().unwrap_or(1);
        if !emoji_chars.is_empty() {
            reactions.insert(emoji_chars, count);
        }
    }

    // Update the count
    if remove {
        if let Some(count) = reactions.get_mut(emoji) {
            *count -= 1;
            if *count <= 0 {
                reactions.remove(emoji);
            }
        }
    } else {
        *reactions.entry(emoji.to_string()).or_insert(0) += 1;
    }

    // Rebuild the summary string
    if reactions.is_empty() {
        return String::new();
    }

    reactions
        .iter()
        .map(|(e, c)| {
            if *c == 1 {
                e.clone()
            } else {
                format!("{} {}", e, c)
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Format a millisecond timestamp for display.
pub fn format_timestamp(timestamp_ms: u64) -> String {
    use chrono::{Local, TimeZone};

    if timestamp_ms == 0 {
        return "".to_string();
    }

    let secs = (timestamp_ms / 1000) as i64;
    let nsecs = ((timestamp_ms % 1000) * 1_000_000) as u32;

    match Local.timestamp_opt(secs, nsecs).single() {
        Some(dt) => {
            let now = Local::now();
            if dt.date_naive() == now.date_naive() {
                dt.format("%H:%M").to_string()
            } else {
                dt.format("%b %d, %H:%M").to_string()
            }
        }
        None => "".to_string(),
    }
}

/// Handle inbox events and update UI accordingly.
pub fn handle_inbox_event(
    event: InboxEvent,
    weak_window: &slint::Weak<MainWindow>,
    user_repo: &UserRepository,
) {
    match event {
        InboxEvent::DmReceived {
            peer_id,
            text,
            timestamp,
            message_id,
        } => {
            handle_dm_received(weak_window, user_repo, peer_id, text, timestamp, message_id);
        }
        InboxEvent::DmDecryptionFailed { peer_id, error } => {
            tracing::error!("DM decryption failed from {}: {}", peer_id, error);
        }
        InboxEvent::DmPollCompleted { total_messages } => {
            if total_messages > 0 {
                tracing::debug!("DM poll completed: {} messages", total_messages);
            }
        }
        InboxEvent::ChannelMessageReceived {
            channel_id,
            sender_id,
            message_id,
            text,
            timestamp,
        } => {
            handle_channel_message_received(
                weak_window,
                user_repo,
                channel_id,
                sender_id,
                message_id,
                text,
                timestamp,
            );
        }
        InboxEvent::MessageReceived { message_id, .. } => {
            tracing::trace!("Inbox message received: {}", message_id);
        }
        InboxEvent::MessageProcessed { message_id } => {
            tracing::trace!("Inbox message processed: {}", message_id);
        }
        InboxEvent::MessageFailed { message_id, error } => {
            tracing::warn!("Inbox message failed: {} - {}", message_id, error);
        }
        InboxEvent::PollCompleted { count, .. } => {
            if count > 0 {
                tracing::trace!("Inbox poll completed: {} messages", count);
            }
        }
        InboxEvent::PollFailed { error } => {
            tracing::warn!("Inbox poll failed: {}", error);
        }
        InboxEvent::AcknowledgeFailed { message_ids, error } => {
            tracing::warn!("Inbox ack failed for {:?}: {}", message_ids, error);
        }
        InboxEvent::DmUnreadCountUpdated {
            channel_id,
            peer_id,
            unread_count,
        } => {
            handle_dm_unread_count(weak_window, channel_id, peer_id, unread_count);
        }
        InboxEvent::DmTyping { peer_id, action } => {
            handle_dm_typing(weak_window, user_repo, peer_id, action);
        }
        InboxEvent::DmReceipt {
            peer_id,
            receipt_type,
            timestamps,
        } => {
            handle_dm_receipt(weak_window, peer_id, receipt_type, timestamps);
        }
        InboxEvent::DmReaction {
            peer_id,
            emoji,
            remove,
            target_sent_timestamp,
            ..
        } => {
            handle_dm_reaction(weak_window, peer_id, emoji, remove, target_sent_timestamp);
        }
        InboxEvent::DmPin {
            peer_id,
            target_sent_timestamp,
            unpin,
            ..
        } => {
            handle_dm_pin(weak_window, peer_id, target_sent_timestamp, unpin);
        }
        InboxEvent::DmEdit {
            peer_id,
            target_sent_timestamp,
            new_body,
        } => {
            handle_dm_edit(weak_window, peer_id, target_sent_timestamp, new_body);
        }
        InboxEvent::DmDeleteForMe {
            conversation_id,
            target_sent_timestamp,
            ..
        } => {
            handle_dm_delete_for_me(weak_window, conversation_id, target_sent_timestamp);
        }
        InboxEvent::DmDeleteForEveryone {
            peer_id,
            target_sent_timestamp,
        } => {
            handle_dm_delete_for_everyone(weak_window, peer_id, target_sent_timestamp);
        }
        InboxEvent::ChannelTyping {
            channel_id,
            sender_id,
            action,
        } => {
            handle_channel_typing(weak_window, user_repo, channel_id, sender_id, action);
        }
        InboxEvent::ChannelReaction {
            channel_id,
            emoji,
            remove,
            target_sent_timestamp,
            ..
        } => {
            handle_channel_reaction(weak_window, channel_id, emoji, remove, target_sent_timestamp);
        }
        InboxEvent::ChannelPin {
            channel_id,
            target_sent_timestamp,
            unpin,
            ..
        } => {
            handle_channel_pin(weak_window, channel_id, target_sent_timestamp, unpin);
        }
        InboxEvent::ChannelEdit {
            channel_id,
            target_sent_timestamp,
            new_body,
            ..
        } => {
            handle_channel_edit(weak_window, channel_id, target_sent_timestamp, new_body);
        }
        InboxEvent::ChannelDeleteForEveryone {
            channel_id,
            target_sent_timestamp,
            ..
        } => {
            handle_channel_delete_for_everyone(weak_window, channel_id, target_sent_timestamp);
        }
        InboxEvent::ChannelDeleteForMe {
            conversation_id,
            target_sent_timestamp,
            ..
        } => {
            handle_channel_delete_for_me(weak_window, conversation_id, target_sent_timestamp);
        }
        InboxEvent::GroupMailboxPollCompleted {
            total_groups,
            total_messages,
        } => {
            tracing::trace!(
                total_groups,
                total_messages,
                "Group mailbox poll completed"
            );
        }
        InboxEvent::GroupMailboxAnnouncement {
            channel_id,
            member_id,
            ..
        } => {
            tracing::debug!(
                channel_id = %channel_id,
                member_id = %member_id,
                "Group mailbox announcement received"
            );
        }
        InboxEvent::FriendRequestsUpdated { received, sent } => {
            tracing::debug!(
                received_count = received.len(),
                sent_count = sent.len(),
                "Friend requests updated"
            );
            // Update UI with friend requests
            let weak = weak_window.clone();
            let received_ui: Vec<crate::FriendRequestData> = received
                .into_iter()
                .map(|r| crate::FriendRequestData {
                    user_id: r.user_id.into(),
                    display_name: r.display_name.into(),
                    username: r.username.into(),
                    avatar_url: r.avatar_url.into(),
                    sent_at: "".into(),
                })
                .collect();
            let sent_ui: Vec<crate::FriendRequestData> = sent
                .into_iter()
                .map(|r| crate::FriendRequestData {
                    user_id: r.user_id.into(),
                    display_name: r.display_name.into(),
                    username: r.username.into(),
                    avatar_url: r.avatar_url.into(),
                    sent_at: "".into(),
                })
                .collect();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_friends_received_requests(slint::ModelRc::new(slint::VecModel::from(received_ui)));
                    w.set_friends_sent_requests(slint::ModelRc::new(slint::VecModel::from(sent_ui)));
                }
            });
        }
        InboxEvent::FriendAccepted {
            friend_user_id,
            channel_id,
        } => {
            tracing::info!(
                friend_user_id = %friend_user_id,
                channel_id = %channel_id,
                "Friend accepted event received - DM channel available"
            );
            // The DM will be added to sidebar via the user events stream handler
            // which triggers this event. The actual sidebar update happens in
            // friends.rs add_dm_to_sidebar when channel_id is received.
            // This event is for logging/future use (e.g., showing a toast notification).
        }
    }
}

// =============================================================================
// DM Event Handlers
// =============================================================================

fn handle_dm_received(
    weak_window: &slint::Weak<MainWindow>,
    user_repo: &UserRepository,
    peer_id: chatter_linux::domain::UserId,
    text: String,
    timestamp: u64,
    message_id: String,
) {
    tracing::info!("DM received from {}: {} chars", peer_id, text.len());

    let sender_name = user_repo
        .get(&peer_id)
        .ok()
        .flatten()
        .map(|u| u.username.clone())
        .unwrap_or_else(|| peer_id.to_string());

    let weak = weak_window.clone();
    let peer_id_str = peer_id.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();

        if selected_dm == peer_id_str {
            // Add message to current view
            let current_model = window.get_messages();
            let mut messages: Vec<_> = (0..current_model.row_count())
                .filter_map(|i| current_model.row_data(i))
                .collect();

            let initials = get_initials(&sender_name);
            let timestamp_str = format_timestamp(timestamp);

            let preview = if text.len() > 50 {
                format!("{}...", &text[..47])
            } else {
                text.clone()
            };

            let msg_item = MessageItemData {
                message_id: message_id.into(),
                sender_id: peer_id_str.clone().into(),
                sender_name: sender_name.into(),
                sender_initials: initials.into(),
                sender_avatar_url: "".into(),
                content: text.into(),
                timestamp: timestamp_str.clone().into(),
                timestamp_ms: timestamp as i32,
                is_own: false,
                is_edited: false,
                is_deleted: false,
                is_system: false,
                is_ai: false,
                has_attachments: false,
                attachment_count: 0,
                attachments: ModelRc::new(VecModel::from(Vec::new())),
                has_reactions: false,
                reaction_summary: "".into(),
                reply_count: 0,
                reply_count_text: "".into(),
                is_thread_reply: false,
                show_sender: true,
                delivery_status: "".into(),
            };

            messages.push(msg_item);
            window.set_messages(ModelRc::new(VecModel::from(messages)));

            // Update sidebar
            update_dm_sidebar(&window, &peer_id_str, &preview, &timestamp_str);
        } else {
            // Update sidebar only
            let timestamp_str = format_timestamp(timestamp);
            let preview = if text.len() > 50 {
                format!("{}...", &text[..47])
            } else {
                text
            };
            update_dm_sidebar(&window, &peer_id_str, &preview, &timestamp_str);
        }
    });
}

fn update_dm_sidebar(window: &MainWindow, peer_id_str: &str, preview: &str, timestamp_str: &str) {
    let dms = window.get_dm_conversations();
    for i in 0..dms.row_count() {
        if let Some(dm) = dms.row_data(i) {
            if dm.user_id.as_str() == peer_id_str {
                let mut updated_dm = dm.clone();
                updated_dm.last_message = preview.into();
                updated_dm.timestamp = timestamp_str.into();

                if let Some(model) = dms.as_any().downcast_ref::<VecModel<DMListItemData>>() {
                    model.set_row_data(i, updated_dm);
                }
                break;
            }
        }
    }
}

fn handle_dm_unread_count(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    peer_id: chatter_linux::domain::UserId,
    unread_count: u32,
) {
    tracing::debug!(
        "DM unread count updated: {} (peer {}) = {}",
        channel_id,
        peer_id,
        unread_count
    );

    if let Some(window) = weak_window.upgrade() {
        let peer_id_str = peer_id.to_string();
        let dms = window.get_dm_conversations();

        for i in 0..dms.row_count() {
            if let Some(dm) = dms.row_data(i) {
                if dm.user_id.as_str() == peer_id_str {
                    let mut updated_dm = dm.clone();
                    updated_dm.unread_count = unread_count as i32;

                    if let Some(model) = dms.as_any().downcast_ref::<VecModel<DMListItemData>>() {
                        model.set_row_data(i, updated_dm);
                    }
                    break;
                }
            }
        }
    }
}

fn handle_dm_typing(
    weak_window: &slint::Weak<MainWindow>,
    user_repo: &UserRepository,
    peer_id: chatter_linux::domain::UserId,
    action: i32,
) {
    tracing::debug!(peer_id = %peer_id, action, "Received typing indicator");

    let peer_name = user_repo
        .get(&peer_id)
        .ok()
        .flatten()
        .map(|u| u.username.clone())
        .unwrap_or_else(|| "Someone".to_string());

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        if selected_dm != peer_id_str {
            return;
        }

        if action == 1 {
            window.set_typing_indicator(format!("{} is typing...", peer_name).into());
        } else {
            window.set_typing_indicator("".into());
        }
    });
}

fn handle_dm_receipt(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    receipt_type: i32,
    timestamps: Vec<u64>,
) {
    tracing::debug!(
        peer_id = %peer_id,
        receipt_type,
        count = timestamps.len(),
        "Received read/delivery receipt"
    );

    let status = match receipt_type {
        1 => "delivered",
        2 | 3 => "read",
        _ => "sent",
    };

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        if selected_dm != peer_id_str {
            return;
        }

        let messages = window.get_messages();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if !msg.is_own {
                    continue;
                }

                let msg_ts = msg.timestamp_ms as u64;
                if timestamps.contains(&msg_ts) {
                    let mut updated_msg = msg.clone();
                    updated_msg.delivery_status = status.into();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                }
            }
        }
    });
}

fn handle_dm_reaction(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    emoji: String,
    remove: bool,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        peer_id = %peer_id,
        emoji = %emoji,
        remove,
        target = target_sent_timestamp,
        "Received reaction"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        if selected_dm != peer_id_str {
            return;
        }

        let messages = window.get_messages();
        let target_ts = target_sent_timestamp as i32;

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ms == target_ts {
                    let mut updated_msg = msg.clone();
                    let new_summary =
                        update_reaction_summary(&msg.reaction_summary.to_string(), &emoji, remove);
                    updated_msg.reaction_summary = new_summary.into();
                    updated_msg.has_reactions = !updated_msg.reaction_summary.is_empty();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

fn handle_dm_pin(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    target_sent_timestamp: u64,
    unpin: bool,
) {
    tracing::debug!(
        peer_id = %peer_id,
        target = target_sent_timestamp,
        unpin,
        "Received pin/unpin"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        if selected_dm != peer_id_str {
            return;
        }

        let messages = window.get_messages();
        let target_ts = target_sent_timestamp as i32;
        let mut preview = String::new();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ms == target_ts {
                    preview = msg.content.to_string();
                    if preview.len() > 100 {
                        preview = format!("{}...", &preview[..97]);
                    }
                    break;
                }
            }
        }

        let current_count = window.get_pinned_total_count();
        if unpin {
            let new_count = (current_count - 1).max(0);
            window.set_pinned_total_count(new_count);
            window.set_has_pinned_messages(new_count > 0);
            if new_count == 0 {
                window.set_pinned_message_preview("".into());
            }
        } else {
            let new_count = current_count + 1;
            window.set_pinned_total_count(new_count);
            window.set_has_pinned_messages(true);
            if !preview.is_empty() {
                window.set_pinned_message_preview(preview.into());
                window.set_pinned_current_index(new_count);
            }
        }
    });
}

fn handle_dm_edit(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    target_sent_timestamp: u64,
    new_body: String,
) {
    tracing::debug!(
        peer_id = %peer_id,
        target = target_sent_timestamp,
        new_len = new_body.len(),
        "Received message edit"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        if selected_dm != peer_id_str {
            return;
        }

        let messages = window.get_messages();
        let target_ts = target_sent_timestamp as i32;

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ms == target_ts {
                    let mut updated_msg = msg.clone();
                    updated_msg.content = new_body.clone().into();
                    updated_msg.is_edited = true;

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

fn handle_dm_delete_for_me(
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: String,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        conversation_id = %conversation_id,
        target = target_sent_timestamp,
        "Received delete-for-me sync"
    );

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        if selected_dm != conversation_id {
            return;
        }

        let messages = window.get_messages();
        let target_ts = target_sent_timestamp as i32;

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ms == target_ts {
                    let mut updated_msg = msg.clone();
                    updated_msg.is_deleted = true;
                    updated_msg.content = "Message deleted".into();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

fn handle_dm_delete_for_everyone(
    weak_window: &slint::Weak<MainWindow>,
    peer_id: chatter_linux::domain::UserId,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        peer_id = %peer_id,
        target = target_sent_timestamp,
        "Received delete-for-everyone from peer"
    );

    let peer_id_str = peer_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_dm = window.get_selected_dm_id().to_string();
        if selected_dm != peer_id_str {
            return;
        }

        let messages = window.get_messages();
        let target_ts = target_sent_timestamp as i32;

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ms == target_ts {
                    let mut updated_msg = msg.clone();
                    updated_msg.is_deleted = true;
                    updated_msg.content = "This message was deleted".into();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

// =============================================================================
// Channel Event Handlers
// =============================================================================

fn handle_channel_message_received(
    weak_window: &slint::Weak<MainWindow>,
    user_repo: &UserRepository,
    channel_id: chatter_linux::domain::ids::ChannelId,
    sender_id: chatter_linux::domain::UserId,
    message_id: String,
    text: String,
    timestamp: u64,
) {
    tracing::info!(
        channel_id = %channel_id,
        sender_id = %sender_id,
        "Channel message received: {} chars",
        text.len()
    );

    let sender_name = user_repo
        .get(&sender_id)
        .ok()
        .flatten()
        .map(|u| u.username.clone())
        .unwrap_or_else(|| sender_id.to_string());

    let weak = weak_window.clone();
    let channel_id_str = channel_id.to_string();
    let sender_id_str = sender_id.to_string();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();

        if selected_channel == channel_id_str {
            let current_model = window.get_messages();
            let mut messages: Vec<_> = (0..current_model.row_count())
                .filter_map(|i| current_model.row_data(i))
                .collect();

            let initials = get_initials(&sender_name);
            let timestamp_str = format_timestamp(timestamp);

            let msg_item = MessageItemData {
                message_id: message_id.into(),
                sender_id: sender_id_str.into(),
                sender_name: sender_name.into(),
                sender_initials: initials.into(),
                sender_avatar_url: "".into(),
                content: text.into(),
                timestamp: timestamp_str.into(),
                timestamp_ms: timestamp as i32,
                is_own: false,
                is_edited: false,
                is_deleted: false,
                is_system: false,
                is_ai: false,
                has_attachments: false,
                attachment_count: 0,
                attachments: ModelRc::new(VecModel::from(Vec::new())),
                has_reactions: false,
                reaction_summary: "".into(),
                reply_count: 0,
                reply_count_text: "".into(),
                is_thread_reply: false,
                show_sender: true,
                delivery_status: "".into(),
            };

            messages.push(msg_item);
            window.set_messages(ModelRc::new(VecModel::from(messages)));

            tracing::debug!("Added channel message to current view");
        }
    });
}

fn handle_channel_typing(
    weak_window: &slint::Weak<MainWindow>,
    user_repo: &UserRepository,
    channel_id: chatter_linux::domain::ids::ChannelId,
    sender_id: chatter_linux::domain::UserId,
    action: i32,
) {
    tracing::debug!(
        channel_id = %channel_id,
        sender_id = %sender_id,
        action,
        "Received channel typing indicator"
    );

    let sender_name = user_repo
        .get(&sender_id)
        .ok()
        .flatten()
        .map(|u| u.username.clone())
        .unwrap_or_else(|| "Someone".to_string());

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
            return;
        }

        if action == 1 {
            window.set_typing_indicator(format!("{} is typing...", sender_name).into());
        } else {
            window.set_typing_indicator("".into());
        }
    });
}

fn handle_channel_reaction(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    emoji: String,
    remove: bool,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        channel_id = %channel_id,
        emoji = %emoji,
        remove,
        target = target_sent_timestamp,
        "Received channel reaction"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
            return;
        }

        let messages = window.get_messages();
        let target_ts = target_sent_timestamp as i32;

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ms == target_ts {
                    let mut updated_msg = msg.clone();
                    let new_summary =
                        update_reaction_summary(&msg.reaction_summary.to_string(), &emoji, remove);
                    updated_msg.reaction_summary = new_summary.into();
                    updated_msg.has_reactions = !updated_msg.reaction_summary.is_empty();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

fn handle_channel_pin(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    target_sent_timestamp: u64,
    unpin: bool,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        unpin,
        "Received channel pin/unpin"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
            return;
        }

        let messages = window.get_messages();
        let target_ts = target_sent_timestamp as i32;
        let mut preview = String::new();

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ms == target_ts {
                    preview = msg.content.to_string();
                    if preview.len() > 100 {
                        preview = format!("{}...", &preview[..97]);
                    }
                    break;
                }
            }
        }

        let current_count = window.get_pinned_total_count();
        if unpin {
            let new_count = (current_count - 1).max(0);
            window.set_pinned_total_count(new_count);
            window.set_has_pinned_messages(new_count > 0);
            if new_count == 0 {
                window.set_pinned_message_preview("".into());
            }
        } else {
            let new_count = current_count + 1;
            window.set_pinned_total_count(new_count);
            window.set_has_pinned_messages(true);
            if !preview.is_empty() {
                window.set_pinned_message_preview(preview.into());
                window.set_pinned_current_index(new_count);
            }
        }
    });
}

fn handle_channel_edit(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    target_sent_timestamp: u64,
    new_body: String,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        new_len = new_body.len(),
        "Received channel message edit"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
            return;
        }

        let messages = window.get_messages();
        let target_ts = target_sent_timestamp as i32;

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ms == target_ts {
                    let mut updated_msg = msg.clone();
                    updated_msg.content = new_body.clone().into();
                    updated_msg.is_edited = true;

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

fn handle_channel_delete_for_everyone(
    weak_window: &slint::Weak<MainWindow>,
    channel_id: chatter_linux::domain::ids::ChannelId,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        channel_id = %channel_id,
        target = target_sent_timestamp,
        "Received channel delete-for-everyone"
    );

    let channel_id_str = channel_id.to_string();
    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != channel_id_str {
            return;
        }

        let messages = window.get_messages();
        let target_ts = target_sent_timestamp as i32;

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ms == target_ts {
                    let mut updated_msg = msg.clone();
                    updated_msg.is_deleted = true;
                    updated_msg.content = "This message was deleted".into();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}

fn handle_channel_delete_for_me(
    weak_window: &slint::Weak<MainWindow>,
    conversation_id: String,
    target_sent_timestamp: u64,
) {
    tracing::debug!(
        conversation_id = %conversation_id,
        target = target_sent_timestamp,
        "Received channel delete-for-me sync"
    );

    let weak = weak_window.clone();

    let _ = slint::invoke_from_event_loop(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };

        let selected_channel = window.get_selected_channel_id().to_string();
        if selected_channel != conversation_id {
            return;
        }

        let messages = window.get_messages();
        let target_ts = target_sent_timestamp as i32;

        for i in 0..messages.row_count() {
            if let Some(msg) = messages.row_data(i) {
                if msg.timestamp_ms == target_ts {
                    let mut updated_msg = msg.clone();
                    updated_msg.is_deleted = true;
                    updated_msg.content = "Message deleted".into();

                    if let Some(model) = messages.as_any().downcast_ref::<VecModel<MessageItemData>>()
                    {
                        model.set_row_data(i, updated_msg);
                    }
                    return;
                }
            }
        }
    });
}
