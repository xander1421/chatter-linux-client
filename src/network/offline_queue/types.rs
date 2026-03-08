//! Offline queue types.
//!
//! Types for queued operations, events, and errors.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
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
