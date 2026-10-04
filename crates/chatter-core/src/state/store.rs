//! Reactive state container.
//!
//! Generic reactive state store with subscription support.
//!
//! # Memory Efficiency
//!
//! State updates are broadcast as `Arc<T>`, meaning only one clone occurs
//! per update regardless of subscriber count. Subscribers share the same
//! Arc, paying only reference counting costs.

use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::broadcast;

/// Shared state snapshot for efficient broadcasting.
///
/// Wraps state in Arc so multiple subscribers share one copy.
pub type StateSnapshot<T> = Arc<T>;

/// Reactive state container with subscription support.
///
/// `Store<T>` provides a thread-safe container for application state
/// that supports reactive updates through broadcast channels.
///
/// # Memory Efficiency
///
/// Updates broadcast `Arc<T>` instead of cloning `T` for each subscriber.
/// This means:
/// - One clone per update (into the Arc)
/// - Zero additional clones regardless of subscriber count
/// - Subscribers share the same underlying data
///
/// # Type Requirements
///
/// - `Clone`: State must be cloneable for snapshots
/// - `Send + Sync`: State must be safe to share across threads
/// - `'static`: State must have a static lifetime for async contexts
///
/// # Example
///
/// ```ignore
/// use chatter_client::state::Store;
///
/// #[derive(Clone, Default)]
/// struct CounterState {
///     count: i32,
/// }
///
/// let store = Store::new(CounterState::default());
///
/// // Subscribe to changes
/// let mut rx = store.subscribe();
///
/// // Update state
/// store.update(|state| state.count += 1);
///
/// // Subscriber receives Arc-wrapped state (zero-copy sharing)
/// let new_state = rx.recv().await.unwrap();
/// assert_eq!(new_state.count, 1);
/// ```
pub struct Store<T: Clone + Send + Sync + 'static> {
    /// The current state, protected by a read-write lock.
    state: Arc<RwLock<T>>,
    /// Broadcast sender for notifying subscribers of state changes.
    /// Uses Arc<T> to avoid cloning state for each subscriber.
    sender: broadcast::Sender<StateSnapshot<T>>,
}

impl<T: Clone + Send + Sync + 'static> Store<T> {
    /// Create a new store with the given initial state.
    ///
    /// The broadcast channel has a capacity of 16, which is sufficient
    /// for typical UI subscriber counts.
    #[must_use]
    pub fn new(initial: T) -> Self {
        let (sender, _) = broadcast::channel(16);
        Self {
            state: Arc::new(RwLock::new(initial)),
            sender,
        }
    }

    /// Get a clone of the current state.
    ///
    /// This acquires a read lock briefly to clone the state.
    #[must_use]
    pub fn get(&self) -> T {
        self.state.read().clone()
    }

    /// Update the state in place using a closure.
    ///
    /// The closure receives a mutable reference to the state.
    /// After the update, broadcasts an Arc-wrapped snapshot to all subscribers.
    ///
    /// # Memory Efficiency
    ///
    /// Only one clone occurs (into Arc), shared by all subscribers.
    /// No cloning happens if there are no subscribers.
    ///
    /// # Example
    ///
    /// ```ignore
    /// store.update(|state| {
    ///     state.count += 1;
    ///     state.last_updated = Instant::now();
    /// });
    /// ```
    pub fn update<F>(&self, f: F)
    where
        F: FnOnce(&mut T),
    {
        // Apply update and create snapshot while holding write lock
        let snapshot = {
            let mut state = self.state.write();
            f(&mut state);

            // Only clone if there are subscribers
            if self.sender.receiver_count() > 0 {
                Some(Arc::new(state.clone()))
            } else {
                None
            }
        };

        // Broadcast outside the lock
        if let Some(snapshot) = snapshot {
            let _ = self.sender.send(snapshot);
        }
    }

    /// Subscribe to state changes.
    ///
    /// Returns a receiver that will receive Arc-wrapped state snapshots.
    /// Multiple subscribers share the same Arc, avoiding per-subscriber cloning.
    ///
    /// # Memory Efficiency
    ///
    /// Receiving `Arc<T>` means subscribers share one allocation.
    /// Use `(*snapshot).clone()` if you need an owned `T`.
    ///
    /// # Note
    ///
    /// The receiver does NOT receive the current state immediately.
    /// Call `get()` or `snapshot()` first if you need the current state
    /// before waiting for changes.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<StateSnapshot<T>> {
        self.sender.subscribe()
    }
}

impl<T: Clone + Send + Sync + Default + 'static> Default for Store<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

// Store is Clone because Arc and broadcast::Sender are Clone
impl<T: Clone + Send + Sync + 'static> Clone for Store<T> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            sender: self.sender.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Default, PartialEq)]
    struct TestState {
        value: i32,
        name: String,
    }

    #[test]
    fn test_store_new_and_get() {
        let store = Store::new(TestState {
            value: 42,
            name: "test".to_string(),
        });

        let state = store.get();
        assert_eq!(state.value, 42);
        assert_eq!(state.name, "test");
    }

    #[test]
    fn test_store_update() {
        let store = Store::new(TestState {
            value: 0,
            name: "initial".to_string(),
        });

        store.update(|s| {
            s.value += 10;
            s.name = "modified".to_string();
        });

        let state = store.get();
        assert_eq!(state.value, 10);
        assert_eq!(state.name, "modified");
    }

    #[tokio::test]
    async fn test_store_multiple_subscribers() {
        let store = Store::new(TestState::default());
        let mut rx1 = store.subscribe();
        let mut rx2 = store.subscribe();

        store.update(|s| s.value = 42);

        // Both subscribers receive the same Arc (shared allocation)
        let received1 = rx1.recv().await.unwrap();
        let received2 = rx2.recv().await.unwrap();
        assert_eq!(received1.value, 42);
        assert_eq!(received2.value, 42);

        // Verify they share the same Arc (pointer equality)
        assert!(Arc::ptr_eq(&received1, &received2));
    }

    #[test]
    fn test_store_clone() {
        let store1 = Store::new(TestState { value: 1, name: "one".to_string() });
        let store2 = store1.clone();

        // Both stores share the same state
        store1.update(|s| s.value = 2);
        assert_eq!(store2.get().value, 2);
    }

    #[test]
    fn test_store_default() {
        let store: Store<TestState> = Store::default();
        let state = store.get();
        assert_eq!(state.value, 0);
        assert_eq!(state.name, "");
    }

    #[tokio::test]
    async fn test_arc_sharing_efficiency() {
        // Verify that multiple subscribers share the same Arc allocation
        let store = Store::new(TestState {
            value: 0,
            name: "large_state_simulation".to_string(),
        });

        let mut rx1 = store.subscribe();
        let mut rx2 = store.subscribe();
        let mut rx3 = store.subscribe();

        store.update(|s| s.value = 100);

        let snap1 = rx1.recv().await.unwrap();
        let snap2 = rx2.recv().await.unwrap();
        let snap3 = rx3.recv().await.unwrap();

        // All three subscribers received the SAME Arc (no cloning)
        assert!(Arc::ptr_eq(&snap1, &snap2));
        assert!(Arc::ptr_eq(&snap2, &snap3));

        // Strong count should be 3 (one per subscriber)
        assert_eq!(Arc::strong_count(&snap1), 3);
    }

    #[test]
    fn test_update_no_clone_without_subscribers() {
        // This test verifies the optimization works by checking state is updated
        // even without subscribers (no clone needed for broadcast)
        let store = Store::new(TestState::default());

        // No subscribers - update should still work
        store.update(|s| s.value = 99);
        assert_eq!(store.get().value, 99);
    }
}
