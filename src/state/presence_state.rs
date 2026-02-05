//! Presence state.
//!
//! Reactive presence tracking for UI binding.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use crate::domain::ids::UserId;
use crate::domain::user::PresenceStatus;
use crate::state::store::Store;

/// Presence state data.
///
/// Tracks presence status for users with staleness timestamps.
/// This provides the UI with reactive access to presence information
/// without direct coupling to the presence service/router.
#[derive(Clone, Debug)]
pub struct PresenceStateData {
    /// Cached presence status by user ID.
    pub presence: HashMap<UserId, PresenceStatus>,
    /// Current user's own presence status.
    pub own_presence: PresenceStatus,
    /// Last update time per user for staleness tracking.
    pub last_updated: HashMap<UserId, DateTime<Utc>>,
}

impl Default for PresenceStateData {
    fn default() -> Self {
        Self {
            presence: HashMap::new(),
            own_presence: PresenceStatus::Offline,
            last_updated: HashMap::new(),
        }
    }
}

/// Reactive presence state manager.
///
/// Provides a reactive wrapper around `PresenceStateData` that allows
/// UI components to subscribe to presence changes.
///
/// # Relationship with PresenceService/PresenceRouter
///
/// - `PresenceService`: Handles presence API calls, subscribes to server updates
/// - `PresenceRouter`: LRU cache for efficient presence lookups
/// - `PresenceState`: Reactive UI layer for subscribing to presence changes
///
/// The service/router populates this state after receiving updates.
/// UI components subscribe to this state for reactive presence indicators.
///
/// # Example
///
/// ```ignore
/// let presence_state = PresenceState::new();
///
/// // UI subscribes to presence changes
/// let mut rx = presence_state.subscribe();
///
/// // Service updates presence after receiving server event
/// presence_state.set_presence(user_id, PresenceStatus::Online);
///
/// // UI receives update
/// let state = rx.recv().await.unwrap();
/// ```
#[derive(Clone)]
pub struct PresenceState {
    store: Store<PresenceStateData>,
}

impl PresenceState {
    /// Create a new presence state with default values.
    ///
    /// Own presence defaults to `Offline`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(PresenceStateData::default()),
        }
    }

    /// Get the current presence state data.
    #[must_use]
    pub fn get(&self) -> PresenceStateData {
        self.store.get()
    }

    /// Subscribe to presence state changes.
    ///
    /// The receiver will receive the state each time it changes.
    /// Call `get()` first if you need the current state immediately.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<PresenceStateData> {
        self.store.subscribe()
    }

    /// Update a single user's presence status.
    pub fn set_presence(&self, user_id: UserId, status: PresenceStatus) {
        self.store.update(|state| {
            state.presence.insert(user_id, status);
            state.last_updated.insert(user_id, Utc::now());
        });
    }

    /// Batch update multiple users' presence status.
    ///
    /// More efficient than calling `set_presence` multiple times
    /// as it only triggers one notification.
    pub fn set_many_presence(&self, updates: Vec<(UserId, PresenceStatus)>) {
        if updates.is_empty() {
            return;
        }
        self.store.update(|state| {
            let now = Utc::now();
            for (user_id, status) in updates {
                state.presence.insert(user_id, status);
                state.last_updated.insert(user_id, now);
            }
        });
    }

    /// Get a user's presence status from the cache.
    #[must_use]
    pub fn get_presence(&self, user_id: &UserId) -> Option<PresenceStatus> {
        self.store.get().presence.get(user_id).copied()
    }

    /// Set the current user's own presence status.
    pub fn set_own_presence(&self, status: PresenceStatus) {
        self.store.update(|state| {
            state.own_presence = status;
        });
    }

    /// Get the current user's own presence status.
    #[must_use]
    pub fn get_own_presence(&self) -> PresenceStatus {
        self.store.get().own_presence
    }

    /// Check if a user is online.
    ///
    /// Returns `false` if the user is not in the cache.
    #[must_use]
    pub fn is_online(&self, user_id: &UserId) -> bool {
        self.store
            .get()
            .presence
            .get(user_id)
            .map_or(false, |status| *status == PresenceStatus::Online)
    }

    /// Check if a user is available (online or away).
    ///
    /// Returns `false` if the user is not in the cache.
    #[must_use]
    pub fn is_available(&self, user_id: &UserId) -> bool {
        self.store
            .get()
            .presence
            .get(user_id)
            .map_or(false, |status| status.is_available())
    }

    /// Clear all presence data (typically called on logout/disconnect).
    pub fn clear(&self) {
        self.store.set(PresenceStateData::default());
    }
}

impl Default for PresenceState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_presence_state_default() {
        let state = PresenceState::new();
        let data = state.get();

        assert!(data.presence.is_empty());
        assert_eq!(data.own_presence, PresenceStatus::Offline);
        assert!(data.last_updated.is_empty());
    }

    #[test]
    fn test_set_and_get_presence() {
        let state = PresenceState::new();
        let user_id = UserId::from_uuid(Uuid::new_v4());

        assert!(state.get_presence(&user_id).is_none());

        state.set_presence(user_id, PresenceStatus::Online);

        let status = state.get_presence(&user_id);
        assert_eq!(status, Some(PresenceStatus::Online));

        // Check last_updated was set
        let data = state.get();
        assert!(data.last_updated.contains_key(&user_id));
    }

    #[test]
    fn test_set_many_presence() {
        let state = PresenceState::new();
        let user1 = UserId::from_uuid(Uuid::new_v4());
        let user2 = UserId::from_uuid(Uuid::new_v4());
        let user3 = UserId::from_uuid(Uuid::new_v4());

        state.set_many_presence(vec![
            (user1, PresenceStatus::Online),
            (user2, PresenceStatus::Away),
            (user3, PresenceStatus::DoNotDisturb),
        ]);

        assert_eq!(state.get_presence(&user1), Some(PresenceStatus::Online));
        assert_eq!(state.get_presence(&user2), Some(PresenceStatus::Away));
        assert_eq!(state.get_presence(&user3), Some(PresenceStatus::DoNotDisturb));
    }

    #[test]
    fn test_set_many_presence_empty() {
        let state = PresenceState::new();

        // Should be a no-op, no crash
        state.set_many_presence(vec![]);

        assert!(state.get().presence.is_empty());
    }

    #[test]
    fn test_own_presence() {
        let state = PresenceState::new();

        assert_eq!(state.get_own_presence(), PresenceStatus::Offline);

        state.set_own_presence(PresenceStatus::Online);
        assert_eq!(state.get_own_presence(), PresenceStatus::Online);

        state.set_own_presence(PresenceStatus::DoNotDisturb);
        assert_eq!(state.get_own_presence(), PresenceStatus::DoNotDisturb);
    }

    #[test]
    fn test_is_online() {
        let state = PresenceState::new();
        let user_online = UserId::from_uuid(Uuid::new_v4());
        let user_away = UserId::from_uuid(Uuid::new_v4());
        let user_unknown = UserId::from_uuid(Uuid::new_v4());

        state.set_presence(user_online, PresenceStatus::Online);
        state.set_presence(user_away, PresenceStatus::Away);

        assert!(state.is_online(&user_online));
        assert!(!state.is_online(&user_away));
        assert!(!state.is_online(&user_unknown));
    }

    #[test]
    fn test_is_available() {
        let state = PresenceState::new();
        let user_online = UserId::from_uuid(Uuid::new_v4());
        let user_away = UserId::from_uuid(Uuid::new_v4());
        let user_dnd = UserId::from_uuid(Uuid::new_v4());
        let user_offline = UserId::from_uuid(Uuid::new_v4());
        let user_unknown = UserId::from_uuid(Uuid::new_v4());

        state.set_presence(user_online, PresenceStatus::Online);
        state.set_presence(user_away, PresenceStatus::Away);
        state.set_presence(user_dnd, PresenceStatus::DoNotDisturb);
        state.set_presence(user_offline, PresenceStatus::Offline);

        // Online and Away are available
        assert!(state.is_available(&user_online));
        assert!(state.is_available(&user_away));

        // DND and Offline are not available
        assert!(!state.is_available(&user_dnd));
        assert!(!state.is_available(&user_offline));

        // Unknown users are not available
        assert!(!state.is_available(&user_unknown));
    }

    #[test]
    fn test_clear() {
        let state = PresenceState::new();
        let user_id = UserId::from_uuid(Uuid::new_v4());

        // Populate state
        state.set_presence(user_id, PresenceStatus::Online);
        state.set_own_presence(PresenceStatus::Online);

        assert!(!state.get().presence.is_empty());

        // Clear
        state.clear();
        let data = state.get();

        assert!(data.presence.is_empty());
        assert_eq!(data.own_presence, PresenceStatus::Offline);
        assert!(data.last_updated.is_empty());
    }

    #[tokio::test]
    async fn test_subscribe() {
        let state = PresenceState::new();
        let mut rx = state.subscribe();
        let user_id = UserId::from_uuid(Uuid::new_v4());

        state.set_presence(user_id, PresenceStatus::Online);

        let received = rx.recv().await.unwrap();
        assert_eq!(received.presence.get(&user_id), Some(&PresenceStatus::Online));
    }

    #[test]
    fn test_clone_shares_state() {
        let state1 = PresenceState::new();
        let state2 = state1.clone();
        let user_id = UserId::from_uuid(Uuid::new_v4());

        state1.set_presence(user_id, PresenceStatus::Online);

        // Both handles see the same state
        assert!(state2.is_online(&user_id));
    }
}
