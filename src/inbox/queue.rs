//! Local queue for retry/ordering.
//!
//! Queue for messages that fail processing, with exponential backoff retry.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::proto::mailbox_v1::EphemeralMessage;

// ============================================================================
// Constants
// ============================================================================

/// Default maximum queue size.
const DEFAULT_MAX_SIZE: usize = 1000;

/// Default maximum retry attempts.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Default base retry delay.
const DEFAULT_BASE_DELAY: Duration = Duration::from_secs(5);

/// Maximum retry delay (cap for exponential backoff).
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300); // 5 minutes

// ============================================================================
// Queued Message
// ============================================================================

/// A message queued for retry.
#[derive(Debug, Clone)]
pub struct QueuedInboxMessage {
    /// The original ephemeral message.
    pub message: EphemeralMessage,
    /// Number of processing attempts.
    pub attempts: u32,
    /// When the message was last attempted.
    pub last_attempt: Instant,
    /// When the message should next be attempted.
    pub next_attempt: Instant,
    /// Last error message (if any).
    pub error: Option<String>,
}

impl QueuedInboxMessage {
    /// Create a new queued message.
    pub fn new(message: EphemeralMessage, error: Option<String>) -> Self {
        let now = Instant::now();
        Self {
            message,
            attempts: 0,
            last_attempt: now,
            next_attempt: now, // Ready for immediate retry
            error,
        }
    }

    /// Get the message ID.
    pub fn message_id(&self) -> &str {
        &self.message.id
    }

    /// Check if the message is ready for retry.
    pub fn is_ready(&self) -> bool {
        Instant::now() >= self.next_attempt
    }

    /// Calculate time until next retry.
    pub fn time_until_ready(&self) -> Duration {
        let now = Instant::now();
        if now >= self.next_attempt {
            Duration::ZERO
        } else {
            self.next_attempt - now
        }
    }

    /// Update after a failed attempt with exponential backoff.
    pub fn mark_failed(&mut self, error: String, base_delay: Duration) {
        self.attempts += 1;
        self.last_attempt = Instant::now();
        self.error = Some(error);

        // Exponential backoff: base_delay * 2^attempts, capped at MAX_RETRY_DELAY
        let delay_multiplier = 2u32.saturating_pow(self.attempts);
        let delay = base_delay
            .saturating_mul(delay_multiplier)
            .min(MAX_RETRY_DELAY);

        self.next_attempt = self.last_attempt + delay;
    }
}

// ============================================================================
// Persisted Queue Entry (for serialization)
// ============================================================================

/// Serializable version of QueuedInboxMessage for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedQueueEntry {
    /// Message ID.
    message_id: String,
    /// Encrypted envelope bytes.
    encrypted_envelope: Vec<u8>,
    /// Envelope type (as i32).
    envelope_type: i32,
    /// Server timestamp.
    timestamp: u64,
    /// Number of attempts.
    attempts: u32,
    /// Seconds until next attempt (relative).
    next_attempt_secs: u64,
    /// Last error message.
    error: Option<String>,
}

impl PersistedQueueEntry {
    fn from_queued(queued: &QueuedInboxMessage) -> Self {
        let now = Instant::now();
        let next_attempt_secs = if queued.next_attempt > now {
            (queued.next_attempt - now).as_secs()
        } else {
            0
        };

        Self {
            message_id: queued.message.id.clone(),
            encrypted_envelope: queued.message.encrypted_envelope.clone(),
            envelope_type: queued.message.envelope_type,
            timestamp: queued.message.timestamp,
            attempts: queued.attempts,
            next_attempt_secs,
            error: queued.error.clone(),
        }
    }

    fn to_queued(&self) -> QueuedInboxMessage {
        let now = Instant::now();
        let message = EphemeralMessage {
            id: self.message_id.clone(),
            encrypted_envelope: self.encrypted_envelope.clone(),
            envelope_type: self.envelope_type,
            timestamp: self.timestamp,
        };

        QueuedInboxMessage {
            message,
            attempts: self.attempts,
            last_attempt: now, // We don't persist exact timing, so use now
            next_attempt: now + Duration::from_secs(self.next_attempt_secs),
            error: self.error.clone(),
        }
    }
}

// ============================================================================
// Local Queue
// ============================================================================

/// Local queue for messages that fail processing.
///
/// Provides retry functionality with exponential backoff for messages
/// that encounter transient errors during processing.
///
/// # Example
///
/// ```ignore
/// let queue = LocalQueue::new(1000, 3, Duration::from_secs(5), None);
///
/// // Enqueue a failed message
/// queue.enqueue(message, Some("decryption failed".to_string()));
///
/// // Check for ready messages
/// while let Some(queued) = queue.dequeue_ready() {
///     match process(&queued.message).await {
///         Ok(_) => queue.remove(&queued.message.id),
///         Err(e) => queue.mark_failed(&queued.message.id, e.to_string()),
///     }
/// }
/// ```
pub struct LocalQueue {
    /// Pending messages.
    messages: VecDeque<QueuedInboxMessage>,
    /// Maximum queue size.
    max_size: usize,
    /// Maximum retry attempts before dropping.
    max_attempts: u32,
    /// Base delay for exponential backoff.
    base_delay: Duration,
    /// Optional persistence path.
    persistence_path: Option<std::path::PathBuf>,
}

impl LocalQueue {
    /// Create a new local queue.
    ///
    /// # Arguments
    ///
    /// * `max_size` - Maximum number of messages in the queue
    /// * `max_attempts` - Maximum retry attempts before dropping
    /// * `base_delay` - Base delay for exponential backoff
    /// * `persistence_path` - Optional path for crash recovery persistence
    pub fn new(
        max_size: usize,
        max_attempts: u32,
        base_delay: Duration,
        persistence_path: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            messages: VecDeque::with_capacity(max_size.min(100)),
            max_size,
            max_attempts,
            base_delay,
            persistence_path,
        }
    }

    /// Create a queue with default settings.
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_MAX_SIZE, DEFAULT_MAX_ATTEMPTS, DEFAULT_BASE_DELAY, None)
    }

    /// Enqueue a message for retry.
    ///
    /// Returns `true` if the message was added, `false` if the queue is full.
    pub fn enqueue(&mut self, message: EphemeralMessage, error: Option<String>) -> bool {
        // Check if already queued (by message ID)
        if self.messages.iter().any(|m| m.message.id == message.id) {
            tracing::debug!(message_id = %message.id, "Message already in retry queue");
            return true; // Already queued, consider it success
        }

        // Check capacity
        if self.messages.len() >= self.max_size {
            tracing::warn!(
                max_size = self.max_size,
                "Retry queue full, dropping oldest message"
            );
            // Drop oldest message to make room
            self.messages.pop_front();
        }

        let queued = QueuedInboxMessage::new(message, error);
        tracing::debug!(
            message_id = %queued.message_id(),
            "Enqueued message for retry"
        );
        self.messages.push_back(queued);
        true
    }

    /// Dequeue the next message that is ready for retry.
    ///
    /// Returns `None` if no messages are ready.
    pub fn dequeue_ready(&mut self) -> Option<QueuedInboxMessage> {
        // Find first ready message
        let ready_idx = self.messages.iter().position(|m| m.is_ready())?;

        self.messages.remove(ready_idx)
    }

    /// Peek at the next ready message without removing it.
    pub fn peek_ready(&self) -> Option<&QueuedInboxMessage> {
        self.messages.iter().find(|m| m.is_ready())
    }

    /// Remove a message by ID (after successful processing).
    ///
    /// Returns `true` if the message was found and removed.
    pub fn remove(&mut self, message_id: &str) -> bool {
        if let Some(pos) = self.messages.iter().position(|m| m.message.id == message_id) {
            self.messages.remove(pos);
            tracing::debug!(message_id = %message_id, "Removed message from retry queue");
            true
        } else {
            false
        }
    }

    /// Mark a message as failed and update retry timing.
    ///
    /// Returns `false` if max attempts exceeded (message should be dropped).
    pub fn mark_failed(&mut self, message_id: &str, error: String) -> bool {
        if let Some(queued) = self.messages.iter_mut().find(|m| m.message.id == message_id) {
            queued.mark_failed(error, self.base_delay);

            // Check if max attempts exceeded
            if queued.attempts >= self.max_attempts {
                tracing::warn!(
                    message_id = %message_id,
                    attempts = queued.attempts,
                    "Message exceeded max retry attempts, will be dropped"
                );
                return false;
            }

            tracing::debug!(
                message_id = %message_id,
                attempts = queued.attempts,
                next_retry_secs = queued.time_until_ready().as_secs(),
                "Message retry scheduled"
            );
            true
        } else {
            // Message not in queue - may have been removed
            false
        }
    }

    /// Get the number of messages in the queue.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Clear all messages from the queue.
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Get the number of messages ready for retry.
    pub fn ready_count(&self) -> usize {
        self.messages.iter().filter(|m| m.is_ready()).count()
    }

    /// Get time until the next message is ready (if any).
    pub fn time_until_next_ready(&self) -> Option<Duration> {
        self.messages
            .iter()
            .map(|m| m.time_until_ready())
            .min()
    }

    // ==================== Persistence Methods ====================

    /// Save queue to persistent storage.
    ///
    /// Uses atomic .tmp rename pattern for safety.
    pub async fn save(&self) -> std::io::Result<()> {
        let path = match &self.persistence_path {
            Some(p) => p,
            None => return Ok(()), // No persistence configured
        };

        // Convert to persisted entries
        let entries: Vec<PersistedQueueEntry> = self
            .messages
            .iter()
            .map(PersistedQueueEntry::from_queued)
            .collect();

        // Serialize to JSON
        let json = serde_json::to_string_pretty(&entries)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        // Write to temporary file first
        let tmp_path = path.with_extension("tmp");
        tokio::fs::write(&tmp_path, json).await?;

        // Atomic rename
        tokio::fs::rename(&tmp_path, path).await?;

        tracing::debug!(
            path = %path.display(),
            count = entries.len(),
            "Saved retry queue to disk"
        );

        Ok(())
    }

    /// Load queue from persistent storage.
    ///
    /// Returns the number of messages loaded.
    pub async fn load(&mut self) -> std::io::Result<usize> {
        let path = match &self.persistence_path {
            Some(p) => p,
            None => return Ok(0), // No persistence configured
        };

        // Handle missing file gracefully
        if !path.exists() {
            return Ok(0);
        }

        let json = tokio::fs::read_to_string(path).await?;

        let entries: Vec<PersistedQueueEntry> = serde_json::from_str(&json)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let count = entries.len();

        // Convert to queued messages
        self.messages = entries.into_iter().map(|e| e.to_queued()).collect();

        tracing::debug!(
            path = %path.display(),
            count = count,
            "Loaded retry queue from disk"
        );

        Ok(count)
    }

    /// Load from a specific path (for testing or explicit loading).
    pub async fn load_from(&mut self, path: &Path) -> std::io::Result<usize> {
        if !path.exists() {
            return Ok(0);
        }

        let json = tokio::fs::read_to_string(path).await?;

        let entries: Vec<PersistedQueueEntry> = serde_json::from_str(&json)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let count = entries.len();
        self.messages = entries.into_iter().map(|e| e.to_queued()).collect();

        Ok(count)
    }

    /// Save to a specific path (for testing or explicit saving).
    pub async fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let entries: Vec<PersistedQueueEntry> = self
            .messages
            .iter()
            .map(PersistedQueueEntry::from_queued)
            .collect();

        let json = serde_json::to_string_pretty(&entries)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let tmp_path = path.with_extension("tmp");
        tokio::fs::write(&tmp_path, json).await?;
        tokio::fs::rename(&tmp_path, path).await?;

        Ok(())
    }
}

impl Default for LocalQueue {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_message(id: &str) -> EphemeralMessage {
        EphemeralMessage {
            id: id.to_string(),
            encrypted_envelope: vec![1, 2, 3],
            envelope_type: 1,
            timestamp: 1234567890,
        }
    }

    #[test]
    fn test_queue_enqueue_dequeue() {
        let mut queue = LocalQueue::with_defaults();

        assert!(queue.is_empty());

        queue.enqueue(make_message("msg-1"), None);
        queue.enqueue(make_message("msg-2"), Some("error".to_string()));

        assert_eq!(queue.len(), 2);

        let first = queue.dequeue_ready().unwrap();
        assert_eq!(first.message_id(), "msg-1");
        assert_eq!(first.attempts, 0);
        assert!(first.error.is_none());

        let second = queue.dequeue_ready().unwrap();
        assert_eq!(second.message_id(), "msg-2");
        assert_eq!(second.error, Some("error".to_string()));

        assert!(queue.is_empty());
    }

    #[test]
    fn test_queue_remove() {
        let mut queue = LocalQueue::with_defaults();

        queue.enqueue(make_message("msg-1"), None);
        queue.enqueue(make_message("msg-2"), None);
        queue.enqueue(make_message("msg-3"), None);

        assert!(queue.remove("msg-2"));
        assert_eq!(queue.len(), 2);

        // Verify msg-2 is gone
        assert!(!queue.remove("msg-2"));

        // Other messages still there
        let first = queue.dequeue_ready().unwrap();
        assert_eq!(first.message_id(), "msg-1");
    }

    #[test]
    fn test_queue_mark_failed() {
        let mut queue = LocalQueue::new(100, 3, Duration::from_millis(100), None);

        queue.enqueue(make_message("msg-1"), None);

        // First failure - should return true (can retry)
        assert!(queue.mark_failed("msg-1", "error 1".to_string()));

        // Message should not be ready immediately due to backoff
        let queued = queue.messages.front().unwrap();
        assert_eq!(queued.attempts, 1);
        assert!(!queued.is_ready());

        // Second failure
        // Need to re-fetch since we can't hold reference across mutation
        queue.messages.front_mut().unwrap().next_attempt = Instant::now();
        assert!(queue.mark_failed("msg-1", "error 2".to_string()));
        assert_eq!(queue.messages.front().unwrap().attempts, 2);

        // Third failure - should return false (max attempts)
        queue.messages.front_mut().unwrap().next_attempt = Instant::now();
        assert!(!queue.mark_failed("msg-1", "error 3".to_string()));
    }

    #[test]
    fn test_queue_exponential_backoff() {
        let base_delay = Duration::from_secs(5);
        let mut queued = QueuedInboxMessage::new(make_message("msg-1"), None);

        // First failure: 5s * 2^1 = 10s
        queued.mark_failed("error".to_string(), base_delay);
        assert_eq!(queued.attempts, 1);
        let delay1 = queued.time_until_ready();
        assert!(delay1 >= Duration::from_secs(9));
        assert!(delay1 <= Duration::from_secs(11));

        // Force next_attempt to now for testing
        queued.next_attempt = Instant::now();

        // Second failure: 5s * 2^2 = 20s
        queued.mark_failed("error".to_string(), base_delay);
        assert_eq!(queued.attempts, 2);
        let delay2 = queued.time_until_ready();
        assert!(delay2 >= Duration::from_secs(19));
        assert!(delay2 <= Duration::from_secs(21));
    }

    #[test]
    fn test_queue_backoff_cap() {
        let base_delay = Duration::from_secs(60);
        let mut queued = QueuedInboxMessage::new(make_message("msg-1"), None);

        // Many failures should cap at MAX_RETRY_DELAY (5 minutes)
        for _ in 0..10 {
            queued.next_attempt = Instant::now();
            queued.mark_failed("error".to_string(), base_delay);
        }

        let delay = queued.time_until_ready();
        assert!(delay <= MAX_RETRY_DELAY + Duration::from_secs(1));
    }

    #[test]
    fn test_queue_duplicate_prevention() {
        let mut queue = LocalQueue::with_defaults();

        queue.enqueue(make_message("msg-1"), None);
        queue.enqueue(make_message("msg-1"), None); // Duplicate

        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_queue_max_size() {
        let mut queue = LocalQueue::new(3, 3, Duration::from_secs(5), None);

        queue.enqueue(make_message("msg-1"), None);
        queue.enqueue(make_message("msg-2"), None);
        queue.enqueue(make_message("msg-3"), None);
        assert_eq!(queue.len(), 3);

        // Fourth message should evict oldest
        queue.enqueue(make_message("msg-4"), None);
        assert_eq!(queue.len(), 3);

        // msg-1 should be gone
        assert!(!queue.messages.iter().any(|m| m.message.id == "msg-1"));
    }

    #[test]
    fn test_queue_ready_count() {
        let mut queue = LocalQueue::new(100, 3, Duration::from_millis(100), None);

        queue.enqueue(make_message("msg-1"), None);
        queue.enqueue(make_message("msg-2"), None);

        assert_eq!(queue.ready_count(), 2);

        // Mark one as failed (not ready due to backoff)
        queue.mark_failed("msg-1", "error".to_string());

        assert_eq!(queue.ready_count(), 1);
    }

    #[test]
    fn test_queue_clear() {
        let mut queue = LocalQueue::with_defaults();

        queue.enqueue(make_message("msg-1"), None);
        queue.enqueue(make_message("msg-2"), None);

        queue.clear();

        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
    }

    #[tokio::test]
    async fn test_queue_persistence_save_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.json");

        let mut queue = LocalQueue::new(100, 3, Duration::from_secs(5), Some(path.clone()));

        queue.enqueue(make_message("msg-1"), None);
        queue.enqueue(make_message("msg-2"), Some("error".to_string()));

        // Save
        queue.save().await.unwrap();
        assert!(path.exists());

        // Load into new queue
        let mut queue2 = LocalQueue::new(100, 3, Duration::from_secs(5), Some(path.clone()));
        let loaded = queue2.load().await.unwrap();

        assert_eq!(loaded, 2);
        assert_eq!(queue2.len(), 2);

        // Verify messages
        let first = queue2.dequeue_ready().unwrap();
        assert_eq!(first.message_id(), "msg-1");

        let second = queue2.dequeue_ready().unwrap();
        assert_eq!(second.message_id(), "msg-2");
        assert_eq!(second.error, Some("error".to_string()));
    }

    #[tokio::test]
    async fn test_queue_load_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");

        let mut queue = LocalQueue::new(100, 3, Duration::from_secs(5), Some(path));
        let loaded = queue.load().await.unwrap();

        assert_eq!(loaded, 0);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn test_queue_atomic_save() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("queue.json");
        let tmp_path = path.with_extension("tmp");

        let mut queue = LocalQueue::with_defaults();
        queue.enqueue(make_message("msg-1"), None);

        queue.save_to(&path).await.unwrap();

        // Final file should exist
        assert!(path.exists());

        // Temp file should NOT exist (was renamed)
        assert!(!tmp_path.exists());
    }
}
