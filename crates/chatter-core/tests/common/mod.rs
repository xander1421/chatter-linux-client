//! Common test utilities for integration tests.
//!
//! Provides shared utilities, fixtures, and assertions for testing
//! gRPC clients and services against a running server.

// Integration tests are external to `chatter-core`, so every service accessor
// they reach for (`client.signal()`, `client.crypto()`, …) is `pub(crate)`
// unless the `testing` feature widens it — see `service_accessor!` in
// `src/lib.rs`. Without the feature the failure surfaces as a pile of
// "method is private" and type-inference errors far from the cause, which is
// exactly what happened when the documented run command omitted it. Say so
// instead.
#[cfg(not(feature = "testing"))]
compile_error!(
    "chatter-core integration tests require the `testing` feature, which makes the \
     service accessors public outside this crate. Run them with, for example:\n\n    \
     INTEGRATION_TESTS=1 cargo test -p chatter-core --features testing --test validated \
     -- --nocapture --test-threads=1"
);

pub mod assertions;
pub mod config;
pub mod fixtures;

// Re-export commonly used items
pub use assertions::*;
pub use config::*;
pub use fixtures::*;

