//! Chat state.
//!
//! Active channel, threads (up to 3 concurrent).

use std::path::PathBuf;
use tokio::sync::broadcast;

use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::state::store::Store;

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

/// A pending file attachment (uploaded but not yet sent).
#[derive(Clone, Debug)]
pub struct PendingAttachment {
    /// The attachment ID from the server after upload.
    pub id: AttachmentId,
    /// Original filename.
    pub filename: String,
    /// Original file path (for display/reference).
    pub path: PathBuf,
    /// File size in bytes.
    pub size: u64,
    /// MIME type.
    pub mime_type: String,
}

impl PendingAttachment {
    /// Create a new pending attachment.
    #[must_use]
    pub fn new(id: AttachmentId, filename: String, path: PathBuf, size: u64, mime_type: String) -> Self {
        Self {
            id,
            filename,
            path,
            size,
            mime_type,
        }
    }
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

    /// Create a thread panel with a known reply count.
    #[must_use]
    pub fn with_reply_count(channel_id: ChannelId, root_message_id: MessageId, reply_count: u32) -> Self {
        Self {
            channel_id,
            root_message_id,
            reply_count,
        }
    }
}

/// Chat navigation state data.
///
/// Tracks the currently active channel, open thread panels, and pending attachments.
#[derive(Clone, Debug, Default)]
pub struct ChatStateData {
    /// Currently selected channel ID.
    pub active_channel_id: Option<ChannelId>,
    /// Open thread panels (max 3 concurrent).
    pub open_threads: Vec<ThreadPanel>,
    /// Index of the focused thread panel (0, 1, or 2), if any.
    pub active_thread_index: Option<usize>,
    /// Pending attachments for the current message being composed.
    pub pending_attachments: Vec<PendingAttachment>,
}

/// Reactive chat navigation state manager.
///
/// Manages the currently active channel and thread panels.
/// Enforces a maximum of 3 concurrent thread panels.
///
/// # Example
///
/// ```ignore
/// let chat_state = ChatState::new();
///
/// // Set active channel
/// chat_state.set_active_channel(channel_id);
///
/// // Open a thread (replaces oldest if already at max)
/// chat_state.open_thread(channel_id, root_message_id);
///
/// // Subscribe to changes
/// let mut rx = chat_state.subscribe();
/// ```
#[derive(Clone)]
pub struct ChatState {
    store: Store<ChatStateData>,
}

impl ChatState {
    /// Create a new chat state with default values.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(ChatStateData::default()),
        }
    }

    /// Get the current chat state.
    #[must_use]
    pub fn get(&self) -> ChatStateData {
        self.store.get()
    }

    /// Subscribe to chat state changes.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ChatStateData> {
        self.store.subscribe()
    }

    /// Set the active channel.
    ///
    /// Clears the active thread index when changing channels.
    pub fn set_active_channel(&self, channel_id: ChannelId) {
        self.store.update(|state| {
            state.active_channel_id = Some(channel_id);
            state.active_thread_index = None;
        });
    }

    /// Clear the active channel.
    pub fn clear_active_channel(&self) {
        self.store.update(|state| {
            state.active_channel_id = None;
            state.active_thread_index = None;
        });
    }

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
            state.open_threads.push(ThreadPanel::new(channel_id, root_message_id));
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

    /// Close all open thread panels.
    pub fn close_all_threads(&self) {
        self.store.update(|state| {
            state.open_threads.clear();
            state.active_thread_index = None;
        });
    }

    /// Set the active thread index.
    ///
    /// Validates that the index is within bounds.
    pub fn set_active_thread_index(&self, index: Option<usize>) {
        self.store.update(|state| {
            state.active_thread_index = match index {
                Some(idx) if idx < state.open_threads.len() => Some(idx),
                Some(_) => None, // Invalid index
                None => None,
            };
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

    /// Add a pending attachment for the current message.
    pub fn add_pending_attachment(&self, attachment: PendingAttachment) {
        self.store.update(|state| {
            state.pending_attachments.push(attachment);
        });
    }

    /// Get all pending attachment IDs.
    #[must_use]
    pub fn get_pending_attachment_ids(&self) -> Vec<AttachmentId> {
        self.store.get().pending_attachments.iter().map(|a| a.id).collect()
    }

    /// Get all pending attachments.
    #[must_use]
    pub fn get_pending_attachments(&self) -> Vec<PendingAttachment> {
        self.store.get().pending_attachments.clone()
    }

    /// Clear all pending attachments (called after message is sent).
    pub fn clear_pending_attachments(&self) {
        self.store.update(|state| {
            state.pending_attachments.clear();
        });
    }

    /// Remove a pending attachment by its ID.
    pub fn remove_pending_attachment(&self, attachment_id: AttachmentId) {
        self.store.update(|state| {
            state.pending_attachments.retain(|a| a.id != attachment_id);
        });
    }
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ChatState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatState")
            .field("data", &self.get())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_state_default() {
        let state = ChatState::new();
        let data = state.get();

        assert!(data.active_channel_id.is_none());
        assert!(data.open_threads.is_empty());
        assert!(data.active_thread_index.is_none());
    }

    #[test]
    fn test_set_active_channel() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        state.set_active_channel(channel_id);
        let data = state.get();

        assert_eq!(data.active_channel_id, Some(channel_id));
    }

    #[test]
    fn test_clear_active_channel() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        state.set_active_channel(channel_id);
        state.clear_active_channel();

        assert!(state.get().active_channel_id.is_none());
    }

    #[test]
    fn test_open_thread() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();
        let root_id = MessageId::new();

        state.open_thread(channel_id, root_id);
        let data = state.get();

        assert_eq!(data.open_threads.len(), 1);
        assert_eq!(data.open_threads[0].root_message_id, root_id);
        assert_eq!(data.active_thread_index, Some(0));
    }

    #[test]
    fn test_open_thread_max_limit() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        let id1 = MessageId::new();
        let id2 = MessageId::new();
        let id3 = MessageId::new();
        let id4 = MessageId::new();

        state.open_thread(channel_id, id1);
        state.open_thread(channel_id, id2);
        state.open_thread(channel_id, id3);
        assert_eq!(state.get().open_threads.len(), 3);

        // Opening 4th should replace the oldest (id1)
        state.open_thread(channel_id, id4);
        let data = state.get();

        assert_eq!(data.open_threads.len(), 3);
        assert!(!data.open_threads.iter().any(|t| t.root_message_id == id1));
        assert!(data.open_threads.iter().any(|t| t.root_message_id == id4));
    }

    #[test]
    fn test_open_existing_thread_focuses_it() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        let id1 = MessageId::new();
        let id2 = MessageId::new();

        state.open_thread(channel_id, id1);
        state.open_thread(channel_id, id2);
        assert_eq!(state.get().active_thread_index, Some(1));

        // Re-opening id1 should focus it without adding a new one
        state.open_thread(channel_id, id1);
        let data = state.get();

        assert_eq!(data.open_threads.len(), 2);
        assert_eq!(data.active_thread_index, Some(0));
    }

    #[test]
    fn test_close_thread() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        let id1 = MessageId::new();
        let id2 = MessageId::new();

        state.open_thread(channel_id, id1);
        state.open_thread(channel_id, id2);
        state.close_thread(id1);

        let data = state.get();
        assert_eq!(data.open_threads.len(), 1);
        assert_eq!(data.open_threads[0].root_message_id, id2);
    }

    #[test]
    fn test_close_active_thread_adjusts_index() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        let id1 = MessageId::new();
        let id2 = MessageId::new();

        state.open_thread(channel_id, id1);
        state.open_thread(channel_id, id2);
        // Active is now 1 (id2)

        state.set_active_thread_index(Some(0)); // Focus id1
        state.close_thread(id1); // Close active thread

        let data = state.get();
        assert_eq!(data.open_threads.len(), 1);
        assert_eq!(data.active_thread_index, Some(0)); // Should now be id2
    }

    #[test]
    fn test_close_all_threads() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        state.open_thread(channel_id, MessageId::new());
        state.open_thread(channel_id, MessageId::new());
        state.close_all_threads();

        let data = state.get();
        assert!(data.open_threads.is_empty());
        assert!(data.active_thread_index.is_none());
    }

    #[test]
    fn test_set_active_thread_index_bounds() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();

        state.open_thread(channel_id, MessageId::new());
        state.open_thread(channel_id, MessageId::new());

        // Valid index
        state.set_active_thread_index(Some(1));
        assert_eq!(state.get().active_thread_index, Some(1));

        // Invalid index (out of bounds)
        state.set_active_thread_index(Some(5));
        assert!(state.get().active_thread_index.is_none());

        // None
        state.set_active_thread_index(None);
        assert!(state.get().active_thread_index.is_none());
    }

    #[test]
    fn test_active_thread() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();
        let root_id = MessageId::new();

        assert!(state.active_thread().is_none());

        state.open_thread(channel_id, root_id);
        let active = state.active_thread();

        assert!(active.is_some());
        assert_eq!(active.unwrap().root_message_id, root_id);
    }

    #[test]
    fn test_update_thread_reply_count() {
        let state = ChatState::new();
        let channel_id = ChannelId::new();
        let root_id = MessageId::new();

        state.open_thread(channel_id, root_id);
        state.update_thread_reply_count(root_id, 42);

        let data = state.get();
        assert_eq!(data.open_threads[0].reply_count, 42);
    }

    #[tokio::test]
    async fn test_chat_state_subscribe() {
        let state = ChatState::new();
        let mut rx = state.subscribe();
        let channel_id = ChannelId::new();

        state.set_active_channel(channel_id);

        let received = rx.recv().await.unwrap();
        assert_eq!(received.active_channel_id, Some(channel_id));
    }

    #[test]
    fn test_chat_state_clone_shares_state() {
        let state1 = ChatState::new();
        let state2 = state1.clone();
        let channel_id = ChannelId::new();

        state1.set_active_channel(channel_id);
        assert_eq!(state2.get().active_channel_id, Some(channel_id));
    }

    #[test]
    fn test_thread_panel_with_reply_count() {
        let channel_id = ChannelId::new();
        let root_id = MessageId::new();
        let panel = ThreadPanel::with_reply_count(channel_id, root_id, 10);

        assert_eq!(panel.channel_id, channel_id);
        assert_eq!(panel.root_message_id, root_id);
        assert_eq!(panel.reply_count, 10);
    }
}
