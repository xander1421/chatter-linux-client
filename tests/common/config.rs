//! Test configuration and skip macros.
//!
//! Provides server configuration and macros for conditionally
//! skipping tests when the server is not available.

use std::time::Duration;

/// Default server URL for integration tests.
pub const SERVER_URL: &str = "http://localhost:50051";

/// Connection timeout for tests.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Request timeout for tests.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Check if integration tests should be skipped.
///
/// Tests are skipped unless `INTEGRATION_TESTS=1` environment variable is set.
pub fn should_skip_integration_tests() -> bool {
    std::env::var("INTEGRATION_TESTS")
        .map(|v| v != "1")
        .unwrap_or(true)
}

/// Macro to skip tests when integration tests are not enabled.
///
/// Call this at the beginning of each integration test:
///
/// ```ignore
/// #[tokio::test]
/// async fn test_something() {
///     skip_if_no_server!();
///     // ... rest of test
/// }
/// ```
#[macro_export]
macro_rules! skip_if_no_server {
    () => {
        if $crate::common::should_skip_integration_tests() {
            eprintln!("Skipping integration test: INTEGRATION_TESTS=1 not set");
            return;
        }
    };
}

/// Get server URL from environment or use default.
pub fn server_url() -> String {
    std::env::var("CHATTER_SERVER_URL").unwrap_or_else(|_| SERVER_URL.to_string())
}
