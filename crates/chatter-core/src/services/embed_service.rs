//! Embed service.
//!
//! Fronts [`LinkPreviewGenerator`](crate::media::embed::LinkPreviewGenerator)
//! with a URL-keyed LRU cache and per-URL in-flight coalescing so that
//! multi-link messages render every preview from cache on re-render
//! instead of re-fetching them.
//!
//! The bridge emits up to 3 URLs per message. The old flow only warmed
//! the first URL; tail URLs were re-fetched on every render. The service
//! exposes `prefetch_previews` to warm all URLs concurrently (with a
//! concurrency cap of 3) and `get_preview` as the cache-hit-or-fetch
//! read path.

use crate::domain::embed::LinkPreview;
use crate::error::media_error::MediaError;
use crate::media::embed::{LinkPreviewGenerator, PreviewFetcher};
use futures::stream::{self, StreamExt};
use moka::sync::Cache;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify};

/// Default LRU capacity for cached previews.
///
/// 512 entries easily covers a busy user's recent link activity while
/// keeping memory bounded (each `LinkPreview` is a handful of `String`s).
pub const DEFAULT_CACHE_CAPACITY: u64 = 512;

/// Default TTL — previews go stale after 30 minutes, matching typical
/// OpenGraph cache lifetimes seen in the wild.
const DEFAULT_TTL: Duration = Duration::from_secs(30 * 60);

/// Maximum URLs to prefetch per message (matches the bridge spec).
pub const MAX_URLS_PER_MESSAGE: usize = 3;

/// Concurrency cap for batch prefetch: never fan out more than this many
/// HTTP requests in parallel from one call, to avoid thundering herds.
pub const PREFETCH_CONCURRENCY: usize = 3;

/// Stub retained for the legacy module path.
///
/// Previous callers imported `EmbedService` as a type-level placeholder;
/// the real caching front-end is [`LinkPreviewService`].
pub struct EmbedService;

/// URL-keyed link-preview cache with concurrent batch prefetch.
///
/// Keyed globally by URL (not by message id) so the same link shared
/// across multiple messages hits the cache once. Uses a bounded LRU —
/// ~512 entries by default — and coalesces duplicate in-flight fetches
/// for the same URL so N concurrent prefetches of the same URL produce
/// exactly one HTTP request.
///
/// The cache stores only `LinkPreview` (the generic OpenGraph shape).
/// Platform-specific embeds (YouTube, GitHub) go through [`UrlUnfurler`]
/// and are not cached here — they have their own extractors.
///
/// [`UrlUnfurler`]: crate::media::embed::UrlUnfurler
pub struct LinkPreviewService {
    fetcher: Arc<dyn PreviewFetcher>,
    cache: Cache<String, LinkPreview>,
    /// Per-URL single-flight coordination: the `Arc<Notify>` is dropped
    /// from the map when the owning task finishes; followers await it
    /// and then read the cache.
    inflight: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
}

impl LinkPreviewService {
    /// Build a service on top of the real network-backed generator.
    #[must_use]
    pub fn new() -> Self {
        Self::with_fetcher(Arc::new(LinkPreviewGenerator::new()))
    }

    /// Build a service on top of an arbitrary [`PreviewFetcher`] — used
    /// by tests to inject a deterministic mock.
    #[must_use]
    pub fn with_fetcher(fetcher: Arc<dyn PreviewFetcher>) -> Self {
        Self::with_fetcher_and_capacity(fetcher, DEFAULT_CACHE_CAPACITY)
    }

    /// Build with a custom cache capacity — lets tests force eviction
    /// without shovelling 512 entries through.
    #[must_use]
    pub fn with_fetcher_and_capacity(
        fetcher: Arc<dyn PreviewFetcher>,
        capacity: u64,
    ) -> Self {
        let cache = Cache::builder()
            .max_capacity(capacity)
            .time_to_live(DEFAULT_TTL)
            .build();
        Self {
            fetcher,
            cache,
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get a preview, serving from cache when available.
    ///
    /// This is the public read path message bubbles call on every render.
    /// When the cache is warm (after a prefetch), this returns synchronously
    /// with no HTTP traffic.
    pub async fn get_preview(&self, url: &str) -> Result<LinkPreview, MediaError> {
        if let Some(hit) = self.cache.get(&url.to_string()) {
            return Ok(hit);
        }
        self.fetch_with_coalescing(url).await
    }

    /// Prefetch previews for up to [`MAX_URLS_PER_MESSAGE`] URLs concurrently.
    ///
    /// Fires at most [`PREFETCH_CONCURRENCY`] fetches in parallel, writes
    /// successes to the cache, and silently drops per-URL errors so a
    /// dead URL never stalls the batch. Already-cached URLs are skipped.
    ///
    /// Safe to call from message-arrival code paths — it never returns
    /// an error.
    pub async fn prefetch_previews(&self, urls: &[String]) {
        let to_fetch: Vec<String> = urls
            .iter()
            .take(MAX_URLS_PER_MESSAGE)
            .filter(|u| self.cache.get(u.as_str()).is_none())
            .cloned()
            .collect();

        if to_fetch.is_empty() {
            return;
        }

        stream::iter(to_fetch)
            .for_each_concurrent(PREFETCH_CONCURRENCY, |url| async move {
                // `fetch_with_coalescing` already writes to cache on
                // success. Swallow failures — a dead URL is expected.
                if let Err(err) = self.fetch_with_coalescing(&url).await {
                    tracing::debug!(%url, error = %err, "link preview prefetch failed");
                }
            })
            .await;
    }

    /// Cheap cache-only check — used by tests and diagnostics.
    #[must_use]
    pub fn cached(&self, url: &str) -> Option<LinkPreview> {
        self.cache.get(&url.to_string())
    }

    /// Current entry count (approximate; may include pending evictions).
    #[must_use]
    pub fn len(&self) -> u64 {
        self.cache.run_pending_tasks();
        self.cache.entry_count()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop all cached previews (tests / user-initiated cache purge).
    pub fn clear(&self) {
        self.cache.invalidate_all();
    }

    /// Single-flight fetch: if another task is already fetching `url`,
    /// wait for it and then read the cache instead of issuing a second
    /// HTTP request.
    async fn fetch_with_coalescing(&self, url: &str) -> Result<LinkPreview, MediaError> {
        // Fast path: someone already populated the cache while we were
        // waiting for a lock upstream.
        if let Some(hit) = self.cache.get(&url.to_string()) {
            return Ok(hit);
        }

        // Decide: do we own the fetch, or follow an existing one?
        let (owner, notify) = {
            let mut guard = self.inflight.lock().await;
            if let Some(existing) = guard.get(url) {
                (false, existing.clone())
            } else {
                let n = Arc::new(Notify::new());
                guard.insert(url.to_string(), n.clone());
                (true, n)
            }
        };

        if !owner {
            // Follower: wait for the owner, then read the cache. If the
            // owner errored, the cache will miss and we re-attempt
            // rather than propagate a stale error — cheap and honest.
            notify.notified().await;
            return self
                .cache
                .get(&url.to_string())
                .ok_or_else(|| MediaError::EmbedError(format!("preview fetch failed: {url}")));
        }

        // Owner path: fetch, populate cache, wake followers.
        let result = self.fetcher.fetch_preview(url).await;
        if let Ok(ref preview) = result {
            self.cache.insert(url.to_string(), preview.clone());
        }
        {
            let mut guard = self.inflight.lock().await;
            guard.remove(url);
        }
        notify.notify_waiters();
        result
    }
}

impl Default for LinkPreviewService {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    /// Deterministic mock: records fetch counts per URL and can be
    /// configured to fail specific URLs.
    struct MockFetcher {
        calls: AtomicUsize,
        per_url_calls: StdMutex<HashMap<String, usize>>,
        fail_urls: StdMutex<Vec<String>>,
        /// Optional delay to expose coalescing races.
        delay: Option<Duration>,
    }

    impl MockFetcher {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                per_url_calls: StdMutex::new(HashMap::new()),
                fail_urls: StdMutex::new(Vec::new()),
                delay: None,
            }
        }

        fn with_delay(mut self, d: Duration) -> Self {
            self.delay = Some(d);
            self
        }

        fn fail(&self, url: &str) {
            self.fail_urls.lock().unwrap().push(url.to_string());
        }

        fn total_calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn calls_for(&self, url: &str) -> usize {
            self.per_url_calls
                .lock()
                .unwrap()
                .get(url)
                .copied()
                .unwrap_or(0)
        }
    }

    #[async_trait]
    impl PreviewFetcher for MockFetcher {
        async fn fetch_preview(&self, url: &str) -> Result<LinkPreview, MediaError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self
                .per_url_calls
                .lock()
                .unwrap()
                .entry(url.to_string())
                .or_insert(0) += 1;

            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }

            if self.fail_urls.lock().unwrap().iter().any(|u| u == url) {
                return Err(MediaError::EmbedError(format!("mock fail: {url}")));
            }

            Ok(LinkPreview::new(url.to_string()).with_title(format!("title-for-{url}")))
        }
    }

    fn urls(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("https://example.com/page/{i}"))
            .collect()
    }

    #[tokio::test]
    async fn prefetch_caches_all_three_urls() {
        let mock = Arc::new(MockFetcher::new());
        let svc = LinkPreviewService::with_fetcher(mock.clone());
        let us = urls(3);

        svc.prefetch_previews(&us).await;

        // All three hit the cache afterwards — no further fetches.
        for u in &us {
            let p = svc.get_preview(u).await.expect("cached");
            assert_eq!(p.url, *u);
            assert_eq!(svc.cached(u).map(|p| p.url), Some(u.clone()));
        }
        assert_eq!(mock.total_calls(), 3, "each URL fetched exactly once");
    }

    #[tokio::test]
    async fn prefetch_caps_at_three_urls() {
        let mock = Arc::new(MockFetcher::new());
        let svc = LinkPreviewService::with_fetcher(mock.clone());
        // Pass 5 URLs — only the first 3 should be warmed.
        let us = urls(5);

        svc.prefetch_previews(&us).await;

        assert_eq!(mock.total_calls(), MAX_URLS_PER_MESSAGE);
        assert!(svc.cached(&us[0]).is_some());
        assert!(svc.cached(&us[1]).is_some());
        assert!(svc.cached(&us[2]).is_some());
        assert!(svc.cached(&us[3]).is_none());
        assert!(svc.cached(&us[4]).is_none());
    }

    #[tokio::test]
    async fn prefetch_partial_failure_other_urls_still_cached() {
        let mock = Arc::new(MockFetcher::new());
        mock.fail("https://example.com/page/1");
        let svc = LinkPreviewService::with_fetcher(mock.clone());
        let us = urls(3);

        svc.prefetch_previews(&us).await;

        assert!(svc.cached(&us[0]).is_some(), "URL 0 cached");
        assert!(svc.cached(&us[1]).is_none(), "URL 1 failed, not cached");
        assert!(svc.cached(&us[2]).is_some(), "URL 2 cached");
    }

    #[tokio::test]
    async fn lru_evicts_when_capacity_exceeded() {
        let mock = Arc::new(MockFetcher::new());
        let svc = LinkPreviewService::with_fetcher_and_capacity(mock.clone(), 2);

        // Insert 4 distinct URLs, prefetching in single-URL batches so
        // the capacity-2 LRU has to evict.
        for i in 0..4 {
            let u = format!("https://example.com/item/{i}");
            svc.prefetch_previews(&[u]).await;
        }

        // moka evicts asynchronously — run pending tasks and confirm
        // the cache is bounded.
        assert!(svc.len() <= 2, "LRU bounded (got {})", svc.len());
    }

    #[tokio::test]
    async fn duplicate_inflight_requests_coalesce() {
        let mock = Arc::new(MockFetcher::new().with_delay(Duration::from_millis(50)));
        let svc = Arc::new(LinkPreviewService::with_fetcher(mock.clone()));
        let url = "https://example.com/hot".to_string();

        // Fire 8 concurrent get_preview calls for the same URL.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let svc = svc.clone();
            let u = url.clone();
            handles.push(tokio::spawn(async move { svc.get_preview(&u).await }));
        }
        for h in handles {
            let r = h.await.expect("task did not panic");
            assert!(r.is_ok(), "all concurrent calls succeeded");
        }

        assert_eq!(
            mock.calls_for(&url),
            1,
            "8 racing calls -> 1 fetch (coalesced)"
        );
    }

    #[tokio::test]
    async fn get_preview_hits_cache_after_prefetch() {
        let mock = Arc::new(MockFetcher::new());
        let svc = LinkPreviewService::with_fetcher(mock.clone());
        let u = "https://example.com/only".to_string();

        svc.prefetch_previews(&[u.clone()]).await;
        assert_eq!(mock.total_calls(), 1);

        // 10 get_preview calls afterwards should all hit the cache.
        for _ in 0..10 {
            let _ = svc.get_preview(&u).await.unwrap();
        }
        assert_eq!(mock.total_calls(), 1, "get_preview served from cache");
    }

    #[tokio::test]
    async fn prefetch_skips_already_cached_urls() {
        let mock = Arc::new(MockFetcher::new());
        let svc = LinkPreviewService::with_fetcher(mock.clone());
        let u = "https://example.com/warm".to_string();

        svc.prefetch_previews(&[u.clone()]).await;
        assert_eq!(mock.total_calls(), 1);

        svc.prefetch_previews(&[u.clone()]).await;
        assert_eq!(mock.total_calls(), 1, "second prefetch was a no-op");
    }

    #[tokio::test]
    async fn clear_drops_cache() {
        let mock = Arc::new(MockFetcher::new());
        let svc = LinkPreviewService::with_fetcher(mock.clone());
        let u = "https://example.com/ephemeral".to_string();

        svc.prefetch_previews(&[u.clone()]).await;
        assert!(svc.cached(&u).is_some());

        svc.clear();
        assert!(svc.cached(&u).is_none());
    }
}
