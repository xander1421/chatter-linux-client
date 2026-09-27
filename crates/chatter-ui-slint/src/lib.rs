//! Generated UI shared by the app and its tests.
//!
//! Keeping this in a dependency lets Rust-only application edits and the
//! application's cfg(test) build reuse the same compiled component tree.

slint::include_modules!();

/// Whether this component tree contains metadata for ElementHandle queries.
pub const SLINT_DEBUG_INFO_ENABLED: bool = cfg!(feature = "ui-test-debug");
