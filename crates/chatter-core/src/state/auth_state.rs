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
    /// Changes whenever a login or sign-out invalidates pending UI work.
    pub session_revision: u64,
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
    /// The login in flight is solving the server's proof of work (an account
    /// with recent failed attempts; server OPEN-ITEMS §133). Seconds of
    /// work on a phone, so the login screen says so instead of looking stuck.
    pub login_verifying: bool,
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
            state.session_revision = state.session_revision.wrapping_add(1);
            state.current_user_id = Some(user_id);
            state.device_id = Some(device_id);
            state.display_name = Some(display_name);
            state.is_logging_in = false;
            state.login_error = None;
            state.login_verifying = false;
        });
    }

    /// Mark the in-flight login as solving (or done solving) a proof of work.
    pub fn set_login_verifying(&self, verifying: bool) {
        self.store.update(|state| state.login_verifying = verifying);
    }

    /// Update state after logout.
    ///
    /// Clears all user identity information.
    pub fn set_logged_out(&self) {
        self.store.update(|state| {
            state.session_revision = state.session_revision.wrapping_add(1);
            state.current_user_id = None;
            state.device_id = None;
            state.display_name = None;
            state.is_logging_in = false;
            state.login_error = None;
        });
    }

    /// Invalidate pending work before asynchronous sign-out teardown begins.
    pub fn invalidate_session(&self) {
        self.store.update(|state| {
            state.session_revision = state.session_revision.wrapping_add(1);
        });
    }

    pub fn update_display_name_if_current(&self, revision: u64, user: UserId, name: String) {
        self.store.update(|state| {
            if state.session_revision == revision && state.current_user_id == Some(user) {
                state.display_name = Some(name);
            }
        });
    }

    /// A user ID alone cannot fence A -> B -> A or a fresh login of A.
    pub fn session_is_current(&self, revision: u64) -> bool {
        let state = self.get();
        state.current_user_id.is_some() && state.session_revision == revision
    }

    pub fn set_logged_out_if_current(&self, revision: u64) -> bool {
        let mut cleared = false;
        self.store.update(|state| {
            if state.session_revision == revision {
                state.session_revision = state.session_revision.wrapping_add(1);
                state.current_user_id = None;
                state.device_id = None;
                state.display_name = None;
                state.is_logging_in = false;
                cleared = true;
            }
        });
        cleared
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
