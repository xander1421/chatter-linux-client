//! Authentication state.
//!
//! Reactive authentication and session state for UI binding.

use tokio::sync::broadcast;

use crate::domain::ids::UserId;
use crate::state::store::Store;

/// Authentication state data.
///
/// Tracks the current user's authentication status, identity,
/// and any pending login operations.
#[derive(Clone, Debug, Default)]
pub struct AuthStateData {
    /// Currently logged in user ID, if any.
    pub current_user_id: Option<UserId>,
    /// Current device ID (1 = primary, 2-127 = linked devices).
    pub device_id: Option<u32>,
    /// Display name of the current user.
    pub display_name: Option<String>,
    /// Whether a login operation is in progress.
    pub is_logging_in: bool,
    /// Login error message, if the last login attempt failed.
    pub login_error: Option<String>,
}

/// Reactive authentication state manager.
///
/// Provides a reactive wrapper around `AuthStateData` that allows
/// UI components to subscribe to authentication state changes.
///
/// # Relationship with AuthService
///
/// - `AuthService`: Handles actual authentication logic (login, logout, device auth)
/// - `AuthState`: Reactive UI state that reflects authentication status
///
/// The service updates this state after successful operations. UI components
/// subscribe to this state for reactive updates without coupling to service logic.
///
/// # Example
///
/// ```ignore
/// let auth_state = AuthState::new();
///
/// // UI subscribes to auth changes
/// let mut rx = auth_state.subscribe();
///
/// // Service updates state after login
/// auth_state.set_logged_in(user_id, device_id, "Alice".to_string());
///
/// // UI receives update
/// let state = rx.recv().await.unwrap();
/// assert!(state.current_user_id.is_some());
/// ```
#[derive(Clone)]
pub struct AuthState {
    store: Store<AuthStateData>,
}

impl AuthState {
    /// Create a new auth state with default (logged out) values.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(AuthStateData::default()),
        }
    }

    /// Get the current authentication state.
    #[must_use]
    pub fn get(&self) -> AuthStateData {
        self.store.get()
    }

    /// Subscribe to authentication state changes.
    ///
    /// The receiver will receive Arc-wrapped state snapshots (zero-copy sharing).
    /// Call `get()` first if you need the current state immediately.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<super::StateSnapshot<AuthStateData>> {
        self.store.subscribe()
    }

    /// Update state after successful login.
    ///
    /// Clears any login errors and sets the user identity.
    pub fn set_logged_in(&self, user_id: UserId, device_id: u32, display_name: String) {
        self.store.update(|state| {
            state.current_user_id = Some(user_id);
            state.device_id = Some(device_id);
            state.display_name = Some(display_name);
            state.is_logging_in = false;
            state.login_error = None;
        });
    }

    /// Update state after logout.
    ///
    /// Clears all user identity information.
    pub fn set_logged_out(&self) {
        self.store.update(|state| {
            state.current_user_id = None;
            state.device_id = None;
            state.display_name = None;
            state.is_logging_in = false;
            state.login_error = None;
        });
    }

    /// Set whether a login operation is in progress.
    ///
    /// UI can use this to show loading indicators.
    pub fn set_logging_in(&self, is_logging_in: bool) {
        self.store.update(|state| {
            state.is_logging_in = is_logging_in;
            if is_logging_in {
                // Clear previous error when starting new login attempt
                state.login_error = None;
            }
        });
    }

    /// Set a login error message.
    ///
    /// Also clears `is_logging_in` since the operation has completed (with error).
    pub fn set_login_error(&self, error: Option<String>) {
        self.store.update(|state| {
            state.login_error = error;
            state.is_logging_in = false;
        });
    }

    /// Quick check if user is currently logged in.
    #[must_use]
    pub fn is_logged_in(&self) -> bool {
        self.store.get().current_user_id.is_some()
    }
}

impl Default for AuthState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_auth_state_default_is_logged_out() {
        let state = AuthState::new();
        let data = state.get();

        assert!(data.current_user_id.is_none());
        assert!(data.device_id.is_none());
        assert!(data.display_name.is_none());
        assert!(!data.is_logging_in);
        assert!(data.login_error.is_none());
        assert!(!state.is_logged_in());
    }

    #[test]
    fn test_auth_state_set_logged_in() {
        let state = AuthState::new();
        let user_id = UserId::from_uuid(Uuid::new_v4());

        state.set_logged_in(user_id, 1, "Alice".to_string());

        let data = state.get();
        assert_eq!(data.current_user_id, Some(user_id));
        assert_eq!(data.device_id, Some(1));
        assert_eq!(data.display_name, Some("Alice".to_string()));
        assert!(!data.is_logging_in);
        assert!(data.login_error.is_none());
        assert!(state.is_logged_in());
    }

    #[test]
    fn test_auth_state_set_logged_out() {
        let state = AuthState::new();
        let user_id = UserId::from_uuid(Uuid::new_v4());

        // First log in
        state.set_logged_in(user_id, 1, "Alice".to_string());
        assert!(state.is_logged_in());

        // Then log out
        state.set_logged_out();
        assert!(!state.is_logged_in());

        let data = state.get();
        assert!(data.current_user_id.is_none());
        assert!(data.device_id.is_none());
        assert!(data.display_name.is_none());
    }

    #[test]
    fn test_auth_state_logging_in_progress() {
        let state = AuthState::new();

        state.set_logging_in(true);
        assert!(state.get().is_logging_in);

        state.set_logging_in(false);
        assert!(!state.get().is_logging_in);
    }

    #[test]
    fn test_auth_state_login_error_clears_logging_in() {
        let state = AuthState::new();

        state.set_logging_in(true);
        state.set_login_error(Some("Invalid password".to_string()));

        let data = state.get();
        assert!(!data.is_logging_in);
        assert_eq!(data.login_error, Some("Invalid password".to_string()));
    }

    #[test]
    fn test_auth_state_new_login_clears_error() {
        let state = AuthState::new();

        // Set an error
        state.set_login_error(Some("Previous error".to_string()));
        assert!(state.get().login_error.is_some());

        // Start new login attempt
        state.set_logging_in(true);
        assert!(state.get().login_error.is_none());
    }

    #[tokio::test]
    async fn test_auth_state_subscribe() {
        let state = AuthState::new();
        let mut rx = state.subscribe();
        let user_id = UserId::from_uuid(Uuid::new_v4());

        state.set_logged_in(user_id, 1, "Bob".to_string());

        let received = rx.recv().await.unwrap();
        assert_eq!(received.current_user_id, Some(user_id));
        assert_eq!(received.display_name, Some("Bob".to_string()));
    }

    #[test]
    fn test_auth_state_clone_shares_state() {
        let state1 = AuthState::new();
        let state2 = state1.clone();
        let user_id = UserId::from_uuid(Uuid::new_v4());

        state1.set_logged_in(user_id, 1, "Charlie".to_string());

        // Both handles see the same state
        assert!(state2.is_logged_in());
        assert_eq!(state2.get().display_name, Some("Charlie".to_string()));
    }
}
