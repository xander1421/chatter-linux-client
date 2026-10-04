//! User state.
//!
//! Reactive user profile and relationship cache for UI binding.

use std::collections::{HashMap, HashSet};


use crate::domain::ids::UserId;
use crate::domain::user::{Profile, User};
use crate::state::store::Store;

/// User state data.
///
/// Caches user entities, profiles, and relationship data (friends/blocked).
/// This provides the UI with reactive access to user information without
/// needing to query the server for each render.
#[derive(Clone, Debug, Default)]
pub struct UserStateData {
    /// Cached user entities by ID.
    pub users: HashMap<UserId, User>,
    /// Cached user profiles by ID.
    pub profiles: HashMap<UserId, Profile>,
    /// List of friend user IDs (profiles fetched separately).
    pub friends: Vec<UserId>,
    /// Set of blocked user IDs for quick lookup.
    pub blocked_users: HashSet<UserId>,
    /// The current logged-in user's own profile.
    pub current_user_profile: Option<Profile>,
}

/// Reactive user state manager.
///
/// Provides a reactive wrapper around `UserStateData` that allows
/// UI components to subscribe to user data changes.
///
/// # Relationship with UserService
///
/// - `UserService`: Fetches user data from server, handles API calls
/// - `UserState`: Reactive UI cache that stores user/profile data locally
///
/// The service populates this state after fetching data. UI components
/// subscribe to this state for reactive updates.
///
/// # Example
///
/// ```ignore
/// let user_state = UserState::new();
///
/// // UI subscribes to user changes
/// let mut rx = user_state.subscribe();
///
/// // Service updates state after fetching
/// user_state.set_user(user);
/// user_state.set_profile(profile);
///
/// // UI receives update
/// let state = rx.recv().await.unwrap();
/// ```
#[derive(Clone)]
pub struct UserState {
    store: Store<UserStateData>,
}

impl UserState {
    /// Create a new user state with empty caches.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(UserStateData::default()),
        }
    }

    /// Get the current user state data.
    #[must_use]
    pub fn get(&self) -> UserStateData {
        self.store.get()
    }

    /// Get a user from the cache by ID.
    #[must_use]
    pub fn get_user(&self, user_id: &UserId) -> Option<User> {
        self.store.get().users.get(user_id).cloned()
    }

    /// Get a profile from the cache by ID.
    #[must_use]
    pub fn get_profile(&self, user_id: &UserId) -> Option<Profile> {
        self.store.get().profiles.get(user_id).cloned()
    }

}

impl Default for UserState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_state_default_empty() {
        let state = UserState::new();
        let data = state.get();

        assert!(data.users.is_empty());
        assert!(data.profiles.is_empty());
        assert!(data.friends.is_empty());
        assert!(data.blocked_users.is_empty());
        assert!(data.current_user_profile.is_none());
    }

}
