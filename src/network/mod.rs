//! Network communication layer.
//!
//! All network communication including gRPC clients and real-time streaming.

pub mod grpc;
pub mod streaming;
pub mod validation;
pub mod connection;
pub mod recovery;
pub mod retry;
pub mod offline_queue;
