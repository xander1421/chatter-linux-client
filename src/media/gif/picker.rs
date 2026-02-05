//! GIF picker.
//!
//! Coordinates GIF client and cache for search and selection.

use super::cache::GifCache;
use super::client::{GifClient, GifResult};
use crate::error::MediaError;

/// Default search limit.
const DEFAULT_SEARCH_LIMIT: u32 = 20;

/// Default trending limit.
const DEFAULT_TRENDING_LIMIT: u32 = 30;

/// GIF picker coordinator.
///
/// Provides a high-level interface for GIF search and selection,
/// coordinating between the API client and cache.
pub struct GifPicker {
    /// Tenor API client.
    client: GifClient,
    /// Search and trending cache.
    cache: GifCache,
}

impl GifPicker {
    /// Create a new GIF picker with the given API key.
    pub fn new(api_key: String) -> Self {
        Self {
            client: GifClient::new(api_key),
            cache: GifCache::new(),
        }
    }

    /// Search for GIFs matching the query.
    ///
    /// Results are cached for faster subsequent lookups.
    pub async fn search(&self, query: &str) -> Result<Vec<GifResult>, MediaError> {
        // Check cache first
        if let Some(results) = self.cache.get_search(query) {
            return Ok(results);
        }

        // Fetch from API
        let results = self.client.search(query, DEFAULT_SEARCH_LIMIT).await?;

        // Store in cache
        self.cache.put_search(query, results.clone());

        Ok(results)
    }

    /// Get trending GIFs.
    ///
    /// Results are cached with a 5-minute TTL.
    pub async fn trending(&self) -> Result<Vec<GifResult>, MediaError> {
        // Check cache first
        if let Some(results) = self.cache.get_trending() {
            return Ok(results);
        }

        // Fetch from API
        let results = self.client.trending(DEFAULT_TRENDING_LIMIT).await?;

        // Store in cache
        self.cache.put_trending(results.clone());

        Ok(results)
    }

    /// Get a specific GIF by ID.
    ///
    /// Individual lookups are not cached.
    pub async fn get_gif(&self, id: &str) -> Result<Option<GifResult>, MediaError> {
        self.client.get_by_id(id).await
    }

    /// Clear the cache.
    pub fn clear_cache(&self) {
        self.cache.clear();
    }
}
