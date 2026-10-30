//! Test configuration and skip macros.
//!
//! Provides server configuration and macros for conditionally
//! skipping tests when the server is not available.

/// Default server URL for integration tests.
/// TLS became mandatory in `4bc9aa51`: the server will not start without
/// `certs/server.crt` and answers a plaintext h2c connection by closing it
/// (`tls: first record does not look like a TLS handshake`). This constant kept
/// saying `http://`, so every proof built on `create_test_pool` — as opposed to
/// the ChatterClient builder in validated.rs, which says `https://` — died with
/// `Unavailable` before sending a byte.
///
/// That is OPEN-ITEMS §73: "client-side and pre-flight, Unavailable in 0.00s,
/// server logs nothing". Six hypotheses were eliminated against it (concurrency
/// level, tokio runtime flavour, server rejection, socket exhaustion, the
/// non-streaming pool, registration count) and it looked load-sensitive because
/// the affected proofs happen to be the ones that run late. The real error —
/// `hyper::Error(Io, BrokenPipe)` under a `tonic::transport::Error` — was
/// discarded by `NetworkError::from_status`, which maps Code::Unavailable to a
/// unit variant, and no subscriber was installed on this path anyway. Both are
/// fixed alongside this.
pub const SERVER_URL: &str = "https://localhost:50051";

/// Check if integration tests should be skipped.
///
/// Tests are skipped unless `INTEGRATION_TESTS=1` environment variable is set.
pub fn should_skip_integration_tests() -> bool {
    std::env::var("INTEGRATION_TESTS")
        .map(|v| v != "1")
        .unwrap_or(true)
}

/// Whether a skipped integration test should be a hard failure.
///
/// Set `CHATTER_REQUIRE_SERVER=1` when the run is supposed to prove something.
/// See [`skip_if_no_server`] for why that matters.
pub fn require_server() -> bool {
    std::env::var("CHATTER_REQUIRE_SERVER")
        .map(|v| v == "1")
        .unwrap_or(false)
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
///
/// # Read this before citing a green run as evidence
///
/// libtest has no runtime "skipped" state, so the early `return` below is
/// reported by cargo as **PASSED**. With `INTEGRATION_TESTS` unset — the
/// default — a full run of this suite prints `test result: ok` while having
/// asserted nothing. 275 call sites across 33 files behave this way. A green
/// integration run is therefore not evidence unless `INTEGRATION_TESTS=1` was
/// set, and there is no way to tell after the fact from cargo's output.
///
/// Two things make that harder to get wrong:
///
/// - `CHATTER_REQUIRE_SERVER=1` turns the skip into a panic, so a run meant to
///   prove something fails loudly instead of passing vacuously. Use it whenever
///   a green result will be reported as verification. Measured on this suite
///   with no server: `51 passed` becomes `4 passed; 47 failed` — that is how
///   much of a green run was real.
/// - the skip line names the test and says it asserted nothing. Note cargo
///   captures output for passing tests, so this is only visible under
///   `--nocapture`; the env var above is the reliable check.
#[macro_export]
macro_rules! skip_if_no_server {
    () => {
        if $crate::common::should_skip_integration_tests() {
            let test = std::thread::current()
                .name()
                .unwrap_or("<unknown>")
                .to_string();
            if $crate::common::require_server() {
                panic!(
                    "{test}: CHATTER_REQUIRE_SERVER=1 but INTEGRATION_TESTS is not 1.\n\
                     This test asserts nothing without a server, and a vacuous pass \
                     was explicitly disallowed for this run.\n\
                     Start the stack and re-run with INTEGRATION_TESTS=1."
                );
            }
            eprintln!(
                "SKIP (reported by cargo as PASSED, asserts nothing): {test} \
                 — INTEGRATION_TESTS=1 not set"
            );
            return;
        }
    };
}

/// Get server URL from environment or use default.
pub fn server_url() -> String {
    std::env::var("CHATTER_SERVER_URL").unwrap_or_else(|_| SERVER_URL.to_string())
}
