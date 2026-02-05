//! Reactive state container.
//!
//! Generic reactive state store with subscription support.

use std::sync::Arc;

use parking_lot::RwLock;
use tokio::sync::broadcast;

/// Reactive state container with subscription support.
///
/// `Store<T>` provides a thread-safe container for application state
/// that supports reactive updates through broadcast channels.
///
/// # Type Requirements
///
/// - `Clone`: State must be cloneable for broadcasting to subscribers
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
/// // Subscriber receives updated state
/// let new_state = rx.recv().await.unwrap();
/// assert_eq!(new_state.count, 1);
/// ```
pub struct Store<T: Clone + Send + Sync + 'static> {
    /// The current state, protected by a read-write lock.
    state: Arc<RwLock<T>>,
    /// Broadcast sender for notifying subscribers of state changes.
    sender: broadcast::Sender<T>,
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

    /// Replace the current state with a new value.
    ///
    /// All subscribers will receive the new state.
    pub fn set(&self, value: T) {
        {
            let mut state = self.state.write();
            *state = value.clone();
        }
        // Ignore send errors (no subscribers is fine)
        let _ = self.sender.send(value);
    }

    /// Update the state in place using a closure.
    ///
    /// The closure receives a mutable reference to the state.
    /// After the update, all subscribers receive the new state.
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
        let new_state = {
            let mut state = self.state.write();
            f(&mut state);
            state.clone()
        };
        // Ignore send errors (no subscribers is fine)
        let _ = self.sender.send(new_state);
    }

    /// Subscribe to state changes.
    ///
    /// Returns a receiver that will receive a copy of the state
    /// each time it changes. The receiver may miss updates if
    /// the channel buffer fills up (lagged).
    ///
    /// # Note
    ///
    /// The receiver does NOT receive the current state immediately.
    /// Call `get()` first if you need the current state before
    /// waiting for changes.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<T> {
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
    fn test_store_set() {
        let store = Store::new(TestState::default());

        store.set(TestState {
            value: 100,
            name: "updated".to_string(),
        });

        let state = store.get();
        assert_eq!(state.value, 100);
        assert_eq!(state.name, "updated");
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
    async fn test_store_subscribe() {
        let store = Store::new(TestState::default());
        let mut rx = store.subscribe();

        // Update state
        store.set(TestState {
            value: 99,
            name: "subscribed".to_string(),
        });

        // Subscriber should receive the update
        let received = rx.recv().await.unwrap();
        assert_eq!(received.value, 99);
        assert_eq!(received.name, "subscribed");
    }

    #[tokio::test]
    async fn test_store_multiple_subscribers() {
        let store = Store::new(TestState::default());
        let mut rx1 = store.subscribe();
        let mut rx2 = store.subscribe();

        store.update(|s| s.value = 42);

        // Both subscribers should receive the update
        let received1 = rx1.recv().await.unwrap();
        let received2 = rx2.recv().await.unwrap();
        assert_eq!(received1.value, 42);
        assert_eq!(received2.value, 42);
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
}
