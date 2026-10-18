//! Chat state.
//!
//! Active channel, threads (up to 3 concurrent), pinned-bar navigation,
//! and streaming ephemera (typing indicators, reactions). All of these are
//! gated on the active conversation, so they live in a single `Store` for
//! one subscription stream and one source of truth.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::domain::ids::{ChannelId, UserId};
use crate::state::store::Store;

mod attachments;
mod conversation;
mod pinned;
mod streaming;
mod thread;

#[cfg(test)]
mod tests;

pub use attachments::PendingAttachment;
pub use conversation::ActiveConversation;
pub use pinned::PinnedSubstate;
pub use streaming::{ReactionSummary, StreamingSubstate};
pub use thread::ThreadPanel;

/// Chat navigation state data.
///
/// Tracks the currently active channel, open thread panels, pending
/// attachments, the pinned-bar navigation substate, and streaming ephemera.
#[derive(Clone, Debug, Default)]
pub struct ChatStateData {
    /// Currently selected channel ID.
    pub active_channel_id: Option<ChannelId>,
    /// Typed active conversation (replaces string-based dispatch).
    pub active_conversation: ActiveConversation,
    /// Open thread panels (max 3 concurrent).
    pub open_threads: Vec<ThreadPanel>,
    /// Index of the focused thread panel (0, 1, or 2), if any.
    pub active_thread_index: Option<usize>,
    /// Pending attachments for the current message being composed.
    pub pending_attachments: Vec<PendingAttachment>,
    /// Inactive conversation trays; navigation swaps these atomically.
    pub attachment_trays: HashMap<Option<ChannelId>, Vec<PendingAttachment>>,
    /// Invalidates asynchronous file selections when the account session resets.
    pub attachment_revision: u64,
    /// Pinned-bar navigation state for the active channel.
    pub pinned: PinnedSubstate,
    /// Real-time ephemera (typing indicators + recent reactions cache).
    pub streaming: StreamingSubstate,
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
    /// Latest sender timestamp that ended each sender's typing in each
    /// conversation (a STOPPED, or a message). Kept out of `store` so
    /// recording one does not wake every state subscriber.
    typing_ended_at: Arc<Mutex<HashMap<(ChannelId, UserId), u64>>>,
    /// Whether the app is on screen (platform lifecycle). Outside `store` for
    /// the same reason: a focus change is not a navigation change.
    app_visible: Arc<AtomicBool>,
}

impl ChatState {
    /// Create a new chat state with default values.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(ChatStateData::default()),
            typing_ended_at: Arc::default(),
            app_visible: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Record whether the app is on screen; returns the previous value.
    pub fn set_app_visible(&self, visible: bool) -> bool {
        self.app_visible.swap(visible, Ordering::AcqRel)
    }

    /// Whether the app is on screen. Starts `true`: a fresh launch is in view.
    pub fn is_app_visible(&self) -> bool {
        self.app_visible.load(Ordering::Acquire)
    }

    /// Get the current chat state.
    #[must_use]
    pub fn get(&self) -> ChatStateData {
        self.store.get()
    }

    /// Subscribe to chat state changes (Arc-wrapped for efficiency).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<super::StateSnapshot<ChatStateData>> {
        self.store.subscribe()
    }

    /// Set the active channel.
    ///
    /// Clears the active thread index when changing channels.
    pub fn set_active_channel(&self, channel_id: ChannelId) {
        self.store.update(|state| {
            Self::switch_attachment_tray(state, Some(channel_id));
            state.active_channel_id = Some(channel_id);
            state.active_thread_index = None;
        });
    }

    /// Set the typed active conversation.
    ///
    /// Also updates `active_channel_id` for backward compatibility.
    pub fn set_active_conversation(&self, conversation: ActiveConversation) {
        self.store.update(|state| {
            Self::switch_attachment_tray(state, conversation.channel_id());
            state.active_channel_id = conversation.channel_id();
            state.active_conversation = conversation;
            state.active_thread_index = None;
        });
    }

    /// Get the current active conversation.
    pub fn active_conversation(&self) -> ActiveConversation {
        self.store.get().active_conversation
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
