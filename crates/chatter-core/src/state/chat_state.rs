//! Chat state.
//!
//! Active channel, threads (up to 3 concurrent), pinned-bar navigation,
//! and streaming ephemera (typing indicators, reactions). All of these are
//! gated on the active conversation, so they live in a single `Store` for
//! one subscription stream and one source of truth.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::domain::anon_dm::AnonDmId;
use crate::domain::anon_group::AnonGroupId;
use crate::domain::ids::{AttachmentId, ChannelId, MessageId, UserId};
use crate::state::store::Store;

// ============================================================================
// Active Conversation (typed, replaces string parsing)
// ============================================================================

/// The currently active conversation, fully typed.
///
/// Replaces the pattern of checking `get_selected_dm_id()` string prefixes
/// and `get_selected_private_group_id().is_empty()` scattered across 109 sites.
/// Set once at sidebar selection time, read everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveConversation {
    /// No conversation selected.
    None,
    /// A server-managed channel (public/private).
    Channel(ChannelId),
    /// A DM to a friend (Signal Protocol E2EE). Channel-free — there is no
    /// server-side DM channel. Keyed entirely by the peer's `UserId`; the
    /// local `conversation_id` (`ChannelId`) is derived via
    /// `ChannelId::from(peer_id)`. ONE conversation whichever transport
    /// carries it: sealed-sender streaming normally, the NDSS'21 mailbox in
    /// Shield mode (`MessageService::send_friend_dm_content`).
    ServerDm { peer_id: UserId },
    /// An anonymous DM (NDSS'21 mailbox, pairwise, HPKE bootstrap).
    /// Server cannot link conversation to registered users.
    AnonDm(AnonDmId),
    /// An anonymous group (NDSS'21 mailbox, fanout).
    AnonGroup(AnonGroupId),
}

impl Default for ActiveConversation {
    fn default() -> Self {
        Self::None
    }
}

impl ActiveConversation {
    /// Get the ChannelId for this conversation (if applicable).
    ///
    /// Channels and DMs have server-assigned ChannelIds.
    /// Anon types can be converted to synthetic ChannelIds.
    pub fn channel_id(&self) -> Option<ChannelId> {
        match self {
            Self::Channel(id) => Some(*id),
            Self::ServerDm { peer_id } => Some(ChannelId::from(*peer_id)),
            Self::AnonDm(id) => Some((*id).into()),
            Self::AnonGroup(id) => Some((*id).into()),
            Self::None => None,
        }
    }

    /// Whether an event or message keyed by `id` belongs to this
    /// conversation's open view. The ids of different conversation kinds are
    /// distinct UUIDs, so the id alone decides.
    pub fn is_for_channel(&self, id: ChannelId) -> bool {
        self.channel_id() == Some(id)
    }

    /// The id the SIDEBAR uses for this conversation — the same string the
    /// context menus send to `move_to_section` / the pin service.
    ///
    /// Not the same as [`channel_id`](Self::channel_id): a server DM is
    /// channel-free and is keyed by the peer's `UserId`, which is what the DM
    /// row carries. Pin state keyed any other way would not match the row.
    pub fn sidebar_key(&self) -> Option<String> {
        match self {
            Self::Channel(id) => Some(id.to_string()),
            Self::ServerDm { peer_id } => Some(peer_id.to_string()),
            Self::AnonDm(id) => Some(id.to_string()),
            Self::AnonGroup(id) => Some(id.to_string()),
            Self::None => None,
        }
    }

    /// The `(type, id)` pair that names this conversation in the sidebar's
    /// routing vocabulary, and the pair call history stores so a past call
    /// can be redialled.
    ///
    /// The type here is the *variant's* type, not the message-storage type
    /// [`conversation_type`](Self::conversation_type) returns — an encrypted
    /// channel stores its messages as `AnonGroup` but is still routed as
    /// `Channel(id)`, and round-tripping through the storage type would
    /// reopen it as an anonymous group.
    pub fn routing_key(&self) -> Option<(crate::domain::message::ConversationType, String)> {
        use crate::domain::message::ConversationType as CT;
        let ct = match self {
            Self::Channel(_) => CT::Channel,
            Self::ServerDm { .. } => CT::ServerDm,
            Self::AnonDm(_) => CT::AnonDm,
            Self::AnonGroup(_) => CT::AnonGroup,
            Self::None => return None,
        };
        self.sidebar_key().map(|id| (ct, id))
    }

    /// Inverse of [`routing_key`](Self::routing_key).
    pub fn from_routing_key(
        conversation_type: &crate::domain::message::ConversationType,
        id: &str,
    ) -> Option<Self> {
        use crate::domain::message::ConversationType as CT;
        match conversation_type {
            CT::Channel | CT::SavedMessages => id.parse().ok().map(Self::Channel),
            CT::ServerDm => id.parse().ok().map(|peer_id| Self::ServerDm { peer_id }),
            CT::AnonDm => id.parse().ok().map(Self::AnonDm),
            CT::AnonGroup => id.parse().ok().map(Self::AnonGroup),
            CT::Mesh | CT::MeshDm => None,
        }
    }

    /// Check if any conversation is active.
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Check if this is an anonymous conversation (AnonDm or AnonGroup).
    pub fn is_anon(&self) -> bool {
        matches!(self, Self::AnonDm(_) | Self::AnonGroup(_))
    }

    /// Get the ConversationType for this active conversation.
    ///
    /// Returns None for ActiveConversation::None.
    ///
    /// `channel_repo` is needed for the `Channel(id)` variant — its
    /// ConversationType comes from the channel row (`Channel`, `ServerDm`
    /// or `SavedMessages`). Encryption does not change it: an encrypted
    /// server channel is still `Channel`, never `AnonGroup`.
    pub fn conversation_type(
        &self,
        channel_repo: &crate::storage::ChannelRepository,
    ) -> Option<crate::domain::message::ConversationType> {
        use crate::domain::message::ConversationType;
        match self {
            Self::Channel(id) => Some(
                crate::services::message::conversion::conversation_type_for_channel(
                    channel_repo,
                    id,
                ),
            ),
            Self::ServerDm { .. } => Some(ConversationType::ServerDm),
            Self::AnonDm(_) => Some(ConversationType::AnonDm),
            Self::AnonGroup(_) => Some(ConversationType::AnonGroup),
            Self::None => None,
        }
    }

    // ========================================================================
    // Selection predicates
    //
    // Typed "is this conversation the active one?" checks used by the UI to
    // highlight sidebar items, gate unread increments, and route receipts
    // without string-comparing Slint window properties.
    // ========================================================================

    /// True if the active conversation is the given regular channel.
    pub fn matches_channel(&self, channel_id: &ChannelId) -> bool {
        matches!(self, Self::Channel(id) if id == channel_id)
    }

    /// True if the active conversation is a DM matching either the channel
    /// or the peer. Used by the DM sidebar where items are identified by
    /// `channel_id` and the header by `peer_id`.
    pub fn matches_dm(&self, channel_id: &ChannelId, peer_id: &UserId) -> bool {
        match self {
            Self::ServerDm { peer_id: pid } => {
                ChannelId::from(*pid) == *channel_id || pid == peer_id
            }
            _ => false,
        }
    }

    /// True if the active conversation is a DM with the given peer.
    pub fn matches_dm_peer(&self, peer_id: &UserId) -> bool {
        matches!(self, Self::ServerDm { peer_id: pid } if pid == peer_id)
    }

    /// True if the active conversation is the given anonymous DM.
    pub fn matches_anon_dm(&self, id: &AnonDmId) -> bool {
        matches!(self, Self::AnonDm(anon) if anon == id)
    }

    /// True if the active conversation is the given anonymous group.
    pub fn matches_anon_group(&self, id: &AnonGroupId) -> bool {
        matches!(self, Self::AnonGroup(g) if g == id)
    }
}

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
    /// Pre-uploaded attachment pointer (for GIFs from picker).
    /// If set, the attachment is already uploaded and this pointer should be used directly.
    pub pointer: Option<crate::proto::content_v1::AttachmentPointer>,
}

impl PendingAttachment {
    /// Create a new pending attachment (will be uploaded when sent).
    #[must_use]
    pub fn new(
        id: AttachmentId,
        filename: String,
        path: PathBuf,
        size: u64,
        mime_type: String,
    ) -> Self {
        Self {
            id,
            filename,
            path,
            size,
            mime_type,
            pointer: None,
        }
    }

    /// Create a pending attachment with a pre-uploaded pointer.
    /// Used for GIFs from the picker that are already uploaded to CDN.
    #[must_use]
    pub fn with_pointer(
        id: AttachmentId,
        path: PathBuf,
        filename: String,
        size: u64,
        mime_type: String,
        pointer: crate::proto::content_v1::AttachmentPointer,
    ) -> Self {
        Self {
            id,
            filename,
            path,
            size,
            mime_type,
            pointer: Some(pointer),
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
}

// ============================================================================
// Pinned messages bar (Telegram-style navigation)
// ============================================================================

/// Pinned messages bar state.
///
/// Tracks the pinned messages for the current channel and navigation
/// position through them (Telegram-style). Lives inside [`ChatStateData`]
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
/// Lives inside [`ChatStateData`] as `streaming`. The `TypingAggregator` and
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

    fn switch_attachment_tray(state: &mut ChatStateData, entering: Option<ChannelId>) {
        if state.active_channel_id == entering {
            return;
        }
        let leaving = std::mem::take(&mut state.pending_attachments);
        if !leaving.is_empty() {
            state
                .attachment_trays
                .insert(state.active_channel_id, leaving);
        }
        state.pending_attachments = state.attachment_trays.remove(&entering).unwrap_or_default();
    }

    /// Clear every account-local tray during session reset.
    pub fn clear_attachment_trays(&self) {
        self.store.update(|state| {
            state.pending_attachments.clear();
            state.attachment_trays.clear();
            state.attachment_revision = state.attachment_revision.wrapping_add(1);
        });
    }

    /// Remove a deleted conversation's tray, including the active tray.
    pub fn take_attachment_tray_for(
        &self,
        conversation: ActiveConversation,
    ) -> Vec<PendingAttachment> {
        let key = conversation.channel_id();
        let mut taken = Vec::new();
        self.store.update(|state| {
            if state.active_conversation == conversation && state.active_channel_id == key {
                taken = std::mem::take(&mut state.pending_attachments);
            } else if let Some(tray) = state.attachment_trays.remove(&key) {
                taken = tray;
            }
        });
        taken
    }

    /// Stage an asynchronous selection under the conversation that requested it.
    pub fn add_pending_attachment_for(
        &self,
        conversation: ActiveConversation,
        revision: u64,
        attachment: PendingAttachment,
    ) {
        self.store.update(|state| {
            if state.attachment_revision != revision {
                return;
            }
            let key = conversation.channel_id();
            if state.active_channel_id == key {
                state.pending_attachments.push(attachment);
            } else {
                state
                    .attachment_trays
                    .entry(key)
                    .or_default()
                    .push(attachment);
            }
        });
    }

    /// Get the current active conversation.
    pub fn active_conversation(&self) -> ActiveConversation {
        self.store.get().active_conversation
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

    /// Add a pending attachment for the current message.
    pub fn add_pending_attachment(&self, attachment: PendingAttachment) {
        self.store.update(|state| {
            state.pending_attachments.push(attachment);
        });
    }

    /// Get all pending attachments.
    #[must_use]
    pub fn get_pending_attachments(&self) -> Vec<PendingAttachment> {
        self.store.get().pending_attachments.clone()
    }

    /// Take the staged attachments for a send to `conversation`, in one
    /// update: `None`, with nothing taken, when another conversation has
    /// become active or the account-local attachment revision has changed.
    /// Each conversation owns its tray; a stale send cannot consume new-account files.
    pub fn take_pending_attachments_for(
        &self,
        conversation: ActiveConversation,
        revision: u64,
    ) -> Option<Vec<PendingAttachment>> {
        let mut taken = None;
        self.store.update(|state| {
            if state.active_conversation == conversation
                && state.active_channel_id == conversation.channel_id()
                && state.attachment_revision == revision
            {
                taken = Some(std::mem::take(&mut state.pending_attachments));
            }
        });
        taken
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
    fn is_for_channel_matches_only_its_own_conversation() {
        let (peer, other) = (UserId::new(), UserId::new());
        let server_dm = ActiveConversation::ServerDm { peer_id: peer };
        assert!(server_dm.is_for_channel(ChannelId::from(peer)));
        assert!(!server_dm.is_for_channel(ChannelId::from(other)));

        let channel = ChannelId::new();
        assert!(ActiveConversation::Channel(channel).is_for_channel(channel));
        assert!(!ActiveConversation::Channel(channel).is_for_channel(ChannelId::new()));
        assert!(!ActiveConversation::None.is_for_channel(channel));
    }

    /// Call history stores `routing_key()` and redials through
    /// `from_routing_key()`. Every conversation that can host a call has to
    /// survive that round trip, or "call back" opens the wrong thing.
    #[test]
    fn routing_key_round_trips_every_call_capable_conversation() {
        let cases = [
            ActiveConversation::Channel(ChannelId::new()),
            ActiveConversation::ServerDm {
                peer_id: UserId::new(),
            },
            ActiveConversation::AnonDm(AnonDmId::new()),
            ActiveConversation::AnonGroup(AnonGroupId::new()),
        ];
        for conv in cases {
            let (ct, id) = conv
                .routing_key()
                .expect("call-capable conversation has a key");
            assert_eq!(ActiveConversation::from_routing_key(&ct, &id), Some(conv));
        }
        assert!(ActiveConversation::None.routing_key().is_none());
    }

    /// A fresh launch is in view; the platform lifecycle flips the flag.
    #[test]
    fn app_visible_starts_true_and_follows_lifecycle() {
        let state = ChatState::new();
        assert!(state.is_app_visible());
        assert!(state.set_app_visible(false), "starts visible");
        assert!(!state.is_app_visible());
        assert!(!state.set_app_visible(true));
        assert!(state.is_app_visible());
    }

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

    // ========================================================================
    // Pinned-bar substate tests (relocated from pinned_state.rs)
    // ========================================================================
    mod pinned_tests {
        use super::*;

        #[test]
        fn test_pinned_state_default() {
            let state = ChatState::new();
            let data = state.get();
            assert!(data.pinned.pinned_message_ids.is_empty());
            assert_eq!(data.pinned.current_index, 0);
            assert!(!data.pinned.is_dismissed);
        }

        #[test]
        fn test_set_pinned_messages() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2]);
            let data = state.get();
            assert_eq!(data.pinned.pinned_message_ids.len(), 2);
            assert_eq!(data.pinned.current_index, 0);
        }

        #[test]
        fn test_set_pinned_messages_resets_index() {
            let state = ChatState::new();

            state.set_pinned_messages(vec![MessageId::new(), MessageId::new(), MessageId::new()]);
            state.pinned_navigate_to(2);
            assert_eq!(state.get().pinned.current_index, 2);

            // Setting new messages should reset index
            state.set_pinned_messages(vec![MessageId::new()]);
            assert_eq!(state.get().pinned.current_index, 0);
        }

        #[test]
        fn test_clear_pinned() {
            let state = ChatState::new();

            state.set_pinned_messages(vec![MessageId::new()]);
            state.pinned_dismiss();
            state.clear_pinned();

            let data = state.get();
            assert!(data.pinned.pinned_message_ids.is_empty());
            assert_eq!(data.pinned.current_index, 0);
            assert!(!data.pinned.is_dismissed);
        }

        #[test]
        fn test_navigate_next_wraparound() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();
            let msg3 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2, msg3]);

            assert_eq!(state.pinned_current_message_id(), Some(msg1));

            state.pinned_navigate_next();
            assert_eq!(state.pinned_current_message_id(), Some(msg2));

            state.pinned_navigate_next();
            assert_eq!(state.pinned_current_message_id(), Some(msg3));

            // Wrap around
            state.pinned_navigate_next();
            assert_eq!(state.pinned_current_message_id(), Some(msg1));
        }

        #[test]
        fn test_navigate_prev_wraparound() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();
            let msg3 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2, msg3]);

            // Start at first, go back to wrap to last
            state.pinned_navigate_prev();
            assert_eq!(state.pinned_current_message_id(), Some(msg3));

            state.pinned_navigate_prev();
            assert_eq!(state.pinned_current_message_id(), Some(msg2));

            state.pinned_navigate_prev();
            assert_eq!(state.pinned_current_message_id(), Some(msg1));
        }

        #[test]
        fn test_navigate_empty_list() {
            let state = ChatState::new();

            // Should not panic
            state.pinned_navigate_next();
            state.pinned_navigate_prev();
            state.pinned_navigate_to(5);

            assert!(state.pinned_current_message_id().is_none());
        }

        #[test]
        fn test_navigate_to() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();
            let msg3 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2, msg3]);

            state.pinned_navigate_to(2);
            assert_eq!(state.pinned_current_message_id(), Some(msg3));

            // Out of bounds - should clamp
            state.pinned_navigate_to(100);
            assert_eq!(state.pinned_current_message_id(), Some(msg3));
        }

        #[test]
        fn test_should_show() {
            let state = ChatState::new();

            // No pinned messages - should not show
            assert!(!state.pinned_should_show());

            // Has pinned messages - should show
            state.set_pinned_messages(vec![MessageId::new()]);
            assert!(state.pinned_should_show());

            // Dismissed - should not show
            state.pinned_dismiss();
            assert!(!state.pinned_should_show());

            // New channel resets dismissed
            state.set_pinned_messages(vec![MessageId::new()]);
            assert!(state.pinned_should_show());
        }

        #[test]
        fn test_current_message_id() {
            let state = ChatState::new();
            assert!(state.pinned_current_message_id().is_none());
            let msg = MessageId::new();
            state.set_pinned_messages(vec![msg]);

            assert_eq!(state.pinned_current_message_id(), Some(msg));
        }

        #[test]
        fn test_total_count() {
            let state = ChatState::new();
            assert_eq!(state.pinned_total_count(), 0);
            state.set_pinned_messages(vec![MessageId::new(), MessageId::new()]);

            assert_eq!(state.pinned_total_count(), 2);
        }

        #[test]
        fn test_add_pinned_message() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();

            state.set_pinned_messages(vec![msg1]);
            state.add_pinned_message(msg2);

            let data = state.get();
            assert_eq!(data.pinned.pinned_message_ids.len(), 2);
            assert!(data.pinned.pinned_message_ids.contains(&msg2));

            // Adding duplicate should not add again
            state.add_pinned_message(msg1);
            assert_eq!(state.pinned_total_count(), 2);
        }

        #[test]
        fn test_remove_pinned_message() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();
            let msg3 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2, msg3]);
            state.pinned_navigate_to(2); // Focus msg3

            state.remove_pinned_message(&msg2);

            let data = state.get();
            assert_eq!(data.pinned.pinned_message_ids.len(), 2);
            assert!(!data.pinned.pinned_message_ids.contains(&msg2));
            // Index should still be valid (pointing to msg3, now at index 1)
            assert_eq!(data.pinned.current_index, 1);
        }

        #[test]
        fn test_remove_pinned_message_adjusts_index() {
            let state = ChatState::new();
            let msg1 = MessageId::new();
            let msg2 = MessageId::new();

            state.set_pinned_messages(vec![msg1, msg2]);
            state.pinned_navigate_to(1); // Focus msg2

            // Remove msg2 (the one at current index)
            state.remove_pinned_message(&msg2);

            let data = state.get();
            assert_eq!(data.pinned.pinned_message_ids.len(), 1);
            assert_eq!(data.pinned.current_index, 0); // Adjusted to valid range
        }

        #[test]
        fn test_remove_last_pinned_message() {
            let state = ChatState::new();
            let msg = MessageId::new();

            state.set_pinned_messages(vec![msg]);
            state.remove_pinned_message(&msg);

            let data = state.get();
            assert!(data.pinned.pinned_message_ids.is_empty());
            assert_eq!(data.pinned.current_index, 0);
        }

        #[tokio::test]
        async fn test_pinned_state_subscribe() {
            let state = ChatState::new();
            let mut rx = state.subscribe();
            let msg = MessageId::new();

            state.set_pinned_messages(vec![msg]);

            let received = rx.recv().await.unwrap();
            assert_eq!(received.pinned.pinned_message_ids.len(), 1);
        }

        #[test]
        fn test_pinned_state_clone_shares_state() {
            let state1 = ChatState::new();
            let state2 = state1.clone();

            state1.set_pinned_messages(vec![MessageId::new(), MessageId::new()]);
            assert_eq!(state2.pinned_total_count(), 2);

            state1.pinned_navigate_next();
            assert_eq!(state2.get().pinned.current_index, 1);
        }
    }

    // ========================================================================
    // Streaming substate tests (relocated from streaming_state.rs)
    // ========================================================================
    mod streaming_tests {
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

        #[test]
        fn test_typing_started_older_than_stop_is_rejected() {
            let state = ChatState::new();
            let (c, u) = (make_channel_id(), make_user_id());
            assert!(state.accept_typing(c, u, 100, true));
            assert!(state.accept_typing(c, u, 200, false));
            // A STARTED sent before that STOPPED, arriving after it.
            assert!(!state.accept_typing(c, u, 150, true));
            assert!(!state.accept_typing(c, u, 200, true));
            // A late older STOPPED does not move the mark back.
            assert!(state.accept_typing(c, u, 120, false));
            assert!(!state.accept_typing(c, u, 180, true));
            assert!(state.accept_typing(c, u, 201, true));
            // Scoped per sender and per conversation; 0 = no timestamp.
            assert!(state.accept_typing(make_channel_id(), u, 150, true));
            assert!(state.accept_typing(c, make_user_id(), 150, true));
            assert!(state.accept_typing(c, u, 0, true));
        }

        // ==================== Typing Tests ====================

        #[test]
        fn test_set_and_get_typing_users() {
            let state = ChatState::new();
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
            let state = ChatState::new();
            let channel_id = make_channel_id();
            let user1 = make_user_id();

            state.set_typing_users(channel_id.clone(), vec![user1]);
            assert!(!state.get_typing_users(&channel_id).is_empty());

            state.clear_typing(&channel_id);
            assert!(state.get_typing_users(&channel_id).is_empty());
        }

        #[test]
        fn test_set_empty_typing_removes_entry() {
            let state = ChatState::new();
            let channel_id = make_channel_id();
            let user1 = make_user_id();

            state.set_typing_users(channel_id.clone(), vec![user1]);
            state.set_typing_users(channel_id.clone(), vec![]);

            // Should be removed from the map
            assert!(
                state
                    .get()
                    .streaming
                    .typing_by_channel
                    .get(&channel_id)
                    .is_none()
            );
        }

        // ==================== Reaction Tests ====================

        #[test]
        fn test_update_reaction_adds_new() {
            let state = ChatState::new();
            let message_id = make_message_id();

            // Add new reaction to message with no reactions
            state.update_reaction(&message_id, "\u{1f389}".to_string(), 1, Some(true));

            let reactions = state.get_reactions(&message_id).unwrap();
            assert_eq!(reactions.len(), 1);
            assert_eq!(reactions[0].emoji, "\u{1f389}");
            assert_eq!(reactions[0].count, 1);
            assert!(reactions[0].user_reacted);
        }

        #[test]
        fn test_update_reaction_ignores_negative_new() {
            let state = ChatState::new();
            let message_id = make_message_id();

            // Trying to decrement a non-existent reaction should do nothing
            state.update_reaction(&message_id, "\u{1f44d}".to_string(), -1, None);

            assert!(state.get_reactions(&message_id).is_none());
        }

        // ==================== General Tests ====================

        #[tokio::test]
        async fn test_subscribe_receives_typing_updates() {
            let state = ChatState::new();
            let mut rx = state.subscribe();

            let channel_id = make_channel_id();
            let user1 = make_user_id();

            state.set_typing_users(channel_id.clone(), vec![user1.clone()]);

            let received = rx.recv().await.unwrap();
            let typers = received
                .streaming
                .typing_by_channel
                .get(&channel_id)
                .unwrap();
            assert_eq!(typers.len(), 1);
            assert!(typers.contains(&user1));
        }

        #[test]
        fn test_streaming_default_impl() {
            let state = ChatState::default();
            let data = state.get();
            assert!(data.streaming.typing_by_channel.is_empty());
            assert!(data.streaming.recent_reactions.is_empty());
            assert!(data.streaming.last_typing_update.is_none());
        }

        #[test]
        fn test_streaming_clone_shares_state() {
            let state1 = ChatState::new();
            let state2 = state1.clone();

            let channel_id = make_channel_id();
            let user1 = make_user_id();

            state1.set_typing_users(channel_id.clone(), vec![user1.clone()]);

            // Both should see the update
            assert_eq!(state2.get_typing_users(&channel_id), vec![user1]);
        }
    }
}

#[cfg(test)]
mod attachment_tray_tests {
    use super::*;

    fn file(name: &str) -> PendingAttachment {
        PendingAttachment::new(
            AttachmentId::new(),
            name.into(),
            Default::default(),
            1,
            "image/png".into(),
        )
    }

    #[test]
    fn delayed_selection_stays_with_its_requesting_conversation() {
        let state = ChatState::new();
        let a = ActiveConversation::Channel(ChannelId::new());
        let b = ActiveConversation::ServerDm {
            peer_id: UserId::new(),
        };
        state.set_active_conversation(a);
        let revision = state.get().attachment_revision;
        state.set_active_conversation(b);
        state.add_pending_attachment(file("b.png"));
        state.add_pending_attachment_for(a, revision, file("a.png"));
        assert_eq!(state.get_pending_attachments()[0].filename, "b.png");
        state.set_active_conversation(a);
        assert_eq!(state.get_pending_attachments()[0].filename, "a.png");
        assert!(state.take_pending_attachments_for(b, revision).is_none());
        assert_eq!(
            state.take_pending_attachments_for(a, revision).unwrap()[0].filename,
            "a.png"
        );
        state.set_active_conversation(b);
        assert_eq!(state.get_pending_attachments()[0].filename, "b.png");
    }

    #[test]
    fn session_reset_clears_hidden_trays_and_rejects_late_selections() {
        let state = ChatState::new();
        let a = ActiveConversation::AnonDm(AnonDmId::new());
        state.set_active_conversation(a);
        let revision = state.get().attachment_revision;
        state.add_pending_attachment(file("private.png"));
        state.set_active_conversation(ActiveConversation::None);
        state.clear_attachment_trays();
        state.add_pending_attachment_for(a, revision, file("late.png"));
        state.set_active_conversation(a);
        assert!(state.get_pending_attachments().is_empty());
        assert!(state.get().attachment_trays.is_empty());
    }

    #[test]
    fn a_stale_take_cannot_consume_a_new_sessions_tray_in_the_same_conversation() {
        let state = ChatState::new();
        let conversation = ActiveConversation::Channel(ChannelId::new());
        state.set_active_conversation(conversation);
        let old_revision = state.get().attachment_revision;
        state.add_pending_attachment(file("old.png"));
        state.clear_attachment_trays();
        state.add_pending_attachment(file("new.png"));
        assert!(
            state
                .take_pending_attachments_for(conversation, old_revision)
                .is_none()
        );
        assert_eq!(state.get_pending_attachments()[0].filename, "new.png");
        assert_eq!(
            state
                .take_pending_attachments_for(conversation, state.get().attachment_revision)
                .unwrap()[0]
                .filename,
            "new.png",
        );
    }
}
