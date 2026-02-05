//! Streaming state.
//!
//! Real-time data (typing, reactions) for UI binding.

use std::collections::HashMap;
use std::time::Instant;

use tokio::sync::broadcast;

use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::state::Store;

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

/// Streaming state data for real-time updates.
#[derive(Clone, Debug, Default)]
pub struct StreamingStateData {
    /// Who is typing per channel.
    pub typing_by_channel: HashMap<ChannelId, Vec<UserId>>,
    /// Recent reactions per message (limited cache for display).
    pub recent_reactions: HashMap<MessageId, Vec<ReactionSummary>>,
    /// When typing state was last updated (for staleness detection).
    pub last_typing_update: Option<Instant>,
}

/// Reactive state for streaming data (typing indicators, reactions).
///
/// This state is a simplified view for UI binding. The `TypingAggregator`
/// and `ReactionAggregator` in the network layer handle raw stream processing.
/// Services bridge from those aggregators to this state.
///
/// # Example
///
/// ```ignore
/// use chatter_linux::state::StreamingState;
/// use chatter_linux::domain::ids::{ChannelId, UserId};
///
/// let state = StreamingState::new();
///
/// // Subscribe to changes
/// let mut rx = state.subscribe();
///
/// // Update typing users
/// let channel_id = ChannelId::new();
/// let user_id = UserId::new();
/// state.set_typing_users(channel_id, vec![user_id]);
///
/// // Query typing users
/// let typers = state.get_typing_users(&channel_id);
/// ```
#[derive(Clone)]
pub struct StreamingState {
    store: Store<StreamingStateData>,
}

impl StreamingState {
    /// Create a new StreamingState.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(StreamingStateData::default()),
        }
    }

    /// Get users currently typing in a channel.
    #[must_use]
    pub fn get_typing_users(&self, channel_id: &ChannelId) -> Vec<UserId> {
        self.store
            .get()
            .typing_by_channel
            .get(channel_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Set the list of users typing in a channel.
    ///
    /// This replaces the current list and notifies subscribers.
    pub fn set_typing_users(&self, channel_id: ChannelId, users: Vec<UserId>) {
        self.store.update(|data| {
            if users.is_empty() {
                data.typing_by_channel.remove(&channel_id);
            } else {
                data.typing_by_channel.insert(channel_id, users);
            }
            data.last_typing_update = Some(Instant::now());
        });
    }

    /// Clear typing state for a channel.
    pub fn clear_typing(&self, channel_id: &ChannelId) {
        self.store.update(|data| {
            data.typing_by_channel.remove(channel_id);
            data.last_typing_update = Some(Instant::now());
        });
    }

    /// Get reactions for a message.
    #[must_use]
    pub fn get_reactions(&self, message_id: &MessageId) -> Option<Vec<ReactionSummary>> {
        self.store.get().recent_reactions.get(message_id).cloned()
    }

    /// Set reactions for a message.
    ///
    /// This replaces the current reaction list and notifies subscribers.
    pub fn set_reactions(&self, message_id: MessageId, reactions: Vec<ReactionSummary>) {
        self.store.update(|data| {
            if reactions.is_empty() {
                data.recent_reactions.remove(&message_id);
            } else {
                data.recent_reactions.insert(message_id, reactions);
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
    /// * `user_reacted` - If Some, sets whether current user has reacted
    pub fn update_reaction(
        &self,
        message_id: &MessageId,
        emoji: String,
        delta: i32,
        user_reacted: Option<bool>,
    ) {
        self.store.update(|data| {
            let reactions = data.recent_reactions.entry(*message_id).or_default();

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
                data.recent_reactions.remove(message_id);
            }
        });
    }

    /// Subscribe to state changes.
    ///
    /// Returns a receiver that will receive updates when state changes.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<StreamingStateData> {
        self.store.subscribe()
    }

    /// Clear all streaming state.
    pub fn clear(&self) {
        self.store.set(StreamingStateData::default());
    }

    /// Get the last typing update time.
    #[must_use]
    pub fn last_typing_update(&self) -> Option<Instant> {
        self.store.get().last_typing_update
    }
}

impl Default for StreamingState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn make_channel_id() -> ChannelId {
        ChannelId(Uuid::new_v4())
    }

    fn make_user_id() -> UserId {
        UserId(Uuid::new_v4())
    }

    fn make_message_id() -> MessageId {
        MessageId(Uuid::new_v4())
    }

    // ==================== Typing Tests ====================

    #[test]
    fn test_set_and_get_typing_users() {
        let state = StreamingState::new();
        let channel_id = make_channel_id();
        let user1 = make_user_id();
        let user2 = make_user_id();

        // Initially empty
        assert!(state.get_typing_users(&channel_id).is_empty());

        // Set typing users
        state.set_typing_users(channel_id.clone(), vec![user1.clone(), user2.clone()]);

        let typers = state.get_typing_users(&channel_id);
        assert_eq!(typers.len(), 2);
        assert!(typers.contains(&user1));
        assert!(typers.contains(&user2));
    }

    #[test]
    fn test_clear_typing() {
        let state = StreamingState::new();
        let channel_id = make_channel_id();
        let user1 = make_user_id();

        state.set_typing_users(channel_id.clone(), vec![user1]);
        assert!(!state.get_typing_users(&channel_id).is_empty());

        state.clear_typing(&channel_id);
        assert!(state.get_typing_users(&channel_id).is_empty());
    }

    #[test]
    fn test_set_empty_typing_removes_entry() {
        let state = StreamingState::new();
        let channel_id = make_channel_id();
        let user1 = make_user_id();

        state.set_typing_users(channel_id.clone(), vec![user1]);
        state.set_typing_users(channel_id.clone(), vec![]);

        // Should be removed from the map
        assert!(state.store.get().typing_by_channel.get(&channel_id).is_none());
    }

    #[test]
    fn test_last_typing_update() {
        let state = StreamingState::new();
        let channel_id = make_channel_id();

        // Initially none
        assert!(state.last_typing_update().is_none());

        // After setting typing users
        state.set_typing_users(channel_id.clone(), vec![make_user_id()]);
        assert!(state.last_typing_update().is_some());

        // After clearing
        let before_clear = state.last_typing_update();
        state.clear_typing(&channel_id);
        let after_clear = state.last_typing_update();

        // Should have updated the timestamp
        assert!(after_clear >= before_clear);
    }

    // ==================== Reaction Tests ====================

    #[test]
    fn test_set_and_get_reactions() {
        let state = StreamingState::new();
        let message_id = make_message_id();

        // Initially none
        assert!(state.get_reactions(&message_id).is_none());

        // Set reactions
        let reactions = vec![
            ReactionSummary {
                emoji: "👍".to_string(),
                count: 5,
                user_reacted: true,
            },
            ReactionSummary {
                emoji: "❤️".to_string(),
                count: 3,
                user_reacted: false,
            },
        ];

        state.set_reactions(message_id.clone(), reactions.clone());

        let retrieved = state.get_reactions(&message_id).unwrap();
        assert_eq!(retrieved.len(), 2);
        assert_eq!(retrieved[0].emoji, "👍");
        assert_eq!(retrieved[0].count, 5);
        assert!(retrieved[0].user_reacted);
    }

    #[test]
    fn test_update_reaction_increment() {
        let state = StreamingState::new();
        let message_id = make_message_id();

        // Set initial reactions
        state.set_reactions(
            message_id.clone(),
            vec![ReactionSummary {
                emoji: "👍".to_string(),
                count: 2,
                user_reacted: false,
            }],
        );

        // Increment
        state.update_reaction(&message_id, "👍".to_string(), 1, None);

        let reactions = state.get_reactions(&message_id).unwrap();
        assert_eq!(reactions[0].count, 3);
        assert!(!reactions[0].user_reacted); // Unchanged
    }

    #[test]
    fn test_update_reaction_decrement() {
        let state = StreamingState::new();
        let message_id = make_message_id();

        // Set initial reactions
        state.set_reactions(
            message_id.clone(),
            vec![ReactionSummary {
                emoji: "👍".to_string(),
                count: 2,
                user_reacted: true,
            }],
        );

        // Decrement
        state.update_reaction(&message_id, "👍".to_string(), -1, Some(false));

        let reactions = state.get_reactions(&message_id).unwrap();
        assert_eq!(reactions[0].count, 1);
        assert!(!reactions[0].user_reacted); // Changed
    }

    #[test]
    fn test_update_reaction_removes_at_zero() {
        let state = StreamingState::new();
        let message_id = make_message_id();

        // Set initial reactions
        state.set_reactions(
            message_id.clone(),
            vec![ReactionSummary {
                emoji: "👍".to_string(),
                count: 1,
                user_reacted: false,
            }],
        );

        // Decrement to zero
        state.update_reaction(&message_id, "👍".to_string(), -1, None);

        // Should be removed entirely
        assert!(state.get_reactions(&message_id).is_none());
    }

    #[test]
    fn test_update_reaction_adds_new() {
        let state = StreamingState::new();
        let message_id = make_message_id();

        // Add new reaction to message with no reactions
        state.update_reaction(&message_id, "🎉".to_string(), 1, Some(true));

        let reactions = state.get_reactions(&message_id).unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].emoji, "🎉");
        assert_eq!(reactions[0].count, 1);
        assert!(reactions[0].user_reacted);
    }

    #[test]
    fn test_update_reaction_ignores_negative_new() {
        let state = StreamingState::new();
        let message_id = make_message_id();

        // Trying to decrement a non-existent reaction should do nothing
        state.update_reaction(&message_id, "👍".to_string(), -1, None);

        assert!(state.get_reactions(&message_id).is_none());
    }

    // ==================== General Tests ====================

    #[test]
    fn test_clear_all() {
        let state = StreamingState::new();

        // Add some data
        let channel_id = make_channel_id();
        let message_id = make_message_id();

        state.set_typing_users(channel_id.clone(), vec![make_user_id()]);
        state.set_reactions(
            message_id.clone(),
            vec![ReactionSummary {
                emoji: "👍".to_string(),
                count: 1,
                user_reacted: false,
            }],
        );

        // Clear everything
        state.clear();

        assert!(state.get_typing_users(&channel_id).is_empty());
        assert!(state.get_reactions(&message_id).is_none());
        assert!(state.last_typing_update().is_none());
    }

    #[tokio::test]
    async fn test_subscribe_receives_typing_updates() {
        let state = StreamingState::new();
        let mut rx = state.subscribe();

        let channel_id = make_channel_id();
        let user1 = make_user_id();

        state.set_typing_users(channel_id.clone(), vec![user1.clone()]);

        let received = rx.recv().await.unwrap();
        let typers = received.typing_by_channel.get(&channel_id).unwrap();
        assert_eq!(typers.len(), 1);
        assert!(typers.contains(&user1));
    }

    #[tokio::test]
    async fn test_subscribe_receives_reaction_updates() {
        let state = StreamingState::new();
        let mut rx = state.subscribe();

        let message_id = make_message_id();

        state.set_reactions(
            message_id.clone(),
            vec![ReactionSummary {
                emoji: "👍".to_string(),
                count: 1,
                user_reacted: true,
            }],
        );

        let received = rx.recv().await.unwrap();
        let reactions = received.recent_reactions.get(&message_id).unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0].emoji, "👍");
    }

    #[test]
    fn test_default_impl() {
        let state = StreamingState::default();
        let data = state.store.get();
        assert!(data.typing_by_channel.is_empty());
        assert!(data.recent_reactions.is_empty());
        assert!(data.last_typing_update.is_none());
    }

    #[test]
    fn test_clone_shares_state() {
        let state1 = StreamingState::new();
        let state2 = state1.clone();

        let channel_id = make_channel_id();
        let user1 = make_user_id();

        state1.set_typing_users(channel_id.clone(), vec![user1.clone()]);

        // Both should see the update
        assert_eq!(state2.get_typing_users(&channel_id), vec![user1]);
    }
}
