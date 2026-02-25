//! Presence stream handling.
//!
//! Routes presence updates and maintains a local presence cache
//! for efficient lookups without network requests.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::broadcast;

use crate::proto::chat_v1::{PresenceUpdate, UserPresence};

/// Capacity for presence broadcast channel.
const CHANNEL_CAPACITY: usize = 128;

/// Maximum entries in presence cache.
const CACHE_CAPACITY: usize = 1000;

/// Presence router with local cache.
///
/// Receives presence updates from the stream, broadcasts them to
/// subscribers, and maintains a local cache for quick lookups.
///
/// # Example
///
/// ```ignore
/// let router = PresenceRouter::new();
///
/// // Subscribe to presence updates
/// let mut rx = router.on_presence_update();
///
/// // Route incoming update
/// router.route(update);
///
/// // Query cached presence
/// if let Some(status) = router.get_presence("user-123") {
///     println!("User is: {:?}", status);
/// }
/// ```
pub struct PresenceRouter {
    /// Broadcast channel for presence updates.
    updates: broadcast::Sender<PresenceUpdate>,

    /// Local presence cache (user_id -> CachedPresence).
    cache: Arc<RwLock<PresenceCache>>,
}

/// Cached presence entry with timestamp.
#[derive(Clone, Debug)]
pub struct CachedPresence {
    /// Current presence status.
    pub status: UserPresence,
    /// Timestamp of last update (Unix ms).
    pub updated_at: u64,
}

/// LRU presence cache with O(1) eviction.
struct PresenceCache {
    entries: HashMap<String, CachedPresence>,
    /// LRU order for eviction (most recent at back).
    lru: VecDeque<String>,
    capacity: usize,
}

impl PresenceCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(capacity),
            lru: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn get(&self, user_id: &str) -> Option<CachedPresence> {
        self.entries.get(user_id).cloned()
    }

    fn insert(&mut self, user_id: String, presence: CachedPresence) {
        // Update LRU order - remove existing entry if present
        self.lru.retain(|id| id != &user_id);
        self.lru.push_back(user_id.clone());

        // Evict oldest if at capacity (O(1) with LRU tracking)
        while self.lru.len() > self.capacity {
            if let Some(oldest) = self.lru.pop_front() {
                self.entries.remove(&oldest);
            }
        }

        self.entries.insert(user_id, presence);
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn get_many(&self, user_ids: &[&str]) -> HashMap<String, CachedPresence> {
        user_ids
            .iter()
            .filter_map(|id| {
                self.entries
                    .get(*id)
                    .map(|p| ((*id).to_string(), p.clone()))
            })
            .collect()
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
    }
}

impl PresenceRouter {
    /// Create a new PresenceRouter.
    pub fn new() -> Self {
        Self {
            updates: broadcast::channel(CHANNEL_CAPACITY).0,
            cache: Arc::new(RwLock::new(PresenceCache::new(CACHE_CAPACITY))),
        }
    }

    /// Subscribe to presence update events.
    pub fn on_presence_update(&self) -> broadcast::Receiver<PresenceUpdate> {
        self.updates.subscribe()
    }

    /// Get the number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.updates.receiver_count()
    }

    /// Get the number of cached presence entries.
    pub fn cache_size(&self) -> usize {
        self.cache.read().len()
    }
}

impl Default for PresenceRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl PresenceRouter {
    /// Route a presence update to subscribers and cache.
    ///
    /// Returns true if broadcast succeeded (has subscribers).
    pub fn route(&self, update: PresenceUpdate) -> bool {
        // Update cache
        let cached = CachedPresence {
            status: UserPresence::try_from(update.presence).unwrap_or(UserPresence::PresenceUnspecified),
            updated_at: update.timestamp,
        };
        self.cache.write().insert(update.user_id.clone(), cached);

        // Broadcast to subscribers
        self.updates.send(update).is_ok()
    }

    /// Get cached presence for a user.
    ///
    /// Returns None if user not in cache.
    pub fn get_presence(&self, user_id: &str) -> Option<CachedPresence> {
        self.cache.read().get(user_id)
    }

    /// Get cached presence for multiple users.
    ///
    /// Returns a map of user_id -> presence for users found in cache.
    pub fn get_many_presence(&self, user_ids: &[&str]) -> HashMap<String, CachedPresence> {
        self.cache.read().get_many(user_ids)
    }

    /// Check if a user is currently online.
    ///
    /// Returns false if user not in cache or offline.
    pub fn is_online(&self, user_id: &str) -> bool {
        self.get_presence(user_id)
            .map(|p| p.status == UserPresence::Online)
            .unwrap_or(false)
    }

    /// Clear the presence cache.
    ///
    /// Useful when disconnecting or logging out.
    pub fn clear_cache(&self) {
        self.cache.write().clear();
    }

    /// Seed the cache with initial presence data.
    ///
    /// Call after connection with bulk presence fetch.
    pub fn seed_cache(&self, entries: Vec<(String, UserPresence, u64)>) {
        let mut cache = self.cache.write();
        for (user_id, status, timestamp) in entries {
            cache.insert(
                user_id,
                CachedPresence {
                    status,
                    updated_at: timestamp,
                },
            );
        }
    }
}

/// Presence stream handler.
///
/// Type alias for backwards compatibility with the architecture document.
/// Use `PresenceRouter` for the full API.
pub type PresenceStream = PresenceRouter;

#[cfg(test)]
mod tests {
    use super::*;

    fn make_presence_update(user_id: &str, presence: UserPresence) -> PresenceUpdate {
        PresenceUpdate {
            user_id: user_id.to_string(),
            presence: presence as i32,
            timestamp: 1234567890000,
        }
    }

    #[test]
    fn test_router_new() {
        let router = PresenceRouter::new();
        assert_eq!(router.subscriber_count(), 0);
        assert_eq!(router.cache_size(), 0);
    }

    #[test]
    fn test_subscribe_increases_count() {
        let router = PresenceRouter::new();
        let _rx = router.on_presence_update();
        assert_eq!(router.subscriber_count(), 1);
    }

    #[test]
    fn test_route_updates_cache() {
        let router = PresenceRouter::new();
        let _rx = router.on_presence_update();

        let update = make_presence_update("user-123", UserPresence::Online);
        router.route(update);

        assert_eq!(router.cache_size(), 1);

        let cached = router.get_presence("user-123").unwrap();
        assert_eq!(cached.status, UserPresence::Online);
    }

    #[tokio::test]
    async fn test_route_broadcasts() {
        let router = PresenceRouter::new();
        let mut rx = router.on_presence_update();

        let update = make_presence_update("user-456", UserPresence::Away);
        assert!(router.route(update));

        let received = rx.try_recv().unwrap();
        assert_eq!(received.user_id, "user-456");
    }

    #[test]
    fn test_get_presence_missing() {
        let router = PresenceRouter::new();
        assert!(router.get_presence("unknown").is_none());
    }

    #[test]
    fn test_is_online() {
        let router = PresenceRouter::new();
        let _rx = router.on_presence_update();

        // Not in cache
        assert!(!router.is_online("user-123"));

        // Add online user
        router.route(make_presence_update("user-123", UserPresence::Online));
        assert!(router.is_online("user-123"));

        // Add offline user
        router.route(make_presence_update("user-456", UserPresence::Offline));
        assert!(!router.is_online("user-456"));

        // Add away user
        router.route(make_presence_update("user-789", UserPresence::Away));
        assert!(!router.is_online("user-789"));
    }

    #[test]
    fn test_get_many_presence() {
        let router = PresenceRouter::new();
        let _rx = router.on_presence_update();

        router.route(make_presence_update("user-1", UserPresence::Online));
        router.route(make_presence_update("user-2", UserPresence::Away));
        router.route(make_presence_update("user-3", UserPresence::Offline));

        let result = router.get_many_presence(&["user-1", "user-2", "unknown"]);

        assert_eq!(result.len(), 2);
        assert!(result.contains_key("user-1"));
        assert!(result.contains_key("user-2"));
        assert!(!result.contains_key("unknown"));
    }

    #[test]
    fn test_clear_cache() {
        let router = PresenceRouter::new();
        let _rx = router.on_presence_update();

        router.route(make_presence_update("user-1", UserPresence::Online));
        router.route(make_presence_update("user-2", UserPresence::Online));

        assert_eq!(router.cache_size(), 2);

        router.clear_cache();
        assert_eq!(router.cache_size(), 0);
    }

    #[test]
    fn test_seed_cache() {
        let router = PresenceRouter::new();

        router.seed_cache(vec![
            ("user-1".to_string(), UserPresence::Online, 1000),
            ("user-2".to_string(), UserPresence::Away, 2000),
        ]);

        assert_eq!(router.cache_size(), 2);

        let p1 = router.get_presence("user-1").unwrap();
        assert_eq!(p1.status, UserPresence::Online);
        assert_eq!(p1.updated_at, 1000);

        let p2 = router.get_presence("user-2").unwrap();
        assert_eq!(p2.status, UserPresence::Away);
    }

    #[test]
    fn test_cache_eviction() {
        // Create a small capacity cache for testing
        let cache = PresenceCache::new(2);
        let cache = Arc::new(RwLock::new(cache));

        let router = PresenceRouter {
            updates: broadcast::channel(16).0,
            cache,
        };
        let _rx = router.on_presence_update();

        // Fill cache
        router.route(PresenceUpdate {
            user_id: "user-1".to_string(),
            presence: UserPresence::Online as i32,
            timestamp: 1000,
        });
        router.route(PresenceUpdate {
            user_id: "user-2".to_string(),
            presence: UserPresence::Online as i32,
            timestamp: 2000,
        });

        assert_eq!(router.cache_size(), 2);

        // Add one more - should evict oldest (user-1)
        router.route(PresenceUpdate {
            user_id: "user-3".to_string(),
            presence: UserPresence::Online as i32,
            timestamp: 3000,
        });

        assert_eq!(router.cache_size(), 2);
        assert!(router.get_presence("user-1").is_none()); // Evicted
        assert!(router.get_presence("user-2").is_some());
        assert!(router.get_presence("user-3").is_some());
    }

    #[test]
    fn test_cached_presence_clone() {
        let cached = CachedPresence {
            status: UserPresence::DoNotDisturb,
            updated_at: 12345,
        };
        let cloned = cached.clone();
        assert_eq!(cloned.status, UserPresence::DoNotDisturb);
        assert_eq!(cloned.updated_at, 12345);
    }
}
