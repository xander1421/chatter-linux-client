//! Message deduplication.
//!
//! Prevent duplicate message processing using bounded GUID tracking.

use parking_lot::RwLock;

use crate::util::bounded_vec::BoundedHashSet;

/// Default capacity for recently processed messages.
const DEFAULT_CAPACITY: usize = 10_000;

/// Statistics about the deduplicator state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeduplicatorStats {
    /// Number of GUIDs currently being tracked.
    pub tracked_count: usize,
    /// Maximum capacity of the deduplicator.
    pub capacity: usize,
}

/// Message deduplicator using bounded GUID tracking.
///
/// Prevents duplicate message processing by tracking recently seen message GUIDs.
/// Uses a bounded hash set with FIFO eviction to limit memory usage.
///
/// # Thread Safety
///
/// Uses `RwLock` for concurrent access - reads (`is_duplicate`) can proceed
/// in parallel, while writes (`mark_processed`) are serialized.
pub struct MessageDeduplicator {
    processed: RwLock<BoundedHashSet<String>>,
    capacity: usize,
}

impl MessageDeduplicator {
    /// Create a new deduplicator with the specified capacity.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Maximum number of GUIDs to track. When exceeded,
    ///   oldest entries are evicted.
    ///
    /// # Panics
    ///
    /// Panics if capacity is 0.
    pub fn new(capacity: usize) -> Self {
        Self {
            processed: RwLock::new(BoundedHashSet::new(capacity)),
            capacity,
        }
    }

    /// Create a new deduplicator with the default capacity (10,000 messages).
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }

    /// Check if a message GUID has been seen before.
    ///
    /// Returns `true` if this is a duplicate (GUID was previously processed).
    /// Returns `false` if the GUID has not been seen.
    ///
    /// # Note
    ///
    /// This only checks - it does not mark the GUID as processed.
    /// Use `mark_processed` to both check and mark atomically.
    pub fn is_duplicate(&self, guid: &str) -> bool {
        self.processed.read().contains(&guid.to_string())
    }

    /// Mark a message GUID as processed.
    ///
    /// Returns `true` if this was a new GUID (not a duplicate).
    /// Returns `false` if the GUID was already present (duplicate).
    ///
    /// This method atomically checks and marks the GUID, which is the
    /// preferred way to handle deduplication in most cases.
    pub fn mark_processed(&self, guid: &str) -> bool {
        self.processed.write().insert(guid.to_string())
    }

    /// Get current statistics for monitoring.
    pub fn stats(&self) -> DeduplicatorStats {
        let processed = self.processed.read();
        DeduplicatorStats {
            tracked_count: processed.len(),
            capacity: self.capacity,
        }
    }
}

impl Default for MessageDeduplicator {
    fn default() -> Self {
        Self::with_default_capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_deduplicator() {
        let dedup = MessageDeduplicator::new(100);
        let stats = dedup.stats();
        assert_eq!(stats.capacity, 100);
        assert_eq!(stats.tracked_count, 0);
    }

    #[test]
    fn test_default_capacity() {
        let dedup = MessageDeduplicator::with_default_capacity();
        assert_eq!(dedup.stats().capacity, DEFAULT_CAPACITY);
    }

    #[test]
    fn test_is_duplicate_returns_false_for_new() {
        let dedup = MessageDeduplicator::new(100);
        assert!(!dedup.is_duplicate("guid-1"));
    }

    #[test]
    fn test_mark_processed_returns_true_for_new() {
        let dedup = MessageDeduplicator::new(100);

        // First time - not a duplicate
        assert!(dedup.mark_processed("guid-1"));
        assert_eq!(dedup.stats().tracked_count, 1);

        // Second time - is a duplicate
        assert!(!dedup.mark_processed("guid-1"));
        assert_eq!(dedup.stats().tracked_count, 1);
    }

    #[test]
    fn test_is_duplicate_after_mark() {
        let dedup = MessageDeduplicator::new(100);

        assert!(!dedup.is_duplicate("guid-1"));
        dedup.mark_processed("guid-1");
        assert!(dedup.is_duplicate("guid-1"));
    }

    #[test]
    fn test_eviction_at_capacity() {
        let dedup = MessageDeduplicator::new(3);

        dedup.mark_processed("guid-1");
        dedup.mark_processed("guid-2");
        dedup.mark_processed("guid-3");
        assert_eq!(dedup.stats().tracked_count, 3);

        // Adding 4th should evict guid-1
        dedup.mark_processed("guid-4");
        assert_eq!(dedup.stats().tracked_count, 3);
        assert!(!dedup.is_duplicate("guid-1")); // Evicted
        assert!(dedup.is_duplicate("guid-2"));
        assert!(dedup.is_duplicate("guid-3"));
        assert!(dedup.is_duplicate("guid-4"));
    }

    #[test]
    fn test_thread_safety() {
        use std::sync::Arc;
        use std::thread;

        let dedup = Arc::new(MessageDeduplicator::new(1000));
        let mut handles = vec![];

        // Spawn multiple threads that mark different GUIDs
        for i in 0..10 {
            let dedup_clone = Arc::clone(&dedup);
            let handle = thread::spawn(move || {
                for j in 0..100 {
                    dedup_clone.mark_processed(&format!("guid-{}-{}", i, j));
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(dedup.stats().tracked_count, 1000);
    }

    #[test]
    fn test_concurrent_duplicate_detection() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::thread;

        let dedup = Arc::new(MessageDeduplicator::new(100));
        let new_count = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];

        // Multiple threads trying to mark the same GUID
        for _ in 0..10 {
            let dedup_clone = Arc::clone(&dedup);
            let new_count_clone = Arc::clone(&new_count);
            let handle = thread::spawn(move || {
                if dedup_clone.mark_processed("same-guid") {
                    new_count_clone.fetch_add(1, Ordering::SeqCst);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Only one thread should have seen it as "new"
        assert_eq!(new_count.load(Ordering::SeqCst), 1);
        assert_eq!(dedup.stats().tracked_count, 1);
    }
}
