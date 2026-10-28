//! Call history data and routing flags for the client presenter.

use crate::domain::call::{CallDirection, CallHistoryEntry, CallResult};
use crate::state::ActiveConversation;

/// One row of the Calls screen.
///
/// Carries the source values for a `CallHistoryRow`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallHistoryRowData {
    /// Stable row id — the signalling call id / room name.
    pub id: String,
    /// Conversation to reopen when the row is tapped.
    pub conversation_id: String,
    /// `ConversationType::as_str` — the same vocabulary the sidebar's
    /// section rows route on.
    pub conversation_type: String,
    /// Conversation or peer name from the call record.
    pub name: String,
    /// The peer's avatar for a 1:1 call, the same source the chat list uses;
    /// `None` = initials. Filled by `ChatterClient::call_history_page`, which
    /// has the stores — the `From` conversion leaves it empty.
    pub avatar_bytes: Option<Vec<u8>>,
    /// A private channel's row: the UI draws its lock glyph, as the chat list
    /// does, instead of initials. Filled with the avatar.
    pub private_channel: bool,
    /// Raw call time and outcome for the presenter.
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub result: Option<CallResult>,
    pub duration_secs: Option<i64>,
    pub in_progress: bool,
    /// Missed calls are the reason this screen exists — the UI paints them.
    pub is_missed: bool,
    /// Video vs voice, which is also what a redial from this row places.
    pub is_video: bool,
    /// Direction, for the row's arrow.
    pub is_incoming: bool,
    /// Whether the conversation can still be reopened to redial. False for
    /// rows whose conversation key no longer resolves.
    pub can_call_back: bool,
}

impl From<&CallHistoryEntry> for CallHistoryRowData {
    fn from(entry: &CallHistoryEntry) -> Self {
        let is_incoming = entry.direction == CallDirection::Incoming;

        Self {
            id: entry.id.clone(),
            conversation_id: entry.conversation_id.clone(),
            conversation_type: entry.conversation_type.as_str().to_string(),
            avatar_bytes: None,
            private_channel: false,
            name: entry.peer_name.clone(),
            started_at: Some(entry.started_at),
            result: Some(entry.result),
            duration_secs: entry.duration_secs,
            in_progress: false,
            // An unanswered call we placed reads "No answer", not red.
            is_missed: is_incoming && entry.result == CallResult::Missed,
            is_video: entry.has_video,
            is_incoming,
            can_call_back: ActiveConversation::from_routing_key(
                &entry.conversation_type,
                &entry.conversation_id,
            )
            .is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::UserId;
    use crate::domain::message::ConversationType;

    fn entry(
        result: CallResult,
        direction: CallDirection,
        duration: Option<i64>,
    ) -> CallHistoryEntry {
        CallHistoryEntry {
            id: "call-1".into(),
            conversation_id: UserId::new().to_string(),
            conversation_type: ConversationType::ServerDm,
            peer_name: "Alice Smith".into(),
            started_at: chrono::Utc::now(),
            ended_at: None,
            direction,
            result,
            duration_secs: duration,
            has_video: false,
        }
    }

    #[test]
    fn call_history_row_carries_raw_outcome_and_routing() {
        let incoming = CallHistoryRowData::from(&entry(
            CallResult::Missed,
            CallDirection::Incoming,
            Some(30),
        ));
        assert_eq!(incoming.result, Some(CallResult::Missed));
        assert_eq!(incoming.duration_secs, Some(30));
        assert!(incoming.started_at.is_some());
        assert!(incoming.is_missed);
        let completed = CallHistoryRowData::from(&entry(
            CallResult::Completed,
            CallDirection::Outgoing,
            Some(222),
        ));
        assert_eq!(completed.name, "Alice Smith");
        assert_eq!(completed.conversation_type, "server_dm");
        assert!(completed.can_call_back);
    }
}
