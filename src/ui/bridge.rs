//! UI bridge.
//!
//! Rust to Slint data bridge for reactive state synchronization.

use slint::Weak;

use crate::events::EventBus;
use crate::state::{AuthState, UIState};

// Include the generated Slint module to access MainWindow
slint::include_modules!();

/// UI bridge connecting Rust state stores to Slint UI.
///
/// The bridge holds references to application state stores and the event bus,
/// providing methods to:
/// - Wire Slint callbacks to Rust handlers
/// - Push state changes to Slint properties asynchronously
///
/// # Example
///
/// ```ignore
/// let ui_state = UIState::new();
/// let auth_state = AuthState::new();
/// let event_bus = EventBus::new();
///
/// let bridge = UiBridge::new(ui_state, auth_state, event_bus);
///
/// let window = MainWindow::new().unwrap();
/// bridge.setup_callbacks(&window);
///
/// let weak = window.as_weak();
/// tokio::spawn(async move {
///     bridge.start_state_sync(weak).await;
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

    /// Wire Slint callbacks to Rust handlers.
    ///
    /// This sets up the callback connections between UI actions (button clicks,
    /// input changes, etc.) and Rust business logic.
    ///
    /// Should be called once after creating the `MainWindow`.
    ///
    /// # Current Implementation
    ///
    /// This is a placeholder. As UI components are added in later phases,
    /// this method will wire up callbacks like:
    /// - Login button → AuthService::login
    /// - Send message → MessageService::send
    /// - Toggle sidebar → UIState::toggle_sidebar
    pub fn setup_callbacks(&self, _window: &MainWindow) {
        // Placeholder: callbacks will be wired as UI components are implemented
        // Example pattern for later:
        //
        // let ui_state = self.ui_state.clone();
        // window.on_toggle_sidebar(move || {
        //     ui_state.toggle_sidebar();
        // });
        //
        // let event_bus = self.event_bus.clone();
        // window.on_send_message(move |text| {
        //     event_bus.emit_ui(UiEvent::SendMessage { text: text.to_string() });
        // });
    }

    /// Start the async state synchronization loop.
    ///
    /// This spawns a background task that listens for state changes from
    /// Rust stores and pushes updates to Slint properties.
    ///
    /// Uses `Weak<MainWindow>` to allow the window to close normally.
    /// When the window is dropped, the weak reference will fail to upgrade
    /// and the sync loop will exit.
    ///
    /// # Current Implementation
    ///
    /// This is a placeholder. As Slint properties are defined in later phases,
    /// this method will push updates like:
    /// - UIStateData changes → theme, compact_mode, sidebar_collapsed properties
    /// - AuthStateData changes → is_logged_in, display_name properties
    pub async fn start_state_sync(&self, window: Weak<MainWindow>) {
        // Clone state stores for the async block
        let ui_state = self.ui_state.clone();
        let _auth_state = self.auth_state.clone();

        // Subscribe to UI state changes
        let mut ui_rx = ui_state.subscribe();

        // State sync loop
        loop {
            tokio::select! {
                // UI state changes
                result = ui_rx.recv() => {
                    match result {
                        Ok(_state) => {
                            // Try to upgrade weak reference
                            if let Some(_window) = window.upgrade() {
                                // Push state to Slint properties
                                // Example pattern for later:
                                //
                                // window.invoke_from_rust(move || {
                                //     window.set_sidebar_collapsed(state.sidebar_collapsed);
                                //     window.set_compact_mode(state.compact_mode);
                                // });
                            } else {
                                // Window closed, exit sync loop
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // Channel closed, exit sync loop
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            // Missed some updates, continue (we'll get the next one)
                            tracing::warn!("UI state sync lagged by {} messages", n);
                        }
                    }
                }
            }
        }
    }

    /// Get a reference to the UI state.
    #[must_use]
    pub fn ui_state(&self) -> &UIState {
        &self.ui_state
    }

    /// Get a reference to the auth state.
    #[must_use]
    pub fn auth_state(&self) -> &AuthState {
        &self.auth_state
    }

    /// Get a reference to the event bus.
    #[must_use]
    pub fn event_bus(&self) -> &EventBus {
        &self.event_bus
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
}
