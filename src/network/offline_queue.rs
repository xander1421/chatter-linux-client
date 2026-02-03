//! Offline operation queue.
//!
//! Queue operations when offline, retry when reconnected.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

/// Types of operations that can be queued.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum OperationType {
    /// Send a message to a channel
    SendMessage {
        channel_id: String,
        content: Vec<u8>, // Serialized message content
    },
    /// Send a reaction
    SendReaction { message_id: String, emoji: String },
    /// Mark messages as read
    MarkRead {
        channel_id: String,
        up_to_message_id: String,
    },
    /// Send typing indicator
    SendTyping { channel_id: String },
    /// Edit a message
    EditMessage { message_id: String, content: Vec<u8> },
    /// Delete a message
    DeleteMessage { message_id: String },
}

impl OperationType {
    /// Get the type name for statistics.
    pub fn type_name(&self) -> &'static str {
        match self {
            OperationType::SendMessage { .. } => "SendMessage",
            OperationType::SendReaction { .. } => "SendReaction",
            OperationType::MarkRead { .. } => "MarkRead",
            OperationType::SendTyping { .. } => "SendTyping",
            OperationType::EditMessage { .. } => "EditMessage",
            OperationType::DeleteMessage { .. } => "DeleteMessage",
        }
    }
}

/// A queued operation waiting for network connectivity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueuedOperation {
    /// Unique identifier for this operation
    pub id: Uuid,
    /// The operation to perform
    pub operation: OperationType,
    /// When the operation was queued (Unix timestamp millis)
    pub queued_at: u64,
    /// Number of replay attempts
    pub attempts: u32,
    /// Maximum attempts before dropping (None = unlimited)
    pub max_attempts: Option<u32>,
    /// Priority (lower = higher priority)
    pub priority: u8,
}

impl QueuedOperation {
    /// Create a new queued operation with default settings.
    pub fn new(operation: OperationType) -> Self {
        Self {
            id: Uuid::new_v4(),
            operation,
            queued_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            attempts: 0,
            max_attempts: Some(5),
            priority: 100, // Default priority
        }
    }

    /// Create with specific priority.
    pub fn with_priority(operation: OperationType, priority: u8) -> Self {
        let mut op = Self::new(operation);
        op.priority = priority;
        op
    }

    /// Create with specific max attempts.
    pub fn with_max_attempts(operation: OperationType, max_attempts: Option<u32>) -> Self {
        let mut op = Self::new(operation);
        op.max_attempts = max_attempts;
        op
    }
}

/// Queue events for subscribers.
#[derive(Clone, Debug)]
pub enum QueueEvent {
    /// Operation added to queue
    Added(Uuid),
    /// Operation removed (completed or expired)
    Removed(Uuid),
    /// Queue cleared
    Cleared,
    /// Replay started
    ReplayStarted { count: usize },
    /// Replay completed
    ReplayCompleted { succeeded: usize, failed: usize },
}

/// Errors that can occur during queue operations.
#[derive(Clone, Debug, thiserror::Error)]
pub enum QueueError {
    /// Queue is at capacity
    #[error("queue is full (max size: {0})")]
    QueueFull(usize),
    /// Serialized operation exceeds size limit
    #[error("operation too large (size: {size}, max: {max})")]
    OperationTooLarge { size: usize, max: usize },
}

/// Queue statistics.
#[derive(Clone, Debug, Default)]
pub struct QueueStats {
    /// Total number of operations in queue
    pub total_operations: usize,
    /// Age of the oldest operation
    pub oldest_operation_age: Option<Duration>,
    /// Count of operations by type
    pub operations_by_type: HashMap<String, usize>,
}

/// Queue for offline operations.
///
/// Thread-safe queue that stores operations when offline and replays
/// them when connectivity is restored. Supports priority ordering,
/// persistence, and event broadcasting.
///
/// # Example
///
/// ```
/// use chatter_linux::network::offline_queue::{OfflineQueue, OperationType};
///
/// let queue = OfflineQueue::new(100);
///
/// // Subscribe to events
/// let mut rx = queue.subscribe();
///
/// // Queue an operation
/// let id = queue.enqueue(OperationType::SendMessage {
///     channel_id: "channel-123".to_string(),
///     content: b"Hello!".to_vec(),
/// }).unwrap();
///
/// // Check queue state
/// assert_eq!(queue.len(), 1);
/// assert!(!queue.is_empty());
///
/// // Peek at next operation
/// if let Some(op) = queue.peek() {
///     println!("Next operation: {:?}", op.operation);
/// }
///
/// // Dequeue for processing
/// let op = queue.dequeue();
/// assert!(op.is_some());
/// assert!(queue.is_empty());
/// ```
pub struct OfflineQueue {
    /// In-memory queue (sorted by priority, then queued_at)
    operations: RwLock<Vec<QueuedOperation>>,
    /// Maximum queue size
    max_size: usize,
    /// Maximum serialized operation size (bytes)
    max_operation_size: usize,
    /// Event sender for queue changes
    events: broadcast::Sender<QueueEvent>,
}

impl OfflineQueue {
    /// Broadcast channel capacity.
    const CHANNEL_CAPACITY: usize = 16;

    /// Default maximum operation size (1MB).
    const DEFAULT_MAX_OPERATION_SIZE: usize = 1024 * 1024;

    /// Create a new OfflineQueue with the given maximum size.
    ///
    /// # Arguments
    ///
    /// * `max_size` - Maximum number of operations in the queue
    pub fn new(max_size: usize) -> Self {
        let (events, _) = broadcast::channel(Self::CHANNEL_CAPACITY);
        Self {
            operations: RwLock::new(Vec::with_capacity(max_size.min(1000))),
            max_size,
            max_operation_size: Self::DEFAULT_MAX_OPERATION_SIZE,
            events,
        }
    }

    /// Create a new OfflineQueue with custom limits.
    ///
    /// # Arguments
    ///
    /// * `max_size` - Maximum number of operations in the queue
    /// * `max_operation_size` - Maximum serialized size of a single operation
    pub fn with_limits(max_size: usize, max_operation_size: usize) -> Self {
        let (events, _) = broadcast::channel(Self::CHANNEL_CAPACITY);
        Self {
            operations: RwLock::new(Vec::with_capacity(max_size.min(1000))),
            max_size,
            max_operation_size,
            events,
        }
    }

    /// Add an operation to the queue.
    ///
    /// Returns the operation's UUID on success.
    ///
    /// # Errors
    ///
    /// Returns `QueueError::QueueFull` if the queue is at capacity.
    /// Returns `QueueError::OperationTooLarge` if the serialized operation
    /// exceeds the maximum size.
    pub fn enqueue(&self, operation: OperationType) -> Result<Uuid, QueueError> {
        self.enqueue_with_priority(operation, 100)
    }

    /// Add an operation with specific priority.
    ///
    /// Lower priority values are processed first.
    ///
    /// # Arguments
    ///
    /// * `operation` - The operation to queue
    /// * `priority` - Priority level (0 = highest, 255 = lowest)
    pub fn enqueue_with_priority(
        &self,
        operation: OperationType,
        priority: u8,
    ) -> Result<Uuid, QueueError> {
        let queued_op = QueuedOperation::with_priority(operation, priority);
        self.enqueue_operation(queued_op)
    }

    /// Add a fully configured operation to the queue.
    pub fn enqueue_operation(&self, op: QueuedOperation) -> Result<Uuid, QueueError> {
        // Check serialized size
        let serialized = serde_json::to_vec(&op).unwrap_or_default();
        if serialized.len() > self.max_operation_size {
            return Err(QueueError::OperationTooLarge {
                size: serialized.len(),
                max: self.max_operation_size,
            });
        }

        let id = op.id;

        let mut ops = self.operations.write();

        // Check capacity
        if ops.len() >= self.max_size {
            return Err(QueueError::QueueFull(self.max_size));
        }

        // Insert in sorted order (by priority, then queued_at)
        let pos = ops
            .binary_search_by(|existing| {
                existing
                    .priority
                    .cmp(&op.priority)
                    .then_with(|| existing.queued_at.cmp(&op.queued_at))
            })
            .unwrap_or_else(|e| e);

        ops.insert(pos, op);

        // Emit event (ignore send errors if no receivers)
        let _ = self.events.send(QueueEvent::Added(id));

        Ok(id)
    }

    /// Remove and return the highest priority operation.
    ///
    /// Returns `None` if the queue is empty.
    pub fn dequeue(&self) -> Option<QueuedOperation> {
        let mut ops = self.operations.write();
        if ops.is_empty() {
            return None;
        }

        let op = ops.remove(0);
        let _ = self.events.send(QueueEvent::Removed(op.id));
        Some(op)
    }

    /// View the next operation without removing it.
    ///
    /// Returns `None` if the queue is empty.
    pub fn peek(&self) -> Option<QueuedOperation> {
        let ops = self.operations.read();
        ops.first().cloned()
    }

    /// Remove a specific operation by ID.
    ///
    /// Returns `true` if the operation was found and removed.
    pub fn remove(&self, id: Uuid) -> bool {
        let mut ops = self.operations.write();
        if let Some(pos) = ops.iter().position(|op| op.id == id) {
            ops.remove(pos);
            let _ = self.events.send(QueueEvent::Removed(id));
            true
        } else {
            false
        }
    }

    /// Remove all operations from the queue.
    pub fn clear(&self) {
        let mut ops = self.operations.write();
        ops.clear();
        let _ = self.events.send(QueueEvent::Cleared);
    }

    /// Get the number of operations in the queue.
    pub fn len(&self) -> usize {
        self.operations.read().len()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.operations.read().is_empty()
    }

    /// Subscribe to queue events.
    ///
    /// Returns a receiver that will get all future queue events.
    pub fn subscribe(&self) -> broadcast::Receiver<QueueEvent> {
        self.events.subscribe()
    }

    /// Get the maximum queue size.
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    // ==================== Persistence Methods ====================

    /// Save queue to persistent storage.
    ///
    /// Serializes queue to JSON and writes to the given path.
    /// Uses atomic rename pattern (write to .tmp, then rename) for safety.
    /// Called periodically and on app shutdown.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to save the queue data
    ///
    /// # Errors
    ///
    /// Returns an IO error if writing fails.
    pub async fn save(&self, path: &Path) -> std::io::Result<()> {
        // Clone operations while holding lock briefly
        let ops: Vec<QueuedOperation> = self.operations.read().clone();

        // Serialize to JSON
        let json = serde_json::to_string_pretty(&ops)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        // Write to temporary file first
        let tmp_path = path.with_extension("tmp");
        tokio::fs::write(&tmp_path, json).await?;

        // Atomic rename
        tokio::fs::rename(&tmp_path, path).await?;

        Ok(())
    }

    /// Load queue from persistent storage.
    ///
    /// Reads and deserializes queue from the given path.
    /// Called on app startup.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to load the queue data from
    ///
    /// # Returns
    ///
    /// Number of operations loaded.
    ///
    /// # Errors
    ///
    /// Returns an IO error if reading fails (except for missing file,
    /// which returns Ok(0)).
    pub async fn load(&self, path: &Path) -> std::io::Result<usize> {
        // Handle missing file gracefully
        if !path.exists() {
            return Ok(0);
        }

        let json = tokio::fs::read_to_string(path).await?;

        let loaded_ops: Vec<QueuedOperation> = serde_json::from_str(&json)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let count = loaded_ops.len();

        // Replace current operations
        let mut ops = self.operations.write();
        *ops = loaded_ops;

        // Sort to ensure correct order (by priority, then queued_at)
        ops.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.queued_at.cmp(&b.queued_at))
        });

        Ok(count)
    }

    // ==================== Replay Support Methods ====================

    /// Get all operations ready for replay.
    ///
    /// Returns operations sorted by priority and queue time.
    /// Does not remove them from the queue.
    pub fn pending_operations(&self) -> Vec<QueuedOperation> {
        self.operations.read().clone()
    }

    /// Mark an operation as attempted.
    ///
    /// Increments attempt count. Returns false if max attempts exceeded
    /// (operation should be removed by caller).
    ///
    /// # Arguments
    ///
    /// * `id` - The operation ID to mark as attempted
    ///
    /// # Returns
    ///
    /// `true` if operation can still be retried, `false` if max attempts
    /// exceeded (caller should remove the operation).
    pub fn mark_attempted(&self, id: Uuid) -> bool {
        let mut ops = self.operations.write();
        if let Some(op) = ops.iter_mut().find(|op| op.id == id) {
            op.attempts += 1;

            // Check if max attempts exceeded
            if let Some(max) = op.max_attempts {
                if op.attempts >= max {
                    return false;
                }
            }
            true
        } else {
            // Operation not found, return false
            false
        }
    }

    /// Get queue statistics.
    ///
    /// Returns statistics about the queue including total operations,
    /// age of oldest operation, and counts by operation type.
    pub fn stats(&self) -> QueueStats {
        let ops = self.operations.read();

        let total_operations = ops.len();

        // Calculate oldest operation age
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let oldest_operation_age = ops.first().map(|op| {
            let age_millis = now_millis.saturating_sub(op.queued_at);
            Duration::from_millis(age_millis)
        });

        // Count operations by type
        let mut operations_by_type: HashMap<String, usize> = HashMap::new();
        for op in ops.iter() {
            *operations_by_type
                .entry(op.operation.type_name().to_string())
                .or_insert(0) += 1;
        }

        QueueStats {
            total_operations,
            oldest_operation_age,
            operations_by_type,
        }
    }

    /// Emit a replay started event.
    ///
    /// Called when beginning to replay queued operations.
    pub fn emit_replay_started(&self, count: usize) {
        let _ = self.events.send(QueueEvent::ReplayStarted { count });
    }

    /// Emit a replay completed event.
    ///
    /// Called when replay finishes with success/failure counts.
    pub fn emit_replay_completed(&self, succeeded: usize, failed: usize) {
        let _ = self
            .events
            .send(QueueEvent::ReplayCompleted { succeeded, failed });
    }
}

impl Default for OfflineQueue {
    fn default() -> Self {
        Self::new(1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Helper to create a test message operation
    fn msg_op(channel: &str, content: &str) -> OperationType {
        OperationType::SendMessage {
            channel_id: channel.to_string(),
            content: content.as_bytes().to_vec(),
        }
    }

    // Helper to create a test reaction operation
    fn reaction_op(msg_id: &str, emoji: &str) -> OperationType {
        OperationType::SendReaction {
            message_id: msg_id.to_string(),
            emoji: emoji.to_string(),
        }
    }

    // ==================== Core Operation Tests ====================

    #[test]
    fn test_enqueue_dequeue() {
        let queue = OfflineQueue::new(100);

        // Enqueue three operations with different priorities to ensure deterministic ordering
        let id1 = queue
            .enqueue_with_priority(msg_op("channel-1", "Hello"), 10)
            .unwrap();
        let id2 = queue
            .enqueue_with_priority(msg_op("channel-1", "World"), 20)
            .unwrap();
        let id3 = queue
            .enqueue_with_priority(msg_op("channel-2", "Test"), 30)
            .unwrap();

        assert_eq!(queue.len(), 3);

        // Dequeue should return in priority order (lower = higher priority)
        let op1 = queue.dequeue().unwrap();
        assert_eq!(op1.id, id1);

        let op2 = queue.dequeue().unwrap();
        assert_eq!(op2.id, id2);

        let op3 = queue.dequeue().unwrap();
        assert_eq!(op3.id, id3);

        assert!(queue.is_empty());
        assert!(queue.dequeue().is_none());
    }

    #[test]
    fn test_priority_ordering() {
        let queue = OfflineQueue::new(100);

        // Enqueue with different priorities (lower = higher priority)
        let low_priority = queue
            .enqueue_with_priority(msg_op("ch", "low"), 200)
            .unwrap();
        let high_priority = queue
            .enqueue_with_priority(msg_op("ch", "high"), 10)
            .unwrap();
        let medium_priority = queue
            .enqueue_with_priority(msg_op("ch", "medium"), 100)
            .unwrap();

        // Should dequeue in priority order
        assert_eq!(queue.dequeue().unwrap().id, high_priority);
        assert_eq!(queue.dequeue().unwrap().id, medium_priority);
        assert_eq!(queue.dequeue().unwrap().id, low_priority);
    }

    #[test]
    fn test_queue_full() {
        let queue = OfflineQueue::new(2); // Small capacity

        queue.enqueue(msg_op("ch", "1")).unwrap();
        queue.enqueue(msg_op("ch", "2")).unwrap();

        // Third should fail
        let result = queue.enqueue(msg_op("ch", "3"));
        assert!(matches!(result, Err(QueueError::QueueFull(2))));

        // After dequeue, should work again
        queue.dequeue();
        assert!(queue.enqueue(msg_op("ch", "3")).is_ok());
    }

    #[test]
    fn test_remove_operation() {
        let queue = OfflineQueue::new(100);

        // Use different priorities to ensure deterministic ordering
        let id1 = queue
            .enqueue_with_priority(msg_op("ch", "1"), 10)
            .unwrap();
        let id2 = queue
            .enqueue_with_priority(msg_op("ch", "2"), 20)
            .unwrap();
        let id3 = queue
            .enqueue_with_priority(msg_op("ch", "3"), 30)
            .unwrap();

        // Remove middle operation
        assert!(queue.remove(id2));
        assert_eq!(queue.len(), 2);

        // Remove non-existent should return false
        assert!(!queue.remove(Uuid::new_v4()));

        // Remaining operations should still be in order
        assert_eq!(queue.dequeue().unwrap().id, id1);
        assert_eq!(queue.dequeue().unwrap().id, id3);
    }

    #[test]
    fn test_clear() {
        let queue = OfflineQueue::new(100);

        queue.enqueue(msg_op("ch", "1")).unwrap();
        queue.enqueue(msg_op("ch", "2")).unwrap();
        queue.enqueue(msg_op("ch", "3")).unwrap();

        assert_eq!(queue.len(), 3);

        queue.clear();

        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn test_peek() {
        let queue = OfflineQueue::new(100);

        assert!(queue.peek().is_none());

        let id = queue.enqueue(msg_op("ch", "hello")).unwrap();

        // Peek should return the operation
        let peeked = queue.peek().unwrap();
        assert_eq!(peeked.id, id);

        // Peek should not remove it
        assert_eq!(queue.len(), 1);
        let peeked_again = queue.peek().unwrap();
        assert_eq!(peeked_again.id, id);
    }

    // ==================== Persistence Tests ====================

    #[tokio::test]
    async fn test_save_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.json");

        let queue = OfflineQueue::new(100);

        // Add some operations with different priorities for deterministic ordering
        let id1 = queue
            .enqueue_with_priority(msg_op("channel-1", "Hello"), 100)
            .unwrap();
        let id2 = queue
            .enqueue_with_priority(reaction_op("msg-1", "thumbsup"), 50)
            .unwrap();
        let id3 = queue
            .enqueue_with_priority(
                OperationType::MarkRead {
                    channel_id: "ch".to_string(),
                    up_to_message_id: "msg-100".to_string(),
                },
                150,
            )
            .unwrap();

        // Save to file
        queue.save(&path).await.unwrap();

        // Create new queue and load
        let new_queue = OfflineQueue::new(100);
        let loaded_count = new_queue.load(&path).await.unwrap();

        assert_eq!(loaded_count, 3);
        assert_eq!(new_queue.len(), 3);

        // Verify priority order is preserved
        // id2 has priority 50 (highest), id1 and id3 have priority 100
        let op = new_queue.dequeue().unwrap();
        assert_eq!(op.id, id2);

        // id1 was queued before id3, so should come first
        let op = new_queue.dequeue().unwrap();
        assert_eq!(op.id, id1);

        let op = new_queue.dequeue().unwrap();
        assert_eq!(op.id, id3);
    }

    #[tokio::test]
    async fn test_load_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");

        let queue = OfflineQueue::new(100);
        let count = queue.load(&path).await.unwrap();

        assert_eq!(count, 0);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn test_save_atomic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let tmp_path = path.with_extension("tmp");

        let queue = OfflineQueue::new(100);
        queue.enqueue(msg_op("ch", "test")).unwrap();

        // Save
        queue.save(&path).await.unwrap();

        // Final file should exist
        assert!(path.exists());

        // Temp file should NOT exist (was renamed)
        assert!(!tmp_path.exists());
    }

    // ==================== Replay Support Tests ====================

    #[test]
    fn test_pending_operations_ordering() {
        let queue = OfflineQueue::new(100);

        queue
            .enqueue_with_priority(msg_op("ch", "low"), 200)
            .unwrap();
        queue
            .enqueue_with_priority(msg_op("ch", "high"), 10)
            .unwrap();
        queue
            .enqueue_with_priority(msg_op("ch", "medium"), 100)
            .unwrap();

        let pending = queue.pending_operations();

        assert_eq!(pending.len(), 3);

        // Should be sorted by priority
        assert_eq!(pending[0].priority, 10);
        assert_eq!(pending[1].priority, 100);
        assert_eq!(pending[2].priority, 200);

        // Should not remove operations
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn test_mark_attempted_increments() {
        let queue = OfflineQueue::new(100);

        let id = queue.enqueue(msg_op("ch", "test")).unwrap();

        // Initial attempts should be 0
        let op = queue.peek().unwrap();
        assert_eq!(op.attempts, 0);

        // Mark as attempted
        assert!(queue.mark_attempted(id));

        let op = queue.peek().unwrap();
        assert_eq!(op.attempts, 1);

        // Mark again
        assert!(queue.mark_attempted(id));

        let op = queue.peek().unwrap();
        assert_eq!(op.attempts, 2);
    }

    #[test]
    fn test_mark_attempted_max_exceeded() {
        let queue = OfflineQueue::new(100);

        // Create operation with max_attempts = 3
        let mut op = QueuedOperation::new(msg_op("ch", "test"));
        op.max_attempts = Some(3);
        let id = queue.enqueue_operation(op).unwrap();

        // First two attempts should return true
        assert!(queue.mark_attempted(id)); // attempts = 1
        assert!(queue.mark_attempted(id)); // attempts = 2

        // Third attempt should return false (max exceeded)
        assert!(!queue.mark_attempted(id)); // attempts = 3 >= max

        // Operation should still be in queue (caller removes it)
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_mark_attempted_unlimited() {
        let queue = OfflineQueue::new(100);

        // Create operation with unlimited attempts
        let mut op = QueuedOperation::new(msg_op("ch", "test"));
        op.max_attempts = None;
        let id = queue.enqueue_operation(op).unwrap();

        // Should always return true
        for _ in 0..100 {
            assert!(queue.mark_attempted(id));
        }
    }

    #[test]
    fn test_mark_attempted_nonexistent() {
        let queue = OfflineQueue::new(100);

        // Should return false for non-existent operation
        assert!(!queue.mark_attempted(Uuid::new_v4()));
    }

    #[test]
    fn test_stats() {
        let queue = OfflineQueue::new(100);

        // Empty stats
        let stats = queue.stats();
        assert_eq!(stats.total_operations, 0);
        assert!(stats.oldest_operation_age.is_none());
        assert!(stats.operations_by_type.is_empty());

        // Add various operations
        queue.enqueue(msg_op("ch", "1")).unwrap();
        queue.enqueue(msg_op("ch", "2")).unwrap();
        queue.enqueue(reaction_op("msg", "thumbsup")).unwrap();
        queue
            .enqueue(OperationType::SendTyping {
                channel_id: "ch".to_string(),
            })
            .unwrap();

        let stats = queue.stats();
        assert_eq!(stats.total_operations, 4);
        assert!(stats.oldest_operation_age.is_some());
        assert_eq!(stats.operations_by_type.get("SendMessage"), Some(&2));
        assert_eq!(stats.operations_by_type.get("SendReaction"), Some(&1));
        assert_eq!(stats.operations_by_type.get("SendTyping"), Some(&1));
    }

    // ==================== Event Tests ====================

    #[tokio::test]
    async fn test_enqueue_emits_added_event() {
        let queue = OfflineQueue::new(100);
        let mut rx = queue.subscribe();

        let id = queue.enqueue(msg_op("ch", "test")).unwrap();

        let event = rx.recv().await.unwrap();
        match event {
            QueueEvent::Added(event_id) => assert_eq!(event_id, id),
            _ => panic!("Expected Added event"),
        }
    }

    #[tokio::test]
    async fn test_dequeue_emits_removed_event() {
        let queue = OfflineQueue::new(100);
        let id = queue.enqueue(msg_op("ch", "test")).unwrap();

        let mut rx = queue.subscribe();

        queue.dequeue();

        let event = rx.recv().await.unwrap();
        match event {
            QueueEvent::Removed(event_id) => assert_eq!(event_id, id),
            _ => panic!("Expected Removed event"),
        }
    }

    #[tokio::test]
    async fn test_remove_emits_removed_event() {
        let queue = OfflineQueue::new(100);
        let id = queue.enqueue(msg_op("ch", "test")).unwrap();

        let mut rx = queue.subscribe();

        queue.remove(id);

        let event = rx.recv().await.unwrap();
        match event {
            QueueEvent::Removed(event_id) => assert_eq!(event_id, id),
            _ => panic!("Expected Removed event"),
        }
    }

    #[tokio::test]
    async fn test_clear_emits_cleared_event() {
        let queue = OfflineQueue::new(100);
        queue.enqueue(msg_op("ch", "1")).unwrap();
        queue.enqueue(msg_op("ch", "2")).unwrap();

        let mut rx = queue.subscribe();

        queue.clear();

        let event = rx.recv().await.unwrap();
        assert!(matches!(event, QueueEvent::Cleared));
    }

    #[tokio::test]
    async fn test_replay_events() {
        let queue = OfflineQueue::new(100);
        let mut rx = queue.subscribe();

        queue.emit_replay_started(5);
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, QueueEvent::ReplayStarted { count: 5 }));

        queue.emit_replay_completed(3, 2);
        let event = rx.recv().await.unwrap();
        assert!(matches!(
            event,
            QueueEvent::ReplayCompleted {
                succeeded: 3,
                failed: 2
            }
        ));
    }

    // ==================== Edge Case Tests ====================

    #[test]
    fn test_operation_type_names() {
        assert_eq!(msg_op("ch", "test").type_name(), "SendMessage");
        assert_eq!(reaction_op("msg", "emoji").type_name(), "SendReaction");
        assert_eq!(
            OperationType::MarkRead {
                channel_id: "ch".to_string(),
                up_to_message_id: "msg".to_string(),
            }
            .type_name(),
            "MarkRead"
        );
        assert_eq!(
            OperationType::SendTyping {
                channel_id: "ch".to_string(),
            }
            .type_name(),
            "SendTyping"
        );
        assert_eq!(
            OperationType::EditMessage {
                message_id: "msg".to_string(),
                content: vec![],
            }
            .type_name(),
            "EditMessage"
        );
        assert_eq!(
            OperationType::DeleteMessage {
                message_id: "msg".to_string(),
            }
            .type_name(),
            "DeleteMessage"
        );
    }

    #[test]
    fn test_queued_operation_builders() {
        let op = QueuedOperation::new(msg_op("ch", "test"));
        assert_eq!(op.priority, 100);
        assert_eq!(op.attempts, 0);
        assert_eq!(op.max_attempts, Some(5));

        let op = QueuedOperation::with_priority(msg_op("ch", "test"), 50);
        assert_eq!(op.priority, 50);

        let op = QueuedOperation::with_max_attempts(msg_op("ch", "test"), None);
        assert_eq!(op.max_attempts, None);
    }

    #[test]
    fn test_default_queue() {
        let queue = OfflineQueue::default();
        assert_eq!(queue.max_size(), 1000);
    }

    #[test]
    fn test_with_limits() {
        let queue = OfflineQueue::with_limits(50, 1024);
        assert_eq!(queue.max_size(), 50);

        // Test that operation size limit is enforced
        let large_content = vec![0u8; 2000]; // Larger than 1024 limit when serialized
        let result = queue.enqueue(OperationType::SendMessage {
            channel_id: "ch".to_string(),
            content: large_content,
        });

        // The operation might be close to the limit, let's use a clearly oversized one
        let huge_content = vec![0u8; 10000];
        let queue2 = OfflineQueue::with_limits(50, 100); // Very small limit
        let result2 = queue2.enqueue(OperationType::SendMessage {
            channel_id: "ch".to_string(),
            content: huge_content,
        });
        assert!(matches!(result2, Err(QueueError::OperationTooLarge { .. })));
    }
}
