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
//! - **UIState**: UI preferences (theme, compact mode, modals)
//!
//! # Usage
//!
//! ```ignore
//! use chatter_core::state::{AuthState, UIState, ModalType};
//!
//! // Create state managers
//! let auth_state = AuthState::new();
//! let ui_state = UIState::new();
//!
//! // Subscribe to changes
//! let mut auth_rx = auth_state.subscribe();
//! let mut ui_rx = ui_state.subscribe();
//!
//! // Update state (triggers subscriber notifications)
//! auth_state.set_logged_in(user_id, device_id, "Alice".to_string());
//! ui_state.open_modal(ModalType::Settings);
//! ```

pub mod auth_state;
pub mod chat_state;
pub mod message_state;
pub mod offline_state;
pub mod privacy_state;
pub mod store;
pub mod user_state;
#[cfg(feature = "jarvis")]
pub mod jarvis_state;

// Re-export core state types
pub use auth_state::{AuthState, AuthStateData};
pub use chat_state::{
    ActiveConversation, ChatState, ChatStateData, PendingAttachment, PinnedSubstate,
    ReactionSummary, StreamingSubstate, ThreadPanel,
};
pub use message_state::{MessageState, MessageStateData, PaginationCursor};
pub use privacy_state::{ConversationPrivacySettings, PrivacyState, PrivacyStateData};
pub use store::{StateSnapshot, Store};
pub use user_state::{UserState, UserStateData};

// Offline state re-exports
pub use offline_state::{OfflineState, OfflineStateData};

// Jarvis AI assistant state re-exports
#[cfg(feature = "jarvis")]
pub use jarvis_state::{JarvisMessage, JarvisRole, JarvisState};
