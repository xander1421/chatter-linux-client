//! Application state management.
//!
//! Reactive state management for UI binding.
//!
//! # Architecture
//!
//! The state layer provides reactive containers that bridge services and UI:
//!
//! - **Store<T>**: Generic reactive container with subscription support
//! - **AuthState**: Authentication status (logged in user, device, progress)
//! - **ChatState**: Active conversation, thread panels, pinned bar, typing/reactions
//! - **MessageState**: Per-channel message cache and pagination
//! - **OfflineState**: Server reachability for the connection banner
//! - **UserState**: User cache
//! - **JarvisState** (`jarvis` feature): Jarvis conversation history

pub mod auth_state;
pub(crate) mod receive_session;
pub mod chat_state;
#[cfg(feature = "jarvis")]
pub mod jarvis_state;
pub mod message_state;
pub mod offline_state;
pub mod store;
pub mod user_state;

// Re-export core state types
pub use auth_state::{AuthState, AuthStateData};
pub use chat_state::{
    ActiveConversation, ChatState, ChatStateData, PendingAttachment, PinnedSubstate,
    ReactionSummary, StreamingSubstate, ThreadPanel,
};
pub use message_state::{MessageState, MessageStateData, PaginationCursor};
pub use store::{StateSnapshot, Store};
pub use user_state::{UserState, UserStateData};

// Offline state re-exports
pub use offline_state::{OfflineState, OfflineStateData};

// Jarvis AI assistant state re-exports
#[cfg(feature = "jarvis")]
pub use jarvis_state::{JarvisMessage, JarvisRole, JarvisState};
