//! Bounded collections.
//!
//! Size-bounded collections with automatic eviction when capacity is reached.

use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

/// Bounded hash set with FIFO eviction.
///
/// Tracks insertion order to evict oldest entries when capacity is reached.
/// Useful for deduplication where we need to remember "recently seen" items.
pub struct BoundedHashSet<T> {
    set: HashSet<T>,
    order: VecDeque<T>,
    capacity: usize,
}

impl<T: Hash + Eq + Clone> BoundedHashSet<T> {
    /// Create a new bounded hash set with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if capacity is 0.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be greater than 0");
        Self {
            set: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Check if a value is present in the set.
    pub fn contains(&self, value: &T) -> bool {
        self.set.contains(value)
    }

    /// Insert a value into the set.
    ///
    /// Returns `true` if this is a new value (not previously present).
    /// Returns `false` if the value was already in the set (duplicate).
    ///
    /// When at capacity, the oldest entry is evicted to make room.
    pub fn insert(&mut self, value: T) -> bool {
        if self.set.contains(&value) {
            return false;
        }

        // Evict oldest entry if at capacity
        if self.set.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }

        self.set.insert(value.clone());
        self.order.push_back(value);
        true
    }

    /// Returns the number of elements in the set.
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// Returns true if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// Returns the maximum capacity of the set.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_bounded_hash_set() {
        let set: BoundedHashSet<String> = BoundedHashSet::new(100);
        assert_eq!(set.capacity(), 100);
        assert_eq!(set.len(), 0);
        assert!(set.is_empty());
    }

    #[test]
    #[should_panic(expected = "capacity must be greater than 0")]
    fn test_zero_capacity_panics() {
        let _set: BoundedHashSet<String> = BoundedHashSet::new(0);
    }

    #[test]
    fn test_insert_and_contains() {
        let mut set: BoundedHashSet<String> = BoundedHashSet::new(10);

        assert!(set.insert("a".to_string()));
        assert!(set.contains(&"a".to_string()));
        assert_eq!(set.len(), 1);

        // Duplicate returns false
        assert!(!set.insert("a".to_string()));
        assert_eq!(set.len(), 1);

        assert!(set.insert("b".to_string()));
        assert!(set.contains(&"b".to_string()));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_eviction_at_capacity() {
        let mut set: BoundedHashSet<i32> = BoundedHashSet::new(3);

        // Fill to capacity
        assert!(set.insert(1));
        assert!(set.insert(2));
        assert!(set.insert(3));
        assert_eq!(set.len(), 3);

        // Adding 4th should evict oldest (1)
        assert!(set.insert(4));
        assert_eq!(set.len(), 3);
        assert!(!set.contains(&1)); // Evicted
        assert!(set.contains(&2));
        assert!(set.contains(&3));
        assert!(set.contains(&4));

        // Adding 5th should evict 2
        assert!(set.insert(5));
        assert!(!set.contains(&2)); // Evicted
        assert!(set.contains(&3));
        assert!(set.contains(&4));
        assert!(set.contains(&5));
    }

    #[test]
    fn test_duplicate_does_not_evict() {
        let mut set: BoundedHashSet<i32> = BoundedHashSet::new(3);

        set.insert(1);
        set.insert(2);
        set.insert(3);

        // Re-inserting existing item should not evict
        assert!(!set.insert(2));
        assert_eq!(set.len(), 3);
        assert!(set.contains(&1));
        assert!(set.contains(&2));
        assert!(set.contains(&3));
    }

    #[test]
    fn test_fifo_eviction_order() {
        let mut set: BoundedHashSet<&str> = BoundedHashSet::new(2);

        set.insert("first");
        set.insert("second");

        // Evicts "first"
        set.insert("third");
        assert!(!set.contains(&"first"));
        assert!(set.contains(&"second"));
        assert!(set.contains(&"third"));

        // Evicts "second"
        set.insert("fourth");
        assert!(!set.contains(&"second"));
        assert!(set.contains(&"third"));
        assert!(set.contains(&"fourth"));
    }
}
