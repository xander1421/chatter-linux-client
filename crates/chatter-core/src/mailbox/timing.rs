//! Timing obfuscation.
//!
//! Random delays and jitter for mailbox operations to prevent timing correlation attacks.
//!
//! The NDSS '21 paper notes that even with ephemeral mailboxes, timing correlation can
//! still link users if operations are performed predictably. This module adds random
//! delays to all mailbox operations.

use std::time::Duration;

use rand::Rng;
use tokio::time::sleep;

/// Default minimum delay before mailbox operations (50ms).
pub const DEFAULT_MIN_DELAY_MS: u64 = 50;

/// Default maximum delay before mailbox operations (500ms).
pub const DEFAULT_MAX_DELAY_MS: u64 = 500;

/// Default jitter percentage for polling intervals (20%).
pub const DEFAULT_JITTER_PERCENT: u8 = 20;

/// Configuration for timing obfuscation.
#[derive(Debug, Clone)]
pub struct TimingConfig {
    /// Whether timing obfuscation is enabled.
    pub enabled: bool,
    /// Minimum delay before operations (milliseconds).
    pub min_delay_ms: u64,
    /// Maximum delay before operations (milliseconds).
    pub max_delay_ms: u64,
    /// Jitter percentage for intervals (0-100).
    pub jitter_percent: u8,
}

impl Default for TimingConfig {
    fn default() -> Self {
        Self {
            enabled: true, // Enabled by default for privacy
            min_delay_ms: DEFAULT_MIN_DELAY_MS,
            max_delay_ms: DEFAULT_MAX_DELAY_MS,
            jitter_percent: DEFAULT_JITTER_PERCENT,
        }
    }
}

impl TimingConfig {
    /// Create a disabled configuration (for testing).
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Default::default()
        }
    }

    /// Create a configuration with custom delay bounds.
    pub fn with_delays(min_ms: u64, max_ms: u64) -> Self {
        Self {
            enabled: true,
            min_delay_ms: min_ms,
            max_delay_ms: max_ms.max(min_ms),
            ..Default::default()
        }
    }
}

/// Timing obfuscation helper.
///
/// Provides methods for adding random delays and jitter to mailbox operations.
///
/// # Example
///
/// ```ignore
/// use chatter::mailbox::timing::{TimingObfuscation, TimingConfig};
///
/// let timing = TimingObfuscation::new(TimingConfig::default());
///
/// // Add random delay before sending
/// timing.delay_before_send().await;
/// mailbox_client.send_to_ephemeral_mailbox(...).await?;
///
/// // Get jittered poll interval
/// let base_interval = Duration::from_secs(30);
/// let jittered = timing.jitter_interval(base_interval);
/// tokio::time::sleep(jittered).await;
/// ```
#[derive(Clone)]
pub struct TimingObfuscation {
    config: TimingConfig,
}

impl TimingObfuscation {
    /// Create a new TimingObfuscation helper.
    pub fn new(config: TimingConfig) -> Self {
        Self { config }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(TimingConfig::default())
    }

    /// Check if timing obfuscation is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Get the current configuration.
    pub fn config(&self) -> &TimingConfig {
        &self.config
    }

    /// Generate a random delay duration.
    ///
    /// Uses uniform distribution between min and max delay.
    fn random_delay(&self) -> Duration {
        if !self.config.enabled || self.config.min_delay_ms >= self.config.max_delay_ms {
            return Duration::from_millis(self.config.min_delay_ms);
        }

        let mut rng = rand::rng();
        let delay_ms = rng.random_range(self.config.min_delay_ms..=self.config.max_delay_ms);
        Duration::from_millis(delay_ms)
    }

    /// Add random delay before a send operation.
    ///
    /// Prevents timing analysis by making sends unpredictable.
    pub async fn delay_before_send(&self) {
        if self.config.enabled {
            let delay = self.random_delay();
            tracing::trace!(
                delay_ms = delay.as_millis(),
                "Timing obfuscation: pre-send delay"
            );
            sleep(delay).await;
        }
    }

    /// Add random delay before a poll operation.
    ///
    /// Prevents timing analysis by making polls unpredictable.
    pub async fn delay_before_poll(&self) {
        if self.config.enabled {
            let delay = self.random_delay();
            tracing::trace!(
                delay_ms = delay.as_millis(),
                "Timing obfuscation: pre-poll delay"
            );
            sleep(delay).await;
        }
    }

    /// Add random delay before any mailbox operation.
    ///
    /// Generic delay for operations not covered by send/poll.
    pub async fn delay_before_operation(&self) {
        if self.config.enabled {
            let delay = self.random_delay();
            tracing::trace!(
                delay_ms = delay.as_millis(),
                "Timing obfuscation: pre-operation delay"
            );
            sleep(delay).await;
        }
    }

    /// Apply jitter to an interval duration.
    ///
    /// Adds random variation (+/- jitter_percent) to the base interval.
    /// Useful for making polling schedules less predictable.
    ///
    /// # Arguments
    ///
    /// * `base_interval` - The base interval to jitter
    ///
    /// # Returns
    ///
    /// The jittered interval (always positive).
    pub fn jitter_interval(&self, base_interval: Duration) -> Duration {
        if !self.config.enabled || self.config.jitter_percent == 0 {
            return base_interval;
        }

        let mut rng = rand::rng();
        let base_ms = base_interval.as_millis() as f64;
        let jitter_factor = self.config.jitter_percent as f64 / 100.0;

        // Random value in [-jitter_factor, +jitter_factor]
        let random_factor: f64 = rng.random_range(-jitter_factor..=jitter_factor);

        // Apply jitter
        let jittered_ms = base_ms * (1.0 + random_factor);

        // Ensure positive result (at least 1ms)
        Duration::from_millis(jittered_ms.max(1.0) as u64)
    }

    /// Decorrelation delay after authenticated CDN upload, before anonymous
    /// mailbox metadata send.
    ///
    /// Prevents the server from correlating "User A uploaded 5MB at t=0"
    /// with "anonymous mailbox write at t=0.2s". The message is already
    /// shown in the UI (optimistic display), so this delay is invisible
    /// to the user — it only affects the network send.
    ///
    /// Uses 500ms–3s range: long enough that the server can't trivially
    /// pair upload→mailbox_write across thousands of concurrent users,
    /// short enough that delivery isn't noticeably delayed.
    pub async fn delay_for_attachment_decorrelation(&self) {
        if !self.config.enabled {
            return;
        }

        let delay = {
            let mut rng = rand::rng();
            // Exponential distribution, mean ~1.5s, clamped to [500ms, 3s].
            let u: f64 = rng.random::<f64>().max(1e-10);
            let exp_secs = -u.ln() * 1.5;
            let clamped = exp_secs.clamp(0.5, 3.0);
            Duration::from_secs_f64(clamped)
        };

        tracing::debug!(
            delay_secs = delay.as_secs_f64(),
            "Timing obfuscation: attachment decorrelation delay"
        );
        sleep(delay).await;
    }

    /// Generate a random delay for batch operations.
    ///
    /// Used between operations in a batch to prevent timing patterns.
    pub async fn delay_between_batch_items(&self) {
        if self.config.enabled {
            // Shorter delay for batch items (10-50ms)
            let delay_ms = {
                let mut rng = rand::rng();
                rng.random_range(10..=50)
            };
            sleep(Duration::from_millis(delay_ms)).await;
        }
    }
}

/// Apply timing obfuscation to an async operation.
///
/// Convenience macro that adds delay before executing an operation.
///
/// # Example
///
/// ```ignore
/// let timing = TimingObfuscation::with_defaults();
/// let result = with_timing_delay!(timing, async {
///     client.send_message(msg).await
/// });
/// ```
#[macro_export]
macro_rules! with_timing_delay {
    ($timing:expr, $operation:expr) => {{
        $timing.delay_before_operation().await;
        $operation
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = TimingConfig::default();
        assert!(config.enabled);
        assert_eq!(config.min_delay_ms, DEFAULT_MIN_DELAY_MS);
        assert_eq!(config.max_delay_ms, DEFAULT_MAX_DELAY_MS);
        assert_eq!(config.jitter_percent, DEFAULT_JITTER_PERCENT);
    }

    #[test]
    fn test_disabled_config() {
        let config = TimingConfig::disabled();
        assert!(!config.enabled);
    }

    #[test]
    fn test_custom_delays() {
        let config = TimingConfig::with_delays(100, 200);
        assert!(config.enabled);
        assert_eq!(config.min_delay_ms, 100);
        assert_eq!(config.max_delay_ms, 200);
    }

    #[test]
    fn test_custom_delays_min_max_swap() {
        // If max < min, should be clamped
        let config = TimingConfig::with_delays(200, 100);
        assert_eq!(config.min_delay_ms, 200);
        assert_eq!(config.max_delay_ms, 200); // max(100, 200)
    }

    #[test]
    fn test_random_delay_bounds() {
        let timing = TimingObfuscation::new(TimingConfig::with_delays(100, 200));

        for _ in 0..100 {
            let delay = timing.random_delay();
            assert!(delay.as_millis() >= 100);
            assert!(delay.as_millis() <= 200);
        }
    }

    #[test]
    fn test_disabled_delay() {
        let timing = TimingObfuscation::new(TimingConfig::disabled());
        let delay = timing.random_delay();
        assert_eq!(delay.as_millis(), DEFAULT_MIN_DELAY_MS as u128);
    }

    #[test]
    fn test_jitter_interval_disabled() {
        let timing = TimingObfuscation::new(TimingConfig::disabled());
        let base = Duration::from_secs(30);
        let jittered = timing.jitter_interval(base);
        assert_eq!(jittered, base);
    }

    #[test]
    fn test_jitter_interval_bounds() {
        let config = TimingConfig {
            enabled: true,
            min_delay_ms: 50,
            max_delay_ms: 500,
            jitter_percent: 20,
        };
        let timing = TimingObfuscation::new(config);

        let base = Duration::from_secs(30);
        let base_ms = base.as_millis();

        for _ in 0..100 {
            let jittered = timing.jitter_interval(base);
            let jittered_ms = jittered.as_millis();

            // Should be within +/- 20% of base
            let min_expected = (base_ms as f64 * 0.8) as u128;
            let max_expected = (base_ms as f64 * 1.2) as u128;

            assert!(
                jittered_ms >= min_expected && jittered_ms <= max_expected,
                "jittered {} not in [{}, {}]",
                jittered_ms,
                min_expected,
                max_expected
            );
        }
    }

    #[test]
    fn test_jitter_zero_percent() {
        let config = TimingConfig {
            enabled: true,
            min_delay_ms: 50,
            max_delay_ms: 500,
            jitter_percent: 0,
        };
        let timing = TimingObfuscation::new(config);

        let base = Duration::from_secs(30);
        let jittered = timing.jitter_interval(base);
        assert_eq!(jittered, base);
    }

    #[tokio::test]
    async fn test_delay_before_send() {
        let timing = TimingObfuscation::new(TimingConfig::with_delays(10, 20));

        let start = std::time::Instant::now();
        timing.delay_before_send().await;
        let elapsed = start.elapsed();

        assert!(elapsed.as_millis() >= 10);
        assert!(elapsed.as_millis() <= 100); // Allow some buffer for test timing
    }

    #[tokio::test]
    async fn test_delay_disabled_is_fast() {
        let timing = TimingObfuscation::new(TimingConfig::disabled());

        let start = std::time::Instant::now();
        timing.delay_before_send().await;
        let elapsed = start.elapsed();

        // Should be nearly instant when disabled
        assert!(elapsed.as_millis() < 10);
    }

    #[test]
    fn test_is_enabled() {
        let enabled = TimingObfuscation::with_defaults();
        assert!(enabled.is_enabled());

        let disabled = TimingObfuscation::new(TimingConfig::disabled());
        assert!(!disabled.is_enabled());
    }

    #[tokio::test]
    async fn test_attachment_decorrelation_delay_bounds() {
        let timing = TimingObfuscation::with_defaults();

        let start = std::time::Instant::now();
        timing.delay_for_attachment_decorrelation().await;
        let elapsed = start.elapsed();

        // Should be in [500ms, 3s] range
        assert!(
            elapsed.as_millis() >= 450, // small buffer for test scheduling
            "decorrelation delay too short: {}ms",
            elapsed.as_millis()
        );
        assert!(
            elapsed.as_millis() <= 3500, // small buffer
            "decorrelation delay too long: {}ms",
            elapsed.as_millis()
        );
    }

    #[tokio::test]
    async fn test_attachment_decorrelation_disabled_is_instant() {
        let timing = TimingObfuscation::new(TimingConfig::disabled());

        let start = std::time::Instant::now();
        timing.delay_for_attachment_decorrelation().await;
        let elapsed = start.elapsed();

        assert!(elapsed.as_millis() < 10);
    }
}
