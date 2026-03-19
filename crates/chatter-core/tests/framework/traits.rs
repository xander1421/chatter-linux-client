//! Test traits for common patterns.
//!
//! Defines traits that test utilities implement for consistent behavior.

use std::future::Future;
use std::time::Duration;

/// Context for a test execution.
///
/// Provides access to test configuration and utilities.
pub trait TestContext {
    /// Get the test name.
    fn test_name(&self) -> &str;

    /// Check if running in CI environment.
    fn is_ci(&self) -> bool {
        std::env::var("CI").is_ok() || std::env::var("GITHUB_ACTIONS").is_ok()
    }

    /// Check if verbose logging is enabled.
    fn is_verbose(&self) -> bool {
        std::env::var("TEST_VERBOSE").is_ok()
    }

    /// Log a message if verbose mode is enabled.
    fn log(&self, msg: &str) {
        if self.is_verbose() {
            eprintln!("[{}] {}", self.test_name(), msg);
        }
    }
}

/// Types that can be reset to initial state.
///
/// Useful for test fixtures that need cleanup between tests.
pub trait Resettable {
    /// Reset to initial state.
    fn reset(&mut self);
}

/// Types that can be reset asynchronously.
pub trait AsyncResettable {
    /// Reset to initial state asynchronously.
    fn reset(&mut self) -> impl Future<Output = ()> + Send;
}

/// Extension trait for async test utilities.
pub trait AsyncTestExt: Future {
    /// Run with a timeout, panicking if exceeded.
    fn with_timeout(self, duration: Duration) -> impl Future<Output = Self::Output>
    where
        Self: Sized,
    {
        async move {
            tokio::time::timeout(duration, self)
                .await
                .expect("Test timed out")
        }
    }
}

impl<F: Future> AsyncTestExt for F {}

/// Trait for test data generators.
pub trait TestDataGenerator {
    /// The type of data generated.
    type Output;

    /// Generate a single instance.
    fn generate() -> Self::Output;

    /// Generate multiple instances.
    fn generate_many(count: usize) -> Vec<Self::Output> {
        (0..count).map(|_| Self::generate()).collect()
    }
}

/// Trait for types that can be converted to test fixtures.
pub trait IntoTestFixture<T> {
    /// Convert into a test fixture.
    fn into_fixture(self) -> T;
}

/// Trait for mock behaviors.
pub trait MockBehavior: Send + Sync {
    /// Called before each mock method invocation.
    fn before_call(&self, _method: &str) {}

    /// Called after each mock method invocation.
    fn after_call(&self, _method: &str) {}

    /// Check if mock should fail this call.
    fn should_fail(&self, _method: &str) -> bool {
        false
    }

    /// Get the failure error if should_fail returns true.
    fn failure_error(&self, _method: &str) -> Option<String> {
        None
    }
}

/// Null mock behavior that does nothing.
pub struct NullBehavior;

impl MockBehavior for NullBehavior {}

/// Mock behavior that fails after N calls.
pub struct FailAfterN {
    count: std::sync::atomic::AtomicU32,
    max: u32,
    error: String,
}

impl FailAfterN {
    pub fn new(max: u32, error: impl Into<String>) -> Self {
        Self {
            count: std::sync::atomic::AtomicU32::new(0),
            max,
            error: error.into(),
        }
    }
}

impl MockBehavior for FailAfterN {
    fn before_call(&self, _method: &str) {
        self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn should_fail(&self, _method: &str) -> bool {
        self.count.load(std::sync::atomic::Ordering::SeqCst) > self.max
    }

    fn failure_error(&self, _method: &str) -> Option<String> {
        if self.should_fail(_method) {
            Some(self.error.clone())
        } else {
            None
        }
    }
}

/// Mock behavior that randomly fails with a given probability.
pub struct RandomFailure {
    probability: f32,
    error: String,
}

impl RandomFailure {
    pub fn new(probability: f32, error: impl Into<String>) -> Self {
        Self {
            probability: probability.clamp(0.0, 1.0),
            error: error.into(),
        }
    }
}

impl MockBehavior for RandomFailure {
    fn should_fail(&self, _method: &str) -> bool {
        rand::random::<f32>() < self.probability
    }

    fn failure_error(&self, _method: &str) -> Option<String> {
        Some(self.error.clone())
    }
}
