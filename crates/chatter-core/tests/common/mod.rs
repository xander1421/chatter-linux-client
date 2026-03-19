//! Common test utilities for integration tests.
//!
//! Provides shared utilities, fixtures, and assertions for testing
//! gRPC clients and services against a running server.

pub mod assertions;
pub mod config;
pub mod fixtures;

// Re-export commonly used items
pub use assertions::*;
pub use config::*;
pub use fixtures::*;

