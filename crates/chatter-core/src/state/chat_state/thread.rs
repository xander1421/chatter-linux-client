use crate::domain::ids::{ChannelId, MessageId};

use super::ChatState;

/// Maximum number of concurrent thread panels allowed.
const MAX_OPEN_THREADS: usize = 3;

/// Information about an open thread panel.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadPanel {
    /// Channel containing the thread.
    pub channel_id: ChannelId,
    /// Root message ID of the thread.
    pub root_message_id: MessageId,
    /// Number of replies in the thread.
    pub reply_count: u32,
}

impl ThreadPanel {
    /// Create a new thread panel.
    #[must_use]
    pub fn new(channel_id: ChannelId, root_message_id: MessageId) -> Self {
        Self {
            channel_id,
            root_message_id,
            reply_count: 0,
        }
    }
}

impl ChatState {
    /// Open a thread panel.
    ///
    /// If a thread for this root message is already open, focuses it instead.
    /// If already at max threads (3), replaces the oldest one.
    pub fn open_thread(&self, channel_id: ChannelId, root_message_id: MessageId) {
        self.store.update(|state| {
            // Check if thread is already open
            if let Some(index) = state
                .open_threads
                .iter()
                .position(|t| t.root_message_id == root_message_id)
            {
                // Focus existing thread
                state.active_thread_index = Some(index);
                return;
            }

            // If at max capacity, remove oldest (first) thread
            if state.open_threads.len() >= MAX_OPEN_THREADS {
                state.open_threads.remove(0);
            }

            // Add new thread
            state
                .open_threads
                .push(ThreadPanel::new(channel_id, root_message_id));
            state.active_thread_index = Some(state.open_threads.len() - 1);
        });
    }

    /// Close a thread panel by its root message ID.
    ///
    /// Adjusts the active thread index if needed.
    pub fn close_thread(&self, root_message_id: MessageId) {
        self.store.update(|state| {
            if let Some(index) = state
                .open_threads
                .iter()
                .position(|t| t.root_message_id == root_message_id)
            {
                state.open_threads.remove(index);

                // Adjust active_thread_index
                if let Some(active_idx) = state.active_thread_index {
                    if active_idx == index {
                        // Closed the active thread
                        state.active_thread_index = if state.open_threads.is_empty() {
                            None
                        } else {
                            Some(active_idx.min(state.open_threads.len() - 1))
                        };
                    } else if active_idx > index {
                        // Active thread shifted down
                        state.active_thread_index = Some(active_idx - 1);
                    }
                }
            }
        });
    }

    /// Get the currently active thread panel, if any.
    #[must_use]
    pub fn active_thread(&self) -> Option<ThreadPanel> {
        let state = self.store.get();
        state
            .active_thread_index
            .and_then(|idx| state.open_threads.get(idx).cloned())
    }

    /// Update the reply count for a thread panel.
    pub fn update_thread_reply_count(&self, root_message_id: MessageId, reply_count: u32) {
        self.store.update(|state| {
            if let Some(thread) = state
                .open_threads
                .iter_mut()
                .find(|t| t.root_message_id == root_message_id)
            {
                thread.reply_count = reply_count;
            }
        });
    }
}
