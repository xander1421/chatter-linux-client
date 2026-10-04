//! Network communication layer.
//!
//! All network communication including gRPC clients and real-time streaming.

pub mod anon_http;
pub mod connection;
pub mod grpc;
pub mod reconnect_config;
pub mod server_monitor;
pub(crate) mod storage_http;
pub mod streaming;
pub mod validation;

pub use connection::{server_status, ConnectionState, FailReason, ServerStatus};
pub use reconnect_config::ReconnectConfig;
