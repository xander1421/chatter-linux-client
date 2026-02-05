//! Pinned state.
//!
//! Pinned messages bar navigation.

use tokio::sync::broadcast;

use crate::domain::ids::{ChannelId, MessageId};
use crate::state::store::Store;

/// Pinned messages bar state data.
///
/// Tracks the pinned messages for the current channel and
/// navigation position through them (Telegram-style).
#[derive(Clone, Debug, Default)]
pub struct PinnedStateData {
    /// Current channel (pinned messages context).
    pub channel_id: Option<ChannelId>,
    /// Ordered list of pinned message IDs.
    pub pinned_message_ids: Vec<MessageId>,
    /// Current navigation position (0-based).
    pub current_index: usize,
    /// Whether the pinned bar is expanded.
    pub is_expanded: bool,
}

/// Reactive pinned messages state manager.
///
/// Provides Telegram-style navigation through pinned messages:
/// - Navigate next/prev with wraparound
/// - Jump to specific index
/// - Toggle expanded view
///
/// # Example
///
/// ```ignore
/// let pinned_state = PinnedState::new();
///
/// // Set pinned messages for a channel
/// pinned_state.set_pinned_messages(channel_id, vec![msg1, msg2, msg3]);
///
/// // Navigate through them
/// pinned_state.navigate_next(); // moves to msg2
/// pinned_state.navigate_next(); // moves to msg3
/// pinned_state.navigate_next(); // wraps to msg1
///
/// // Get current pinned message
/// let current = pinned_state.current_message_id();
/// ```
#[derive(Clone)]
pub struct PinnedState {
    store: Store<PinnedStateData>,
}

impl PinnedState {
    /// Create a new pinned state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(PinnedStateData::default()),
        }
    }

    /// Get the current pinned state.
    #[must_use]
    pub fn get(&self) -> PinnedStateData {
        self.store.get()
    }

    /// Subscribe to pinned state changes.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<PinnedStateData> {
        self.store.subscribe()
    }

    /// Set pinned messages for a channel.
    ///
    /// Resets the navigation index to 0.
    pub fn set_pinned_messages(&self, channel_id: ChannelId, message_ids: Vec<MessageId>) {
        self.store.update(|state| {
            state.channel_id = Some(channel_id);
            state.pinned_message_ids = message_ids;
            state.current_index = 0;
            // Keep expanded state unchanged
        });
    }

    /// Clear all pinned state.
    pub fn clear(&self) {
        self.store.update(|state| {
            state.channel_id = None;
            state.pinned_message_ids.clear();
            state.current_index = 0;
            state.is_expanded = false;
        });
    }

    /// Navigate to the next pinned message.
    ///
    /// Wraps around to the first message after the last.
    pub fn navigate_next(&self) {
        self.store.update(|state| {
            if !state.pinned_message_ids.is_empty() {
                state.current_index = (state.current_index + 1) % state.pinned_message_ids.len();
            }
        });
    }

    /// Navigate to the previous pinned message.
    ///
    /// Wraps around to the last message before the first.
    pub fn navigate_prev(&self) {
        self.store.update(|state| {
            if !state.pinned_message_ids.is_empty() {
                if state.current_index == 0 {
                    state.current_index = state.pinned_message_ids.len() - 1;
                } else {
                    state.current_index -= 1;
                }
            }
        });
    }

    /// Navigate to a specific index.
    ///
    /// Clamps to valid range if out of bounds.
    pub fn navigate_to(&self, index: usize) {
        self.store.update(|state| {
            if !state.pinned_message_ids.is_empty() {
                state.current_index = index.min(state.pinned_message_ids.len() - 1);
            }
        });
    }

    /// Toggle the expanded state of the pinned bar.
    pub fn toggle_expanded(&self) {
        self.store.update(|state| {
            state.is_expanded = !state.is_expanded;
        });
    }

    /// Set the expanded state of the pinned bar.
    pub fn set_expanded(&self, expanded: bool) {
        self.store.update(|state| {
            state.is_expanded = expanded;
        });
    }

    /// Get the currently focused pinned message ID.
    #[must_use]
    pub fn current_message_id(&self) -> Option<MessageId> {
        let state = self.store.get();
        state.pinned_message_ids.get(state.current_index).copied()
    }

    /// Get the total count of pinned messages.
    #[must_use]
    pub fn total_count(&self) -> usize {
        self.store.get().pinned_message_ids.len()
    }

    /// Check if there are multiple pinned messages (2 or more).
    ///
    /// Useful for showing navigation controls.
    #[must_use]
    pub fn has_multiple(&self) -> bool {
        self.store.get().pinned_message_ids.len() >= 2
    }

    /// Add a pinned message to the list.
    ///
    /// Adds to the end of the list if not already present.
    pub fn add_pinned_message(&self, message_id: MessageId) {
        self.store.update(|state| {
            if !state.pinned_message_ids.contains(&message_id) {
                state.pinned_message_ids.push(message_id);
            }
        });
    }

    /// Remove a pinned message from the list.
    ///
    /// Adjusts the current index if needed.
    pub fn remove_pinned_message(&self, message_id: &MessageId) {
        self.store.update(|state| {
            if let Some(index) = state.pinned_message_ids.iter().position(|id| id == message_id) {
                state.pinned_message_ids.remove(index);

                // Adjust current_index if needed
                if state.pinned_message_ids.is_empty() {
                    state.current_index = 0;
                } else if state.current_index >= state.pinned_message_ids.len() {
                    state.current_index = state.pinned_message_ids.len() - 1;
                }
            }
        });
    }
}

impl Default for PinnedState {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PinnedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedState")
            .field("data", &self.get())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pinned_state_default() {
        let state = PinnedState::new();
        let data = state.get();

        assert!(data.channel_id.is_none());
        assert!(data.pinned_message_ids.is_empty());
        assert_eq!(data.current_index, 0);
        assert!(!data.is_expanded);
    }

    #[test]
    fn test_set_pinned_messages() {
        let state = PinnedState::new();
        let channel_id = ChannelId::new();
        let msg1 = MessageId::new();
        let msg2 = MessageId::new();

        state.set_pinned_messages(channel_id, vec![msg1, msg2]);
        let data = state.get();

        assert_eq!(data.channel_id, Some(channel_id));
        assert_eq!(data.pinned_message_ids.len(), 2);
        assert_eq!(data.current_index, 0);
    }

    #[test]
    fn test_set_pinned_messages_resets_index() {
        let state = PinnedState::new();
        let channel_id = ChannelId::new();

        state.set_pinned_messages(channel_id, vec![MessageId::new(), MessageId::new(), MessageId::new()]);
        state.navigate_to(2);
        assert_eq!(state.get().current_index, 2);

        // Setting new messages should reset index
        state.set_pinned_messages(channel_id, vec![MessageId::new()]);
        assert_eq!(state.get().current_index, 0);
    }

    #[test]
    fn test_clear() {
        let state = PinnedState::new();
        let channel_id = ChannelId::new();

        state.set_pinned_messages(channel_id, vec![MessageId::new()]);
        state.set_expanded(true);
        state.clear();

        let data = state.get();
        assert!(data.channel_id.is_none());
        assert!(data.pinned_message_ids.is_empty());
        assert_eq!(data.current_index, 0);
        assert!(!data.is_expanded);
    }

    #[test]
    fn test_navigate_next_wraparound() {
        let state = PinnedState::new();
        let channel_id = ChannelId::new();
        let msg1 = MessageId::new();
        let msg2 = MessageId::new();
        let msg3 = MessageId::new();

        state.set_pinned_messages(channel_id, vec![msg1, msg2, msg3]);

        assert_eq!(state.current_message_id(), Some(msg1));

        state.navigate_next();
        assert_eq!(state.current_message_id(), Some(msg2));

        state.navigate_next();
        assert_eq!(state.current_message_id(), Some(msg3));

        // Wrap around
        state.navigate_next();
        assert_eq!(state.current_message_id(), Some(msg1));
    }

    #[test]
    fn test_navigate_prev_wraparound() {
        let state = PinnedState::new();
        let channel_id = ChannelId::new();
        let msg1 = MessageId::new();
        let msg2 = MessageId::new();
        let msg3 = MessageId::new();

        state.set_pinned_messages(channel_id, vec![msg1, msg2, msg3]);

        // Start at first, go back to wrap to last
        state.navigate_prev();
        assert_eq!(state.current_message_id(), Some(msg3));

        state.navigate_prev();
        assert_eq!(state.current_message_id(), Some(msg2));

        state.navigate_prev();
        assert_eq!(state.current_message_id(), Some(msg1));
    }

    #[test]
    fn test_navigate_empty_list() {
        let state = PinnedState::new();

        // Should not panic
        state.navigate_next();
        state.navigate_prev();
        state.navigate_to(5);

        assert!(state.current_message_id().is_none());
    }

    #[test]
    fn test_navigate_to() {
        let state = PinnedState::new();
        let channel_id = ChannelId::new();
        let msg1 = MessageId::new();
        let msg2 = MessageId::new();
        let msg3 = MessageId::new();

        state.set_pinned_messages(channel_id, vec![msg1, msg2, msg3]);

        state.navigate_to(2);
        assert_eq!(state.current_message_id(), Some(msg3));

        // Out of bounds - should clamp
        state.navigate_to(100);
        assert_eq!(state.current_message_id(), Some(msg3)); // Clamped to last
    }

    #[test]
    fn test_toggle_expanded() {
        let state = PinnedState::new();

        assert!(!state.get().is_expanded);

        state.toggle_expanded();
        assert!(state.get().is_expanded);

        state.toggle_expanded();
        assert!(!state.get().is_expanded);
    }

    #[test]
    fn test_set_expanded() {
        let state = PinnedState::new();

        state.set_expanded(true);
        assert!(state.get().is_expanded);

        state.set_expanded(false);
        assert!(!state.get().is_expanded);
    }

    #[test]
    fn test_current_message_id() {
        let state = PinnedState::new();
        assert!(state.current_message_id().is_none());

        let channel_id = ChannelId::new();
        let msg = MessageId::new();
        state.set_pinned_messages(channel_id, vec![msg]);

        assert_eq!(state.current_message_id(), Some(msg));
    }

    #[test]
    fn test_total_count() {
        let state = PinnedState::new();
        assert_eq!(state.total_count(), 0);

        let channel_id = ChannelId::new();
        state.set_pinned_messages(channel_id, vec![MessageId::new(), MessageId::new()]);

        assert_eq!(state.total_count(), 2);
    }

    #[test]
    fn test_has_multiple() {
        let state = PinnedState::new();
        let channel_id = ChannelId::new();

        assert!(!state.has_multiple());

        state.set_pinned_messages(channel_id, vec![MessageId::new()]);
        assert!(!state.has_multiple());

        state.set_pinned_messages(channel_id, vec![MessageId::new(), MessageId::new()]);
        assert!(state.has_multiple());
    }

    #[test]
    fn test_add_pinned_message() {
        let state = PinnedState::new();
        let channel_id = ChannelId::new();
        let msg1 = MessageId::new();
        let msg2 = MessageId::new();

        state.set_pinned_messages(channel_id, vec![msg1]);
        state.add_pinned_message(msg2);

        let data = state.get();
        assert_eq!(data.pinned_message_ids.len(), 2);
        assert!(data.pinned_message_ids.contains(&msg2));

        // Adding duplicate should not add again
        state.add_pinned_message(msg1);
        assert_eq!(state.total_count(), 2);
    }

    #[test]
    fn test_remove_pinned_message() {
        let state = PinnedState::new();
        let channel_id = ChannelId::new();
        let msg1 = MessageId::new();
        let msg2 = MessageId::new();
        let msg3 = MessageId::new();

        state.set_pinned_messages(channel_id, vec![msg1, msg2, msg3]);
        state.navigate_to(2); // Focus msg3

        state.remove_pinned_message(&msg2);

        let data = state.get();
        assert_eq!(data.pinned_message_ids.len(), 2);
        assert!(!data.pinned_message_ids.contains(&msg2));
        // Index should still be valid (pointing to msg3, now at index 1)
        assert_eq!(data.current_index, 1);
    }

    #[test]
    fn test_remove_pinned_message_adjusts_index() {
        let state = PinnedState::new();
        let channel_id = ChannelId::new();
        let msg1 = MessageId::new();
        let msg2 = MessageId::new();

        state.set_pinned_messages(channel_id, vec![msg1, msg2]);
        state.navigate_to(1); // Focus msg2

        // Remove msg2 (the one at current index)
        state.remove_pinned_message(&msg2);

        let data = state.get();
        assert_eq!(data.pinned_message_ids.len(), 1);
        assert_eq!(data.current_index, 0); // Adjusted to valid range
    }

    #[test]
    fn test_remove_last_pinned_message() {
        let state = PinnedState::new();
        let channel_id = ChannelId::new();
        let msg = MessageId::new();

        state.set_pinned_messages(channel_id, vec![msg]);
        state.remove_pinned_message(&msg);

        let data = state.get();
        assert!(data.pinned_message_ids.is_empty());
        assert_eq!(data.current_index, 0);
    }

    #[tokio::test]
    async fn test_pinned_state_subscribe() {
        let state = PinnedState::new();
        let mut rx = state.subscribe();
        let channel_id = ChannelId::new();
        let msg = MessageId::new();

        state.set_pinned_messages(channel_id, vec![msg]);

        let received = rx.recv().await.unwrap();
        assert_eq!(received.pinned_message_ids.len(), 1);
    }

    #[test]
    fn test_pinned_state_clone_shares_state() {
        let state1 = PinnedState::new();
        let state2 = state1.clone();
        let channel_id = ChannelId::new();

        state1.set_pinned_messages(channel_id, vec![MessageId::new(), MessageId::new()]);
        assert_eq!(state2.total_count(), 2);

        state1.navigate_next();
        assert_eq!(state2.get().current_index, 1);
    }
}
