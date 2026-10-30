//! Message content extraction and channel grouping metadata.

use crate::domain::message::MessageContent;

// ============================================================================
// Helper Functions
// ============================================================================

/// The fields needed to decide whether a channel row joins the preceding row.
/// Day labels are the same ones used for date chips in the message list.
pub struct ChannelGroupingRow<'a> {
    pub sender_id: &'a str,
    pub timestamp_ns: u64,
    pub day_label: &'a str,
    pub is_system: bool,
    pub is_call: bool,
    pub is_deleted: bool,
}

/// A later message hides its sender header only when it directly follows a
/// visible message from the same sender on the same local day, within 5 minutes.
#[must_use]
pub fn channel_message_is_continuation(
    previous: Option<&ChannelGroupingRow<'_>>,
    current: &ChannelGroupingRow<'_>,
) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    if previous.is_system
        || previous.is_call
        || previous.is_deleted
        || current.is_system
        || current.is_call
        || current.is_deleted
        || previous.sender_id.is_empty()
        || previous.sender_id != current.sender_id
        || previous.timestamp_ns == 0
        || current.timestamp_ns == 0
        || previous.day_label.is_empty()
        || previous.day_label != current.day_label
    {
        return false;
    }
    current
        .timestamp_ns
        .checked_sub(previous.timestamp_ns)
        .is_some_and(|elapsed| elapsed <= 5 * 60 * 1_000_000_000)
}

/// Shape a visible channel page, including a newly prepended or removed row's
/// neighbour. Each flag belongs to the row at the same index.
#[must_use]
pub fn channel_continuation_flags(rows: &[ChannelGroupingRow<'_>]) -> Vec<bool> {
    rows.iter()
        .enumerate()
        .map(|(index, current)| {
            channel_message_is_continuation(index.checked_sub(1).map(|i| &rows[i]), current)
        })
        .collect()
}

/// Extract display content from a message.
///
/// Returns the text to display for a message, handling different content types.
#[must_use]
pub fn extract_content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Mixed(mixed) => mixed.text.clone().unwrap_or_default(),
        MessageContent::AI(ai) => ai.text.clone(),
        MessageContent::System(sys) => sys.description(),
        MessageContent::Call(call) => match call.call_type {
            crate::domain::message::CallMessageType::Started => {
                format!("{} started a call", call.initiator_name)
            }
            crate::domain::message::CallMessageType::Ended => {
                let duration = call.duration_display().unwrap_or_default();
                if duration.is_empty() {
                    "Call ended".to_string()
                } else {
                    format!("Call ended ({duration})")
                }
            }
            crate::domain::message::CallMessageType::Missed => {
                format!("Missed call from {}", call.initiator_name)
            }
            crate::domain::message::CallMessageType::Declined => {
                format!("Declined call from {}", call.initiator_name)
            }
        },
        MessageContent::Encrypted(_) => "[Encrypted message]".to_string(),
        MessageContent::SealedSender(_) => "[Encrypted message]".to_string(),
    }
}

/// Raw call participant identity and avatar resolved from local state.
#[derive(Clone, Debug, Default)]
pub struct CallParticipantInfo {
    pub display_name: String,
    pub avatar_bytes: Option<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SystemMessage;

    #[test]
    fn test_extract_content_text() {
        // Text content
        let text = MessageContent::Text("Hello".to_string());
        assert_eq!(extract_content_text(&text), "Hello");

        // System message
        let system = MessageContent::System(SystemMessage::ChannelCreated);
        assert_eq!(extract_content_text(&system), "created the channel");

        // Encrypted
        let encrypted = MessageContent::Encrypted(crate::domain::message::EncryptedContent::new(
            vec![1, 2, 3],
            crate::domain::message::EncryptedMessageType::Signal,
        ));
        assert_eq!(extract_content_text(&encrypted), "[Encrypted message]");
    }
}
