//! Connection recovery.
//!
//! Fibonacci backoff and connection recovery strategies.

use std::time::Duration;

use rand::Rng;

/// Fibonacci-based backoff strategy with optional jitter.
///
/// Follows Signal's pattern of using Fibonacci sequence timeouts
/// for connection retry attempts, providing a natural growth curve
/// that's less aggressive than exponential backoff.
pub struct FibonacciBackoff {
    timeouts: &'static [Duration],
    index: usize,
    jitter: Duration,
}

impl FibonacciBackoff {
    /// Standard Fibonacci timeouts (1s to 55s).
    ///
    /// Suitable for most reconnection scenarios with reasonable
    /// retry frequency.
    pub const STANDARD: &'static [Duration] = &[
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(3),
        Duration::from_secs(5),
        Duration::from_secs(8),
        Duration::from_secs(13),
        Duration::from_secs(21),
        Duration::from_secs(34),
        Duration::from_secs(55),
    ];

    /// Extended Fibonacci timeouts for prolonged offline scenarios.
    ///
    /// Extends up to ~10 minutes between retries for situations
    /// where the server may be down for an extended period.
    pub const EXTENDED: &'static [Duration] = &[
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(3),
        Duration::from_secs(5),
        Duration::from_secs(8),
        Duration::from_secs(13),
        Duration::from_secs(21),
        Duration::from_secs(34),
        Duration::from_secs(55),
        Duration::from_secs(89),
        Duration::from_secs(144),
        Duration::from_secs(233),
        Duration::from_secs(377),
        Duration::from_secs(610),
    ];

    /// Default jitter duration (5 seconds).
    const DEFAULT_JITTER: Duration = Duration::from_secs(5);

    /// Create a new FibonacciBackoff with default jitter.
    ///
    /// # Arguments
    ///
    /// * `timeouts` - Static slice of timeout durations (use STANDARD or EXTENDED)
    pub fn new(timeouts: &'static [Duration]) -> Self {
        Self {
            timeouts,
            index: 0,
            jitter: Self::DEFAULT_JITTER,
        }
    }

    /// Create a new FibonacciBackoff with custom jitter.
    ///
    /// # Arguments
    ///
    /// * `timeouts` - Static slice of timeout durations
    /// * `jitter` - Maximum random jitter to add to each timeout
    pub fn with_jitter(timeouts: &'static [Duration], jitter: Duration) -> Self {
        Self {
            timeouts,
            index: 0,
            jitter,
        }
    }

    /// Get the next timeout duration and advance the index.
    ///
    /// Returns the current timeout plus random jitter, then advances
    /// to the next timeout in the sequence. If already at the last
    /// timeout, stays at that value.
    pub fn next_timeout(&mut self) -> Duration {
        let base_timeout = self.timeouts[self.index];

        // Advance index but cap at last element
        if self.index < self.timeouts.len() - 1 {
            self.index += 1;
        }

        // Add random jitter
        if self.jitter > Duration::ZERO {
            let jitter_nanos = rand::rng().random_range(0..self.jitter.as_nanos() as u64);
            base_timeout + Duration::from_nanos(jitter_nanos)
        } else {
            base_timeout
        }
    }

    /// Reset to the beginning of the timeout sequence.
    pub fn reset(&mut self) {
        self.index = 0;
    }

    /// Reset with a new timeout sequence.
    ///
    /// # Arguments
    ///
    /// * `timeouts` - New static slice of timeout durations
    pub fn reset_with(&mut self, timeouts: &'static [Duration]) {
        self.timeouts = timeouts;
        self.index = 0;
    }

    /// Get the current index in the sequence.
    pub fn current_index(&self) -> usize {
        self.index
    }

    /// Check if at the maximum timeout (end of sequence).
    pub fn is_at_max(&self) -> bool {
        self.index >= self.timeouts.len() - 1
    }
}

/// Connection recovery manager.
///
/// Coordinates reconnection attempts using backoff strategies.
pub struct ConnectionRecovery {
    backoff: FibonacciBackoff,
    max_attempts: Option<usize>,
    attempt_count: usize,
}

impl ConnectionRecovery {
    /// Create a new ConnectionRecovery with standard backoff.
    pub fn new() -> Self {
        Self {
            backoff: FibonacciBackoff::new(FibonacciBackoff::STANDARD),
            max_attempts: None,
            attempt_count: 0,
        }
    }

    /// Create with a maximum number of attempts.
    pub fn with_max_attempts(max_attempts: usize) -> Self {
        Self {
            backoff: FibonacciBackoff::new(FibonacciBackoff::STANDARD),
            max_attempts: Some(max_attempts),
            attempt_count: 0,
        }
    }

    /// Create with extended backoff for prolonged offline.
    pub fn extended() -> Self {
        Self {
            backoff: FibonacciBackoff::new(FibonacciBackoff::EXTENDED),
            max_attempts: None,
            attempt_count: 0,
        }
    }

    /// Get the next retry delay, or None if max attempts exceeded.
    pub fn next_delay(&mut self) -> Option<Duration> {
        if let Some(max) = self.max_attempts {
            if self.attempt_count >= max {
                return None;
            }
        }
        self.attempt_count += 1;
        Some(self.backoff.next_timeout())
    }

    /// Reset the recovery state after successful connection.
    pub fn reset(&mut self) {
        self.backoff.reset();
        self.attempt_count = 0;
    }

    /// Get the current attempt count.
    pub fn attempt_count(&self) -> usize {
        self.attempt_count
    }

    /// Check if recovery has been exhausted (max attempts reached).
    pub fn is_exhausted(&self) -> bool {
        self.max_attempts
            .map(|max| self.attempt_count >= max)
            .unwrap_or(false)
    }
}

impl Default for ConnectionRecovery {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fibonacci_sequence() {
        // Test without jitter to verify exact sequence
        let mut backoff = FibonacciBackoff::with_jitter(FibonacciBackoff::STANDARD, Duration::ZERO);

        assert_eq!(backoff.next_timeout(), Duration::from_secs(1));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(2));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(3));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(5));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(8));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(13));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(21));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(34));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(55));
        // Should stay at max
        assert_eq!(backoff.next_timeout(), Duration::from_secs(55));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(55));
    }

    #[test]
    fn test_jitter_bounds() {
        let jitter = Duration::from_secs(5);
        let mut backoff = FibonacciBackoff::with_jitter(FibonacciBackoff::STANDARD, jitter);

        // Run multiple times to test jitter
        for _ in 0..10 {
            backoff.reset();
            let timeout = backoff.next_timeout();
            let base = Duration::from_secs(1);
            assert!(timeout >= base);
            assert!(timeout < base + jitter);
        }
    }

    #[test]
    fn test_reset() {
        let mut backoff = FibonacciBackoff::with_jitter(FibonacciBackoff::STANDARD, Duration::ZERO);

        // Advance a few times
        backoff.next_timeout();
        backoff.next_timeout();
        backoff.next_timeout();
        assert_eq!(backoff.current_index(), 3);

        // Reset
        backoff.reset();
        assert_eq!(backoff.current_index(), 0);
        assert_eq!(backoff.next_timeout(), Duration::from_secs(1));
    }

    #[test]
    fn test_extended_sequence() {
        let mut backoff =
            FibonacciBackoff::with_jitter(FibonacciBackoff::EXTENDED, Duration::ZERO);

        // Advance through standard
        for _ in 0..9 {
            backoff.next_timeout();
        }

        // Now in extended territory
        assert_eq!(backoff.next_timeout(), Duration::from_secs(89));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(144));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(233));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(377));
        assert_eq!(backoff.next_timeout(), Duration::from_secs(610));
        // Should stay at max
        assert_eq!(backoff.next_timeout(), Duration::from_secs(610));
    }

    #[test]
    fn test_reset_with_different_sequence() {
        let mut backoff = FibonacciBackoff::with_jitter(FibonacciBackoff::STANDARD, Duration::ZERO);

        // Start with standard
        assert_eq!(backoff.next_timeout(), Duration::from_secs(1));

        // Switch to extended
        backoff.reset_with(FibonacciBackoff::EXTENDED);
        assert_eq!(backoff.current_index(), 0);
        assert_eq!(backoff.next_timeout(), Duration::from_secs(1));

        // Verify we're now on extended sequence
        for _ in 0..8 {
            backoff.next_timeout();
        }
        assert_eq!(backoff.next_timeout(), Duration::from_secs(89)); // Extended-only value
    }

    #[test]
    fn test_is_at_max() {
        let mut backoff = FibonacciBackoff::with_jitter(FibonacciBackoff::STANDARD, Duration::ZERO);

        assert!(!backoff.is_at_max());

        // Advance to end
        for _ in 0..9 {
            backoff.next_timeout();
        }

        assert!(backoff.is_at_max());
    }

    #[test]
    fn test_connection_recovery_basic() {
        let mut recovery = ConnectionRecovery::new();

        assert_eq!(recovery.attempt_count(), 0);
        assert!(!recovery.is_exhausted());

        let delay = recovery.next_delay();
        assert!(delay.is_some());
        assert_eq!(recovery.attempt_count(), 1);
    }

    #[test]
    fn test_connection_recovery_with_max_attempts() {
        let mut recovery = ConnectionRecovery::with_max_attempts(3);

        assert!(recovery.next_delay().is_some());
        assert!(recovery.next_delay().is_some());
        assert!(recovery.next_delay().is_some());
        assert!(recovery.next_delay().is_none());
        assert!(recovery.is_exhausted());
    }

    #[test]
    fn test_connection_recovery_reset() {
        let mut recovery = ConnectionRecovery::with_max_attempts(3);

        recovery.next_delay();
        recovery.next_delay();
        assert_eq!(recovery.attempt_count(), 2);

        recovery.reset();
        assert_eq!(recovery.attempt_count(), 0);
        assert!(!recovery.is_exhausted());
    }
}
