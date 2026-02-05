//! GIF cache.
//!
//! In-memory caching for GIF search and trending results with TTL-based eviction.

use super::client::GifResult;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default maximum number of cached search queries.
const DEFAULT_MAX_SEARCH_ENTRIES: usize = 100;

/// Default TTL for search cache entries (1 minute).
const DEFAULT_SEARCH_TTL: Duration = Duration::from_secs(60);

/// Default TTL for trending cache (5 minutes).
const DEFAULT_TRENDING_TTL: Duration = Duration::from_secs(300);

/// Cached search results with timestamp.
struct CachedSearchResults {
    results: Vec<GifResult>,
    cached_at: Instant,
}

/// GIF cache for search and trending results.
///
/// Uses simple HashMap with TTL-based expiration and size-based eviction.
pub struct GifCache {
    /// Search results cache: query -> (timestamp, results).
    search_cache: Mutex<HashMap<String, CachedSearchResults>>,
    /// Trending results cache with timestamp.
    trending_cache: Mutex<Option<CachedSearchResults>>,
    /// Maximum search cache entries.
    max_search_entries: usize,
    /// TTL for search cache entries.
    search_ttl: Duration,
    /// TTL for trending cache.
    trending_ttl: Duration,
}

impl GifCache {
    /// Create a new GIF cache with default settings.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_SEARCH_ENTRIES, DEFAULT_TRENDING_TTL)
    }

    /// Create a new GIF cache with custom settings.
    ///
    /// # Arguments
    /// * `search_entries` - Maximum number of cached search queries
    /// * `trending_ttl` - TTL for trending cache
    pub fn with_capacity(search_entries: usize, trending_ttl: Duration) -> Self {
        Self {
            search_cache: Mutex::new(HashMap::new()),
            trending_cache: Mutex::new(None),
            max_search_entries: search_entries,
            search_ttl: DEFAULT_SEARCH_TTL,
            trending_ttl,
        }
    }

    /// Get cached search results for a query.
    ///
    /// Returns None if not cached or expired.
    pub fn get_search(&self, query: &str) -> Option<Vec<GifResult>> {
        let normalized = Self::normalize_query(query);
        let cache = self.search_cache.lock();

        if let Some(cached) = cache.get(&normalized) {
            if cached.cached_at.elapsed() < self.search_ttl {
                return Some(cached.results.clone());
            }
        }
        None
    }

    /// Store search results in the cache.
    ///
    /// Evicts oldest entries if over capacity.
    pub fn put_search(&self, query: &str, results: Vec<GifResult>) {
        let normalized = Self::normalize_query(query);
        let mut cache = self.search_cache.lock();

        // Evict expired entries first
        let now = Instant::now();
        cache.retain(|_, v| now.duration_since(v.cached_at) < self.search_ttl);

        // Evict oldest if over capacity
        if cache.len() >= self.max_search_entries {
            // Find and remove the oldest entry
            if let Some(oldest_key) = cache
                .iter()
                .min_by_key(|(_, v)| v.cached_at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest_key);
            }
        }

        cache.insert(
            normalized,
            CachedSearchResults {
                results,
                cached_at: Instant::now(),
            },
        );
    }

    /// Get cached trending results.
    ///
    /// Returns None if not cached or expired.
    pub fn get_trending(&self) -> Option<Vec<GifResult>> {
        let cache = self.trending_cache.lock();

        if let Some(ref cached) = *cache {
            if cached.cached_at.elapsed() < self.trending_ttl {
                return Some(cached.results.clone());
            }
        }
        None
    }

    /// Store trending results in the cache.
    pub fn put_trending(&self, results: Vec<GifResult>) {
        let mut cache = self.trending_cache.lock();
        *cache = Some(CachedSearchResults {
            results,
            cached_at: Instant::now(),
        });
    }

    /// Clear all caches.
    pub fn clear(&self) {
        self.search_cache.lock().clear();
        *self.trending_cache.lock() = None;
    }

    /// Normalize a search query for cache key consistency.
    fn normalize_query(query: &str) -> String {
        query.trim().to_lowercase()
    }
}

impl Default for GifCache {
    fn default() -> Self {
        Self::new()
    }
}
