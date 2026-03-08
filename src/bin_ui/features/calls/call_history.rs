//! Call history message creation.
//!
//! Creates and stores call history messages in the database.

use std::sync::Arc;

use chrono::{Duration, Utc};
use uuid::Uuid;

use chatter_linux::domain::ids::{ChannelId, MessageId, UserId};
use chatter_linux::domain::message::{CallMessage, CallMessageType, Message, MessageContent};
use chatter_linux::services::message::MessageService;

use super::timers::current_time_ns;

/// Create and save a call history message to the database and UI.
///
/// # Arguments
/// * `message_service` - Message service for storing the message
/// * `channel_id` - Channel where the call occurred
/// * `call_id` - Unique call identifier
/// * `initiator_id` - User who initiated the call
/// * `initiator_name` - Display name of the initiator
/// * `is_outgoing` - True if we initiated the call
/// * `duration_secs` - Call duration in seconds (0 for missed calls)
/// * `is_missed` - True if the call was missed/declined
pub fn create_call_history_message(
    message_service: Arc<MessageService>,
    channel_id: &ChannelId,
    call_id: &str,
    initiator_id: &UserId,
    initiator_name: &str,
    is_outgoing: bool,
    duration_secs: i64,
    is_missed: bool,
) {
    // Create call message content
    let call_type = if is_missed {
        CallMessageType::Missed
    } else {
        CallMessageType::Ended
    };

    let duration = if duration_secs > 0 {
        Some(Duration::seconds(duration_secs))
    } else {
        None
    };

    let call_msg = CallMessage {
        call_id: call_id.to_string(),
        call_type,
        initiator_id: *initiator_id,
        initiator_name: initiator_name.to_string(),
        started_at: None, // We don't track this currently
        ended_at: Some(Utc::now()),
        duration,
        participant_count: 2, // DM calls have 2 participants
    };

    // Create the message
    let message = Message {
        id: MessageId(Uuid::new_v4()),
        channel_id: *channel_id,
        sender_id: Some(*initiator_id),
        content: MessageContent::Call(call_msg),
        thread_id: None,
        reply_to: None,
        attachments: vec![],
        is_pinned: false,
        is_edited: false,
        is_deleted: false,
        sent_at: current_time_ns(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        edited_at: None,
        reaction_counts: vec![],
        current_user_reactions: vec![],
        mentioned_integrations: vec![],
        targets_ai_bot: false,
    };

    // Save to database via service
    if let Err(e) = message_service.store_message_local(&message) {
        tracing::error!(error = %e, "Failed to save call history message");
    } else {
        tracing::info!(
            call_id = call_id,
            channel_id = %channel_id,
            is_outgoing = is_outgoing,
            is_missed = is_missed,
            duration_secs = duration_secs,
            "Saved call history message"
        );
    }
}
