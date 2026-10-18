use std::collections::HashMap;
use std::time::Instant;

use crate::domain::ids::{ChannelId, MessageId, UserId};

use super::ChatState;

// ============================================================================
// Streaming ephemera (typing indicators, reactions)
// ============================================================================

/// Simplified reaction data for UI display.
#[derive(Clone, Debug, PartialEq)]
pub struct ReactionSummary {
    /// The emoji for this reaction.
    pub emoji: String,
    /// Number of users who reacted with this emoji.
    pub count: u32,
    /// Whether the current user has reacted with this emoji.
    pub user_reacted: bool,
}

/// Streaming state data for real-time updates (typing, reactions).
///
/// Lives inside [`super::ChatStateData`] as `streaming`. The `TypingAggregator` and
/// `ReactionAggregator` in the network layer handle raw stream processing;
/// services bridge from those aggregators into this substate.
#[derive(Clone, Debug, Default)]
pub struct StreamingSubstate {
    /// Who is typing per channel.
    pub typing_by_channel: HashMap<ChannelId, Vec<UserId>>,
    /// Recent reactions per message (limited cache for display).
    pub recent_reactions: HashMap<MessageId, Vec<ReactionSummary>>,
    /// When typing state was last updated (for staleness detection).
    pub last_typing_update: Option<Instant>,
}

impl ChatState {
    // ========================================================================
    // Streaming ephemera: typing indicators
    // ========================================================================

    /// Get users currently typing in a channel.
    #[must_use]
    pub fn get_typing_users(&self, channel_id: &ChannelId) -> Vec<UserId> {
        self.store
            .get()
            .streaming
            .typing_by_channel
            .get(channel_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Set the list of users typing in a channel.
    ///
    /// Replaces the current list and notifies subscribers. An empty `users`
    /// list removes the channel entry.
    pub fn set_typing_users(&self, channel_id: ChannelId, users: Vec<UserId>) {
        self.store.update(|state| {
            if users.is_empty() {
                state.streaming.typing_by_channel.remove(&channel_id);
            } else {
                state.streaming.typing_by_channel.insert(channel_id, users);
            }
            state.streaming.last_typing_update = Some(Instant::now());
        });
    }

    /// Clear typing state for a channel.
    pub fn clear_typing(&self, channel_id: &ChannelId) {
        self.store.update(|state| {
            state.streaming.typing_by_channel.remove(channel_id);
            state.streaming.last_typing_update = Some(Instant::now());
        });
    }

    /// Order an incoming typing indicator against what already ended it.
    ///
    /// Typing indicators are separate sealed messages and can arrive out of
    /// order, so a STARTED sent before a STOPPED (or before the message it
    /// announced) may land after it and show "typing" for a sender who has
    /// stopped. `timestamp` is the SENDER's clock (TypingMessage.timestamp,
    /// or a message's sent_at), so the comparison is within one clock.
    ///
    /// A STOPPED / message records its timestamp and is always accepted.
    /// A STARTED is accepted unless it is no newer than the last recorded end
    /// for the same sender in the same conversation. `timestamp == 0` means
    /// the sender gave none: accepted, never recorded.
    pub fn accept_typing(
        &self,
        channel_id: ChannelId,
        user_id: UserId,
        timestamp: u64,
        started: bool,
    ) -> bool {
        if timestamp == 0 {
            return true;
        }
        let mut ended = self.typing_ended_at.lock();
        let last = ended.entry((channel_id, user_id)).or_insert(0);
        if started {
            timestamp > *last
        } else {
            *last = (*last).max(timestamp);
            true
        }
    }

    // ========================================================================
    // Streaming ephemera: reactions cache
    // ========================================================================

    /// Get reactions for a message.
    #[must_use]
    pub fn get_reactions(&self, message_id: &MessageId) -> Option<Vec<ReactionSummary>> {
        self.store
            .get()
            .streaming
            .recent_reactions
            .get(message_id)
            .cloned()
    }

    /// Replace a scoped cache entry with persisted core presentation.
    pub fn replace_reactions(&self, message_id: &MessageId, reactions: Vec<ReactionSummary>) {
        self.store.update(|state| {
            if reactions.is_empty() {
                state.streaming.recent_reactions.remove(message_id);
            } else {
                state
                    .streaming
                    .recent_reactions
                    .insert(*message_id, reactions);
            }
        });
    }

    /// Update a single reaction on a message.
    ///
    /// # Arguments
    ///
    /// * `message_id` - The message to update
    /// * `emoji` - The emoji being reacted with
    /// * `delta` - Change in count (+1 for add, -1 for remove)
    /// * `user_reacted` - If `Some`, sets whether the current user has reacted
    pub fn update_reaction(
        &self,
        message_id: &MessageId,
        emoji: String,
        delta: i32,
        user_reacted: Option<bool>,
    ) {
        self.store.update(|state| {
            let reactions = state
                .streaming
                .recent_reactions
                .entry(*message_id)
                .or_default();

            // Find existing reaction for this emoji
            if let Some(reaction) = reactions.iter_mut().find(|r| r.emoji == emoji) {
                // Update existing reaction
                reaction.count = (reaction.count as i32 + delta).max(0) as u32;
                if let Some(reacted) = user_reacted {
                    reaction.user_reacted = reacted;
                }

                // Remove if count is zero
                if reaction.count == 0 {
                    reactions.retain(|r| r.emoji != emoji);
                }
            } else if delta > 0 {
                // Add new reaction entry (only if adding)
                reactions.push(ReactionSummary {
                    emoji,
                    count: delta as u32,
                    user_reacted: user_reacted.unwrap_or(false),
                });
            }

            // Clean up empty entries
            if reactions.is_empty() {
                state.streaming.recent_reactions.remove(message_id);
            }
        });
    }
}
