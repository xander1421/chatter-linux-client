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

/// Keep a test client off the OS keyring. Without the opt-out marker beside
/// its database, every client a test builds asks Secret Service for a fresh
/// passphrase entry — a desktop prompt on a developer machine, a hang in CI.
pub fn headless(base_dir: &std::path::Path) {
    let db_path = chatter_core::system::Directories::with_base_dir(base_dir.to_path_buf())
        .expect("create test directories")
        .database_path();
    let db_dir = db_path.parent().expect("database path has a parent");
    std::fs::write(
        db_dir.join(chatter_core::system::keyring::NO_KEYRING_MARKER),
        b"",
    )
    .expect("write no-keyring marker");
}

pub mod fixtures;
pub mod groups;
pub mod membership;

// Re-export commonly used items
pub use assertions::*;
pub use config::*;
pub use fixtures::*;
#[allow(unused_imports)] // used only by tests/validated.rs
pub use groups::*;
