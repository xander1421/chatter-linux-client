//! UI bridge.
//!
//! Rust to Slint data bridge for reactive state synchronization.

use crate::events::EventBus;
use crate::state::{AuthState, UIState, UIStateData};

/// UI bridge connecting Rust state stores to Slint UI.
///
/// The bridge holds references to application state stores and the event bus,
/// providing methods to:
/// - Access state for UI callback handlers
/// - Subscribe to state changes for pushing to Slint properties
///
/// # Architecture
///
/// The bridge is designed to be used from the main binary where `slint::include_modules!()`
/// is called. The main binary:
/// 1. Creates the UiBridge with state stores
/// 2. Creates the MainWindow (from slint::include_modules!)
/// 3. Wires callbacks using bridge.ui_state(), bridge.auth_state(), etc.
/// 4. Spawns a sync task using bridge.subscribe_ui_state()
///
/// This design avoids having the library depend on Slint-generated types,
/// which would create duplicate type definitions.
///
/// # Example
///
/// ```ignore
/// // In main.rs:
/// slint::include_modules!();
///
/// let bridge = UiBridge::new(ui_state, auth_state, event_bus);
/// let window = MainWindow::new().unwrap();
///
/// // Wire callbacks
/// let ui_state = bridge.ui_state().clone();
/// window.on_toggle_sidebar(move || {
///     ui_state.toggle_sidebar();
/// });
///
/// // Spawn sync task
/// let weak_window = window.as_weak();
/// let mut rx = bridge.subscribe_ui_state();
/// tokio::spawn(async move {
///     while let Ok(state) = rx.recv().await {
///         if let Some(w) = weak_window.upgrade() {
///             // Push state to Slint
///         } else {
///             break;
///         }
///     }
/// });
///
/// window.run().unwrap();
/// ```
pub struct UiBridge {
    /// UI preferences state (theme, compact mode, modals).
    ui_state: UIState,
    /// Authentication state (current user, login status).
    auth_state: AuthState,
    /// Event bus for domain/UI events.
    event_bus: EventBus,
}

impl UiBridge {
    /// Create a new UI bridge with the given state stores and event bus.
    ///
    /// The bridge takes ownership of cloned state handles, not the originals,
    /// since `UIState`, `AuthState`, and `EventBus` all implement `Clone`
    /// by sharing underlying Arc'd state.
    #[must_use]
    pub fn new(ui_state: UIState, auth_state: AuthState, event_bus: EventBus) -> Self {
        Self {
            ui_state,
            auth_state,
            event_bus,
        }
    }

    /// Get a reference to the UI state.
    ///
    /// Use this to:
    /// - Read current state: `bridge.ui_state().get()`
    /// - Update state in callbacks: `bridge.ui_state().toggle_sidebar()`
    #[must_use]
    pub fn ui_state(&self) -> &UIState {
        &self.ui_state
    }

    /// Get a reference to the auth state.
    ///
    /// Use this to:
    /// - Check login status: `bridge.auth_state().is_logged_in()`
    /// - Read user info: `bridge.auth_state().get().display_name`
    #[must_use]
    pub fn auth_state(&self) -> &AuthState {
        &self.auth_state
    }

    /// Get a reference to the event bus.
    ///
    /// Use this to emit UI events or subscribe to domain events.
    #[must_use]
    pub fn event_bus(&self) -> &EventBus {
        &self.event_bus
    }

    /// Subscribe to UI state changes.
    ///
    /// Returns a receiver that emits `UIStateData` whenever the UI state changes.
    /// Use this to push state updates to Slint properties.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut rx = bridge.subscribe_ui_state();
    /// while let Ok(state) = rx.recv().await {
    ///     window.set_sidebar_collapsed(state.sidebar_collapsed);
    /// }
    /// ```
    #[must_use]
    pub fn subscribe_ui_state(&self) -> tokio::sync::broadcast::Receiver<UIStateData> {
        self.ui_state.subscribe()
    }

    /// Subscribe to auth state changes.
    ///
    /// Returns a receiver that emits `AuthStateData` whenever the auth state changes.
    /// Use this to update login UI, show/hide authenticated views, etc.
    #[must_use]
    pub fn subscribe_auth_state(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::state::AuthStateData> {
        self.auth_state.subscribe()
    }
}

impl Clone for UiBridge {
    fn clone(&self) -> Self {
        Self {
            ui_state: self.ui_state.clone(),
            auth_state: self.auth_state.clone(),
            event_bus: self.event_bus.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ui_bridge_new() {
        let ui_state = UIState::new();
        let auth_state = AuthState::new();
        let event_bus = EventBus::new();

        let bridge = UiBridge::new(ui_state, auth_state, event_bus);

        // Bridge should hold the state references
        assert!(!bridge.auth_state().is_logged_in());
    }

    #[test]
    fn test_ui_bridge_clone_shares_state() {
        let ui_state = UIState::new();
        let auth_state = AuthState::new();
        let event_bus = EventBus::new();

        let bridge1 = UiBridge::new(ui_state, auth_state, event_bus);
        let bridge2 = bridge1.clone();

        // Update via one bridge
        bridge1.ui_state().set_compact_mode(true);

        // Should be visible via the other
        assert!(bridge2.ui_state().get().compact_mode);
    }

    #[test]
    fn test_ui_bridge_accessors() {
        let ui_state = UIState::new();
        let auth_state = AuthState::new();
        let event_bus = EventBus::new();

        let bridge = UiBridge::new(ui_state, auth_state, event_bus);

        // All accessors should work
        let _ = bridge.ui_state();
        let _ = bridge.auth_state();
        let _ = bridge.event_bus();
    }

    #[tokio::test]
    async fn test_subscribe_ui_state() {
        let ui_state = UIState::new();
        let auth_state = AuthState::new();
        let event_bus = EventBus::new();

        let bridge = UiBridge::new(ui_state, auth_state, event_bus);
        let mut rx = bridge.subscribe_ui_state();

        // Update state
        bridge.ui_state().set_compact_mode(true);

        // Should receive the update
        let state = rx.recv().await.unwrap();
        assert!(state.compact_mode);
    }

    #[tokio::test]
    async fn test_subscribe_auth_state() {
        use crate::domain::ids::UserId;

        let ui_state = UIState::new();
        let auth_state = AuthState::new();
        let event_bus = EventBus::new();

        let bridge = UiBridge::new(ui_state, auth_state, event_bus);
        let mut rx = bridge.subscribe_auth_state();

        // Update state
        let user_id = UserId::new();
        bridge
            .auth_state()
            .set_logged_in(user_id, 1, "Test".to_string());

        // Should receive the update
        let state = rx.recv().await.unwrap();
        assert_eq!(state.current_user_id, Some(user_id));
    }
}
