//! Network communication layer.
//!
//! All network communication including gRPC clients and real-time streaming.

pub mod connection;
pub mod connection_manager;
pub mod grpc;
pub mod offline_queue;
pub mod recovery;
pub mod retry;
pub mod streaming;
pub mod validation;

pub use connection::{ConnectionEvent, ConnectionState, ConnectionStateMachine, FailReason};
pub use connection_manager::{ConnectionConfig, ConnectionManager, ReplayResult};
pub use offline_queue::{OfflineQueue, OperationType, QueueError, QueueEvent, QueuedOperation, QueueStats};
pub use recovery::{ConnectionRecovery, FibonacciBackoff};
pub use retry::{RetryExecutor, RetryPolicy};
