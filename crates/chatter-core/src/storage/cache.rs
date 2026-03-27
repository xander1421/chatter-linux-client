//! In-memory LRU caches.
//!
//! Bounded, concurrent LRU caches for frequently accessed data.
//! Reduces SQLite read pressure for hot paths (contacts, recent messages,
//! channel metadata) by keeping a configurable in-memory layer.
//!
//! Built on `moka` which provides:
//! - Thread-safe concurrent access without external locking
//! - Size-based eviction (weighted by entry size)
//! - Time-based expiration (TTL per entry)
//! - O(1) amortized get/put

use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

/// Default maximum cache capacity (number of entries).
const DEFAULT_MAX_ENTRIES: u64 = 10_000;

/// Default time-to-live for cache entries.
const DEFAULT_TTL_SECS: u64 = 300; // 5 minutes

/// Default time-to-idle (evict if not accessed within this window).
const DEFAULT_TTI_SECS: u64 = 120; // 2 minutes

/// Configuration for an LRU cache instance.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum number of entries.
    pub max_entries: u64,
    /// Time-to-live: entries expire after this duration since insertion.
    pub ttl: Duration,
    /// Time-to-idle: entries expire after this duration since last access.
    pub tti: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            ttl: Duration::from_secs(DEFAULT_TTL_SECS),
            tti: Duration::from_secs(DEFAULT_TTI_SECS),
        }
    }
}

/// Thread-safe, bounded LRU cache with time-based eviction.
///
/// Wraps `moka::sync::Cache` for concurrent read-heavy workloads.
/// Suitable for caching DB query results that are read far more often
/// than written (contacts, channel metadata, recent messages).
#[derive(Clone)]
pub struct LruCache<K, V>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    inner: moka::sync::Cache<K, V>,
}

impl<K, V> LruCache<K, V>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Create a new cache with the given configuration.
    pub fn new(config: CacheConfig) -> Self {
        let inner = moka::sync::Cache::builder()
            .max_capacity(config.max_entries)
            .time_to_live(config.ttl)
            .time_to_idle(config.tti)
            .build();
        Self { inner }
    }

    /// Create a cache with default settings (~10k entries, 5min TTL, 2min TTI).
    pub fn with_defaults() -> Self {
        Self::new(CacheConfig::default())
    }

    /// Get a cached value, returning None on miss.
    pub fn get(&self, key: &K) -> Option<V> {
        self.inner.get(key)
    }

    /// Insert or update a cache entry.
    pub fn insert(&self, key: K, value: V) {
        self.inner.insert(key, value);
    }

    /// Remove a specific entry. Returns true if the key was present.
    pub fn invalidate(&self, key: &K) {
        self.inner.invalidate(key);
    }

    /// Remove all entries.
    pub fn invalidate_all(&self) {
        self.inner.invalidate_all();
    }

    /// Current number of entries (approximate, may include pending evictions).
    pub fn entry_count(&self) -> u64 {
        self.inner.entry_count()
    }

    /// Get a value or compute it from the provided closure on cache miss.
    pub fn get_or_insert_with(&self, key: K, f: impl FnOnce() -> V) -> V {
        self.inner.get_with(key, f)
    }
}

/// Pre-configured cache layer for hot application data.
///
/// Holds typed caches for the most frequently accessed entities.
/// Attach to services that need fast reads without hitting SQLite.
#[derive(Clone)]
pub struct CacheLayer {
    /// Cache for contact/user lookups by user ID string.
    pub contacts: LruCache<String, Vec<u8>>,
    /// Cache for channel metadata by channel ID string.
    pub channels: LruCache<String, Vec<u8>>,
    /// Cache for recent message batches by channel ID string.
    /// Value is serialized message list (bincode or similar).
    pub recent_messages: LruCache<String, Vec<u8>>,
}

impl CacheLayer {
    /// Create a new cache layer with default sizing.
    ///
    /// - Contacts: 5k entries, 10min TTL (changes infrequently)
    /// - Channels: 2k entries, 5min TTL
    /// - Recent messages: 500 entries, 2min TTL (changes frequently)
    pub fn new() -> Self {
        Self {
            contacts: LruCache::new(CacheConfig {
                max_entries: 5_000,
                ttl: Duration::from_secs(600),
                tti: Duration::from_secs(300),
            }),
            channels: LruCache::new(CacheConfig {
                max_entries: 2_000,
                ttl: Duration::from_secs(300),
                tti: Duration::from_secs(120),
            }),
            recent_messages: LruCache::new(CacheConfig {
                max_entries: 500,
                ttl: Duration::from_secs(120),
                tti: Duration::from_secs(60),
            }),
        }
    }

    /// Invalidate all caches. Call after bulk operations (e.g., sync).
    pub fn invalidate_all(&self) {
        self.contacts.invalidate_all();
        self.channels.invalidate_all();
        self.recent_messages.invalidate_all();
    }
}

impl Default for CacheLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared handle to the application cache layer.
pub type SharedCacheLayer = Arc<CacheLayer>;
