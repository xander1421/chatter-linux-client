//! Test macros for common patterns.
//!
//! Provides macros for:
//! - Skipping tests based on conditions
//! - Asserting results and errors
//! - Running async tests with timeouts
//! - Parameterized tests

// Note: skip_if_no_server! is defined in common/config.rs and re-exported

/// Skip test with a custom reason.
///
/// # Example
///
/// ```rust
/// #[test]
/// fn test_platform_specific() {
///     skip_test!("Only runs on Linux");
/// }
/// ```
#[macro_export]
macro_rules! skip_test {
    ($reason:expr) => {
        eprintln!("Skipping: {}", $reason);
        return;
    };
}

/// Assert that a Result is Ok, returning the value.
///
/// Provides better error messages than `.unwrap()`.
///
/// # Example
///
/// ```rust
/// let value = assert_ok!(some_result);
/// let value = assert_ok!(some_result, "Custom error context");
/// ```
#[macro_export]
macro_rules! assert_ok {
    ($expr:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => panic!(
                "assertion failed: expected Ok, got Err\n  expression: {}\n  error: {:?}",
                stringify!($expr),
                e
            ),
        }
    };
    ($expr:expr, $msg:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => panic!(
                "assertion failed: {}\n  expression: {}\n  error: {:?}",
                $msg,
                stringify!($expr),
                e
            ),
        }
    };
}

/// Assert that a Result is Err with a specific error pattern.
///
/// # Example
///
/// ```rust
/// assert_err!(result, NetworkError::NotFound(_));
/// assert_err!(result, NetworkError::InvalidRequest(msg) if msg.contains("invalid"));
/// ```
#[macro_export]
macro_rules! assert_err {
    ($expr:expr, $pattern:pat) => {
        match $expr {
            Err($pattern) => {}
            Err(e) => panic!(
                "assertion failed: expected {}, got {:?}\n  expression: {}",
                stringify!($pattern),
                e,
                stringify!($expr)
            ),
            Ok(v) => panic!(
                "assertion failed: expected Err({}), got Ok({:?})\n  expression: {}",
                stringify!($pattern),
                v,
                stringify!($expr)
            ),
        }
    };
    ($expr:expr, $pattern:pat if $guard:expr) => {
        match $expr {
            Err($pattern) if $guard => {}
            Err(e) => panic!(
                "assertion failed: expected {} if {}, got {:?}\n  expression: {}",
                stringify!($pattern),
                stringify!($guard),
                e,
                stringify!($expr)
            ),
            Ok(v) => panic!(
                "assertion failed: expected Err, got Ok({:?})\n  expression: {}",
                v,
                stringify!($expr)
            ),
        }
    };
}

/// Assert that an Option is Some, returning the value.
///
/// # Example
///
/// ```rust
/// let value = assert_some!(optional_value);
/// ```
#[macro_export]
macro_rules! assert_some {
    ($expr:expr) => {
        match $expr {
            Some(v) => v,
            None => panic!(
                "assertion failed: expected Some, got None\n  expression: {}",
                stringify!($expr)
            ),
        }
    };
    ($expr:expr, $msg:expr) => {
        match $expr {
            Some(v) => v,
            None => panic!(
                "assertion failed: {}\n  expression: {}",
                $msg,
                stringify!($expr)
            ),
        }
    };
}

/// Assert that an Option is None.
///
/// # Example
///
/// ```rust
/// assert_none!(optional_value);
/// ```
#[macro_export]
macro_rules! assert_none {
    ($expr:expr) => {
        match $expr {
            None => {}
            Some(v) => panic!(
                "assertion failed: expected None, got Some({:?})\n  expression: {}",
                v,
                stringify!($expr)
            ),
        }
    };
}

/// Run an async test with a timeout.
///
/// # Example
///
/// ```rust
/// async_test!(test_name, Duration::from_secs(10), {
///     // test body
/// });
/// ```
#[macro_export]
macro_rules! async_test {
    ($name:ident, $timeout:expr, $body:block) => {
        #[tokio::test]
        async fn $name() {
            tokio::time::timeout($timeout, async $body)
                .await
                .expect(concat!(
                    "Test '",
                    stringify!($name),
                    "' timed out"
                ))
        }
    };
}

/// Run an async test that requires server with timeout.
///
/// # Example
///
/// ```rust
/// integration_test!(test_name, Duration::from_secs(30), {
///     let pool = create_test_pool().await?;
///     // ...
/// });
/// ```
#[macro_export]
macro_rules! integration_test {
    ($name:ident, $timeout:expr, $body:block) => {
        #[tokio::test]
        async fn $name() {
            skip_if_no_server!();
            tokio::time::timeout($timeout, async $body)
                .await
                .expect(concat!(
                    "Integration test '",
                    stringify!($name),
                    "' timed out"
                ))
        }
    };
}

/// Create parameterized tests.
///
/// # Example
///
/// ```rust
/// test_cases! {
///     test_channel_types,
///     [
///         (ChannelType::Public, "public"),
///         (ChannelType::Private, "private"),
///     ],
///     |(channel_type, name)| {
///         assert!(name.len() > 0);
///     }
/// }
/// ```
#[macro_export]
macro_rules! test_cases {
    ($name:ident, [$($case:expr),+ $(,)?], $test:expr) => {
        mod $name {
            use super::*;

            $(
                paste::paste! {
                    #[test]
                    fn [<case_ $case:lower:snake>]() {
                        let test_fn: fn(_) -> () = $test;
                        test_fn($case);
                    }
                }
            )+
        }
    };
}

/// Assert that two collections contain the same elements (order-independent).
///
/// # Example
///
/// ```rust
/// assert_contains_same!([1, 2, 3], [3, 1, 2]);
/// ```
#[macro_export]
macro_rules! assert_contains_same {
    ($left:expr, $right:expr) => {{
        let mut left: Vec<_> = $left.into_iter().collect();
        let mut right: Vec<_> = $right.into_iter().collect();
        left.sort();
        right.sort();
        assert_eq!(
            left, right,
            "Collections contain different elements"
        );
    }};
}

/// Wait for a condition to become true with timeout.
///
/// # Example
///
/// ```rust
/// wait_for!(Duration::from_secs(5), || user.is_online());
/// ```
#[macro_export]
macro_rules! wait_for {
    ($timeout:expr, $condition:expr) => {{
        let start = std::time::Instant::now();
        loop {
            if $condition {
                break;
            }
            if start.elapsed() > $timeout {
                panic!(
                    "Timeout waiting for condition: {}",
                    stringify!($condition)
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }};
}

/// Wait for a condition with custom poll interval.
#[macro_export]
macro_rules! wait_for_with_interval {
    ($timeout:expr, $interval:expr, $condition:expr) => {{
        let start = std::time::Instant::now();
        loop {
            if $condition {
                break;
            }
            if start.elapsed() > $timeout {
                panic!(
                    "Timeout waiting for condition: {}",
                    stringify!($condition)
                );
            }
            tokio::time::sleep($interval).await;
        }
    }};
}

// Re-export macros at module level
// Note: skip_if_no_server is defined in common/config.rs and re-exported via prelude
pub use skip_test;
pub use assert_ok;
pub use assert_err;
pub use assert_some;
pub use assert_none;
pub use async_test;
pub use integration_test;
pub use test_cases;
pub use assert_contains_same;
pub use wait_for;
pub use wait_for_with_interval;
