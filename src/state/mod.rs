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
//! use chatter_linux::state::{AuthState, UIState, ModalType};
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

pub mod app_state;
pub mod auth_state;
pub mod call_state;
pub mod chat_state;
pub mod message_state;
pub mod offline_state;
pub mod pinned_state;
pub mod privacy_state;
pub mod store;
pub mod streaming_state;
pub mod ui_state;
pub mod user_state;

// Re-export core state types
pub use auth_state::{AuthState, AuthStateData};
pub use store::Store;
pub use ui_state::{ModalType, Theme, UIState, UIStateData};
