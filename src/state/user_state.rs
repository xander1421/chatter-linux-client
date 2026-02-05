//! User state.
//!
//! Reactive user profile and relationship cache for UI binding.

use std::collections::{HashMap, HashSet};

use tokio::sync::broadcast;

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

    /// Subscribe to user state changes.
    ///
    /// The receiver will receive the state each time it changes.
    /// Call `get()` first if you need the current state immediately.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<UserStateData> {
        self.store.subscribe()
    }

    /// Add or update a user in the cache.
    pub fn set_user(&self, user: User) {
        self.store.update(|state| {
            state.users.insert(user.id, user);
        });
    }

    /// Add or update a profile in the cache.
    pub fn set_profile(&self, profile: Profile) {
        self.store.update(|state| {
            state.profiles.insert(profile.user_id, profile);
        });
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

    /// Replace the entire friend list.
    pub fn set_friends(&self, friends: Vec<UserId>) {
        self.store.update(|state| {
            state.friends = friends;
        });
    }

    /// Add a user to the friend list if not already present.
    pub fn add_friend(&self, user_id: UserId) {
        self.store.update(|state| {
            if !state.friends.contains(&user_id) {
                state.friends.push(user_id);
            }
        });
    }

    /// Remove a user from the friend list.
    pub fn remove_friend(&self, user_id: &UserId) {
        self.store.update(|state| {
            state.friends.retain(|id| id != user_id);
        });
    }

    /// Replace the entire blocked users set.
    pub fn set_blocked(&self, blocked: HashSet<UserId>) {
        self.store.update(|state| {
            state.blocked_users = blocked;
        });
    }

    /// Add a user to the blocked set.
    pub fn block_user(&self, user_id: UserId) {
        self.store.update(|state| {
            state.blocked_users.insert(user_id);
        });
    }

    /// Remove a user from the blocked set.
    pub fn unblock_user(&self, user_id: &UserId) {
        self.store.update(|state| {
            state.blocked_users.remove(user_id);
        });
    }

    /// Check if a user is blocked.
    #[must_use]
    pub fn is_blocked(&self, user_id: &UserId) -> bool {
        self.store.get().blocked_users.contains(user_id)
    }

    /// Set the current logged-in user's profile.
    pub fn set_current_user_profile(&self, profile: Profile) {
        self.store.update(|state| {
            state.current_user_profile = Some(profile);
        });
    }

    /// Clear all cached data (typically called on logout).
    pub fn clear(&self) {
        self.store.set(UserStateData::default());
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
    use uuid::Uuid;

    fn test_user(id: UserId) -> User {
        User::new(id, "testuser".to_string(), vec![0u8; 32])
    }

    fn test_profile(id: UserId) -> Profile {
        Profile::new(id, "Test User".to_string())
    }

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

    #[test]
    fn test_set_and_get_user() {
        let state = UserState::new();
        let user_id = UserId::from_uuid(Uuid::new_v4());
        let user = test_user(user_id);

        state.set_user(user.clone());

        let retrieved = state.get_user(&user_id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().id, user_id);
    }

    #[test]
    fn test_set_and_get_profile() {
        let state = UserState::new();
        let user_id = UserId::from_uuid(Uuid::new_v4());
        let profile = test_profile(user_id);

        state.set_profile(profile.clone());

        let retrieved = state.get_profile(&user_id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().user_id, user_id);
    }

    #[test]
    fn test_friends_list() {
        let state = UserState::new();
        let friend1 = UserId::from_uuid(Uuid::new_v4());
        let friend2 = UserId::from_uuid(Uuid::new_v4());

        // Set friends
        state.set_friends(vec![friend1, friend2]);
        assert_eq!(state.get().friends.len(), 2);

        // Add friend (already exists, should not duplicate)
        state.add_friend(friend1);
        assert_eq!(state.get().friends.len(), 2);

        // Add new friend
        let friend3 = UserId::from_uuid(Uuid::new_v4());
        state.add_friend(friend3);
        assert_eq!(state.get().friends.len(), 3);

        // Remove friend
        state.remove_friend(&friend1);
        assert_eq!(state.get().friends.len(), 2);
        assert!(!state.get().friends.contains(&friend1));
    }

    #[test]
    fn test_blocked_users() {
        let state = UserState::new();
        let user1 = UserId::from_uuid(Uuid::new_v4());
        let user2 = UserId::from_uuid(Uuid::new_v4());

        // Block user
        state.block_user(user1);
        assert!(state.is_blocked(&user1));
        assert!(!state.is_blocked(&user2));

        // Set blocked set
        let mut blocked = HashSet::new();
        blocked.insert(user2);
        state.set_blocked(blocked);
        assert!(!state.is_blocked(&user1));
        assert!(state.is_blocked(&user2));

        // Unblock
        state.unblock_user(&user2);
        assert!(!state.is_blocked(&user2));
    }

    #[test]
    fn test_current_user_profile() {
        let state = UserState::new();
        let user_id = UserId::from_uuid(Uuid::new_v4());
        let profile = test_profile(user_id);

        assert!(state.get().current_user_profile.is_none());

        state.set_current_user_profile(profile.clone());
        assert!(state.get().current_user_profile.is_some());
        assert_eq!(state.get().current_user_profile.unwrap().user_id, user_id);
    }

    #[test]
    fn test_clear() {
        let state = UserState::new();
        let user_id = UserId::from_uuid(Uuid::new_v4());

        // Populate state
        state.set_user(test_user(user_id));
        state.set_profile(test_profile(user_id));
        state.set_friends(vec![user_id]);
        state.block_user(user_id);
        state.set_current_user_profile(test_profile(user_id));

        // Clear
        state.clear();
        let data = state.get();

        assert!(data.users.is_empty());
        assert!(data.profiles.is_empty());
        assert!(data.friends.is_empty());
        assert!(data.blocked_users.is_empty());
        assert!(data.current_user_profile.is_none());
    }

    #[tokio::test]
    async fn test_subscribe() {
        let state = UserState::new();
        let mut rx = state.subscribe();
        let user_id = UserId::from_uuid(Uuid::new_v4());

        state.set_user(test_user(user_id));

        let received = rx.recv().await.unwrap();
        assert!(received.users.contains_key(&user_id));
    }

    #[test]
    fn test_clone_shares_state() {
        let state1 = UserState::new();
        let state2 = state1.clone();
        let user_id = UserId::from_uuid(Uuid::new_v4());

        state1.set_user(test_user(user_id));

        // Both handles see the same state
        assert!(state2.get_user(&user_id).is_some());
    }
}
