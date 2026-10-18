use crate::domain::ids::{ChannelId, MessageId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// Thread
// ============================================================================

/// Message thread (conversation branch).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub root_message_id: MessageId,
    pub conversation_id: ChannelId,
    pub reply_count: u32,
    pub last_reply_at: Option<DateTime<Utc>>,
    pub participant_ids: Vec<UserId>,
}

impl Thread {
    /// Create a new thread from a root message.
    #[must_use]
    pub fn new(root_message_id: MessageId, conversation_id: ChannelId) -> Self {
        Self {
            root_message_id,
            conversation_id,
            reply_count: 0,
            last_reply_at: None,
            participant_ids: Vec::new(),
        }
    }

    /// Add a reply to the thread.
    pub fn add_reply(&mut self, participant_id: UserId) {
        self.reply_count += 1;
        self.last_reply_at = Some(Utc::now());
        if !self.participant_ids.contains(&participant_id) {
            self.participant_ids.push(participant_id);
        }
    }

    /// Check if the thread has any replies.
    #[must_use]
    pub fn has_replies(&self) -> bool {
        self.reply_count > 0
    }
}

/// Where a newly-received message belongs in an already-ordered list, or that
/// it's a duplicate. See [`ordered_insert_position`].
#[derive(Debug, PartialEq, Eq)]
pub enum InsertPosition {
    /// A message with the same `(timestamp_ns, sender_id)` is already present.
    Duplicate,
    /// Insert the new message at this index to keep the list ordered.
    At(usize),
}

/// Signal-style message ordering policy, kept in the domain so every renderer
/// applies the same rule instead of re-implementing it.
///
/// Messages are ordered by `timestamp_ns` ascending, ties broken by ascending
/// `sender_id`. Deduplication is by the full `(timestamp_ns, sender_id)` key.
///
/// `existing` must already be ordered by this same rule; each entry is that
/// row's `(timestamp_ns, sender_id)`. Returns [`InsertPosition::Duplicate`] if
/// the new key is already present, else the index to insert at.
pub fn ordered_insert_position(
    existing: &[(u64, &str)],
    new_ts: u64,
    new_sender: &str,
) -> InsertPosition {
    if existing
        .iter()
        .any(|(ts, s)| *ts == new_ts && *s == new_sender)
    {
        return InsertPosition::Duplicate;
    }
    let pos = existing
        .iter()
        .position(|(ts, s)| match ts.cmp(&new_ts) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => *s > new_sender,
            std::cmp::Ordering::Less => false,
        })
        .unwrap_or(existing.len());
    InsertPosition::At(pos)
}
