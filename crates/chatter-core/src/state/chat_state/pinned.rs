use crate::domain::ids::MessageId;

use super::ChatState;

// ============================================================================
// Pinned messages bar (Telegram-style navigation)
// ============================================================================

/// Pinned messages bar state.
///
/// Tracks the pinned messages for the current channel and navigation
/// position through them (Telegram-style). Lives inside [`super::ChatStateData`]
/// as `pinned`.
#[derive(Clone, Debug, Default)]
pub struct PinnedSubstate {
    /// Ordered list of pinned message IDs.
    pub pinned_message_ids: Vec<MessageId>,
    /// Current navigation position (0-based).
    pub current_index: usize,
    /// Whether the pinned bar has been dismissed by user.
    /// Resets when switching channels or when new message is pinned.
    pub is_dismissed: bool,
}

impl ChatState {
    // ========================================================================
    // Pinned messages bar (Telegram-style navigation)
    //
    // All methods operate on `state.pinned` and notify subscribers through the
    // single ChatState store.
    // ========================================================================

    /// Set pinned messages for the open conversation.
    ///
    /// Resets the navigation index to 0 and clears dismissed state.
    pub fn set_pinned_messages(&self, message_ids: Vec<MessageId>) {
        self.store.update(|state| {
            state.pinned.pinned_message_ids = message_ids;
            state.pinned.current_index = 0;
            state.pinned.is_dismissed = false; // Reset on channel change
        });
    }

    /// Clear all pinned-bar state.
    pub fn clear_pinned(&self) {
        self.store.update(|state| {
            state.pinned.pinned_message_ids.clear();
            state.pinned.current_index = 0;
            state.pinned.is_dismissed = false;
        });
    }

    /// Navigate to the next pinned message (wraps around).
    pub fn pinned_navigate_next(&self) {
        self.store.update(|state| {
            if !state.pinned.pinned_message_ids.is_empty() {
                state.pinned.current_index =
                    (state.pinned.current_index + 1) % state.pinned.pinned_message_ids.len();
            }
        });
    }

    /// Navigate to the previous pinned message (wraps around).
    pub fn pinned_navigate_prev(&self) {
        self.store.update(|state| {
            if !state.pinned.pinned_message_ids.is_empty() {
                if state.pinned.current_index == 0 {
                    state.pinned.current_index = state.pinned.pinned_message_ids.len() - 1;
                } else {
                    state.pinned.current_index -= 1;
                }
            }
        });
    }

    /// Navigate to a specific pinned-message index. Clamps to valid range.
    pub fn pinned_navigate_to(&self, index: usize) {
        self.store.update(|state| {
            if !state.pinned.pinned_message_ids.is_empty() {
                state.pinned.current_index = index.min(state.pinned.pinned_message_ids.len() - 1);
            }
        });
    }

    /// Dismiss the pinned bar.
    ///
    /// The bar will reappear when switching channels or when a new message is pinned.
    pub fn pinned_dismiss(&self) {
        self.store.update(|state| {
            state.pinned.is_dismissed = true;
        });
    }

    /// Un-dismiss the pinned bar (re-show it).
    pub fn pinned_undismiss(&self) {
        self.store.update(|state| {
            state.pinned.is_dismissed = false;
        });
    }

    /// Check if the pinned bar should be visible.
    ///
    /// Returns true if there are pinned messages and the bar is not dismissed.
    #[must_use]
    pub fn pinned_should_show(&self) -> bool {
        let state = self.store.get();
        !state.pinned.pinned_message_ids.is_empty() && !state.pinned.is_dismissed
    }

    /// Get the currently focused pinned message ID.
    #[must_use]
    pub fn pinned_current_message_id(&self) -> Option<MessageId> {
        let state = self.store.get();
        state
            .pinned
            .pinned_message_ids
            .get(state.pinned.current_index)
            .copied()
    }

    /// Get the total count of pinned messages.
    #[must_use]
    pub fn pinned_total_count(&self) -> usize {
        self.store.get().pinned.pinned_message_ids.len()
    }

    /// Add a pinned message to the list (deduped, appended at end).
    pub fn add_pinned_message(&self, message_id: MessageId) {
        self.store.update(|state| {
            if !state.pinned.pinned_message_ids.contains(&message_id) {
                state.pinned.pinned_message_ids.push(message_id);
            }
        });
    }

    /// Remove a pinned message from the list (adjusts current index).
    pub fn remove_pinned_message(&self, message_id: &MessageId) {
        self.store.update(|state| {
            if let Some(index) = state
                .pinned
                .pinned_message_ids
                .iter()
                .position(|id| id == message_id)
            {
                state.pinned.pinned_message_ids.remove(index);

                // Adjust current_index if needed
                if state.pinned.pinned_message_ids.is_empty() {
                    state.pinned.current_index = 0;
                } else if state.pinned.current_index >= state.pinned.pinned_message_ids.len() {
                    state.pinned.current_index = state.pinned.pinned_message_ids.len() - 1;
                }
            }
        });
    }
}
