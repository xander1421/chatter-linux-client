//! Calls-screen row data for Slint UI bindings.
//!
//! Maps a stored [`CallHistoryEntry`] to the strings the Calls list renders.
//! All of the shaping — direction wording, duration formatting, whether the
//! row can be redialled — happens here so the UI crate stays a renderer.

use crate::domain::call::{CallDirection, CallHistoryEntry, CallResult};
use crate::state::ActiveConversation;
use crate::ui::chat_data::{initials_from_name, sidebar_time_at};

/// One row of the Calls screen.
///
/// Maps to the `CallHistoryRow` struct in `ui/calls/types.slint`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallHistoryRowData {
    /// Stable row id — the signalling call id / room name.
    pub id: String,
    /// Conversation to reopen when the row is tapped.
    pub conversation_id: String,
    /// `ConversationType::as_str` — the same vocabulary the sidebar's
    /// section rows route on.
    pub conversation_type: String,
    /// Conversation or peer display name. Never empty: falls back to
    /// "Unknown" so a row is always identifiable.
    pub name: String,
    /// Avatar fallback initials for `name`.
    pub initials: String,
    /// The peer's avatar for a 1:1 call, the same source the chat list uses;
    /// `None` = initials. Filled by `ChatterClient::call_history_page`, which
    /// has the stores — the `From` conversion leaves it empty.
    pub avatar_bytes: Option<Vec<u8>>,
    /// When the call happened, in the chat list's short form ("14:03",
    /// "Yesterday", "Mon", "Sep 24").
    pub timestamp: String,
    /// Outcome line: "Outgoing · 3:42", "Missed", "Declined".
    pub detail: String,
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
        let name = if entry.peer_name.is_empty() {
            "Unknown".to_string()
        } else {
            entry.peer_name.clone()
        };
        let is_incoming = entry.direction == CallDirection::Incoming;

        Self {
            id: entry.id.clone(),
            conversation_id: entry.conversation_id.clone(),
            conversation_type: entry.conversation_type.as_str().to_string(),
            initials: initials_from_name(&name),
            avatar_bytes: None,
            name,
            timestamp: sidebar_time_at(entry.started_at, chrono::Local::now()),
            detail: detail_line(entry, is_incoming),
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

/// The second line of a row: outcome, plus duration for a call that
/// connected. An unanswered call's ring time is not a call length.
fn detail_line(entry: &CallHistoryEntry, is_incoming: bool) -> String {
    let outcome = match entry.result {
        CallResult::Missed if is_incoming => "Missed",
        CallResult::Missed => "No answer",
        CallResult::Declined => "Declined",
        CallResult::Failed => "Failed",
        CallResult::Completed if is_incoming => "Incoming",
        CallResult::Completed => "Outgoing",
    };
    match entry.duration_secs {
        Some(secs) if secs > 0 && entry.result == CallResult::Completed => format!("{} \u{00B7} {}", outcome, format_call_duration(secs)),
        _ => outcome.to_string(),
    }
}

/// `M:SS`, or `H:MM:SS` once a call runs past the hour.
#[must_use]
pub fn format_call_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::UserId;
    use crate::domain::message::ConversationType;

    fn entry(result: CallResult, direction: CallDirection, duration: Option<i64>) -> CallHistoryEntry {
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
    fn duration_formats_across_the_hour_boundary() {
        assert_eq!(format_call_duration(0), "0:00");
        assert_eq!(format_call_duration(42), "0:42");
        assert_eq!(format_call_duration(222), "3:42");
        assert_eq!(format_call_duration(3735), "1:02:15");
    }

    #[test]
    fn detail_line_says_missed_only_for_incoming() {
        let incoming = entry(CallResult::Missed, CallDirection::Incoming, None);
        assert_eq!(CallHistoryRowData::from(&incoming).detail, "Missed");
        assert!(CallHistoryRowData::from(&incoming).is_missed);

        let outgoing = entry(CallResult::Missed, CallDirection::Outgoing, None);
        assert_eq!(CallHistoryRowData::from(&outgoing).detail, "No answer");
    }

    #[test]
    fn unconnected_rows_never_show_a_duration() {
        let missed = entry(CallResult::Missed, CallDirection::Incoming, Some(30));
        assert_eq!(CallHistoryRowData::from(&missed).detail, "Missed");
        let no_answer = entry(CallResult::Missed, CallDirection::Outgoing, Some(28));
        assert_eq!(CallHistoryRowData::from(&no_answer).detail, "No answer");
    }

    #[test]
    fn completed_row_carries_duration_and_is_redialable() {
        let row = CallHistoryRowData::from(&entry(
            CallResult::Completed,
            CallDirection::Outgoing,
            Some(222),
        ));
        assert_eq!(row.detail, "Outgoing \u{00B7} 3:42");
        assert_eq!(row.initials, "AS");
        assert_eq!(row.conversation_type, "server_dm");
        assert!(row.can_call_back);
    }
}
