//! Reconnect backoff configuration.
//!
//! Tunable parameters for the exponential-backoff reconnect loop used by the
//! push listener (and any other component that reconnects to the server).
//! Prior to this module these constants were baked into `push_listener.rs`
//! (`initial=5s`, `max=60s`, `multiplier=2`, infinite attempts) which made
//! deployment-specific tuning impossible — e.g. mobile clients may want a
//! tighter give-up to conserve battery while desktop clients may want to
//! retry indefinitely.
//!
//! Call [`ReconnectConfig::default`] for the historical behavior, or build
//! a custom config for tighter control:
//!
//! ```
//! use std::time::Duration;
//! use chatter_core::network::ReconnectConfig;
//!
//! let cfg = ReconnectConfig {
//!     initial_delay: Duration::from_secs(5),
//!     max_delay: Duration::from_secs(60),
//!     backoff_multiplier: 2.0,
//!     jitter_ratio: 0.2,
//!     max_attempts: Some(10),
//!     give_up_after: Some(Duration::from_secs(600)),
//! };
//! ```
//!
//! At default values the timing sequence is preserved byte-for-byte with the
//! previous hard-coded implementation (5s, 10s, 20s, 40s, 60s, 60s, ...).

use std::time::Duration;

/// Historical initial delay baked into `push_listener.rs`.
const DEFAULT_INITIAL_DELAY: Duration = Duration::from_secs(5);

/// Historical max delay cap baked into `push_listener.rs`.
const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Historical multiplier baked into `push_listener.rs` (`retry_delay * 2`).
const DEFAULT_BACKOFF_MULTIPLIER: f32 = 2.0;

/// Default jitter ratio (fraction of computed delay). 0.0 preserves the
/// previous jitter-free behavior. Callers can opt in to e.g. 0.2 for
/// ±20% jitter.
const DEFAULT_JITTER_RATIO: f32 = 0.0;

/// Reconnect backoff / give-up tuning.
///
/// All fields are public so callers can construct partial configs with the
/// struct-update syntax:
///
/// ```
/// use chatter_core::network::ReconnectConfig;
/// let cfg = ReconnectConfig {
///     max_attempts: Some(5),
///     ..ReconnectConfig::default()
/// };
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReconnectConfig {
    /// Delay before the first reconnect attempt.
    pub initial_delay: Duration,

    /// Upper cap on any single retry delay. The computed delay never
    /// exceeds this value regardless of multiplier / attempt count.
    pub max_delay: Duration,

    /// Multiplier applied after each failed attempt. `next = cur * multiplier`.
    /// Clamped to `max_delay`. Values < 1.0 produce a constant delay; values
    /// between 1.0 and 2.0 are gentler than the historical 2x.
    pub backoff_multiplier: f32,

    /// Random jitter ratio added/subtracted from each delay. `0.2` adds up
    /// to ±20% of the base delay. Use `0.0` for deterministic timing.
    pub jitter_ratio: f32,

    /// Maximum number of retry attempts before giving up. `None` = infinite.
    /// Matches the historical "retry forever" behavior by default.
    pub max_attempts: Option<u32>,

    /// Absolute wall-clock duration after which the reconnect loop gives up,
    /// regardless of attempt count. `None` = never time out.
    ///
    /// When both `max_attempts` and `give_up_after` are set the first to
    /// trip wins.
    pub give_up_after: Option<Duration>,
}

impl ReconnectConfig {
    /// Construct the historical defaults as a `const`. Usable in static
    /// contexts, and guarantees zero runtime cost when the caller doesn't
    /// want to override anything.
    ///
    /// Preserves the exact behavior of the pre-refactor push listener:
    /// `5s → 10s → 20s → 40s → 60s → 60s → …`, no jitter, no give-up.
    pub const fn const_default() -> Self {
        Self {
            initial_delay: DEFAULT_INITIAL_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
            backoff_multiplier: DEFAULT_BACKOFF_MULTIPLIER,
            jitter_ratio: DEFAULT_JITTER_RATIO,
            max_attempts: None,
            give_up_after: None,
        }
    }

    /// Compute the next delay given the current one.
    ///
    /// `current * backoff_multiplier`, clamped to `[initial_delay, max_delay]`.
    /// Jitter is not applied here — callers should call [`Self::with_jitter`]
    /// on the result when they actually sleep.
    pub fn next_delay(&self, current: Duration) -> Duration {
        let scaled_secs = current.as_secs_f32() * self.backoff_multiplier;
        let scaled = Duration::from_secs_f32(scaled_secs.max(0.0));
        scaled.min(self.max_delay)
    }

    /// Apply jitter to a base delay using the supplied RNG source.
    ///
    /// `rand_unit` must be a uniform `f32` in `[0.0, 1.0)` (e.g. from
    /// `rand::rng().random::<f32>()`). Factoring the RNG out lets tests
    /// drive the jitter deterministically.
    pub fn with_jitter(&self, base: Duration, rand_unit: f32) -> Duration {
        if self.jitter_ratio <= 0.0 {
            return base;
        }
        let ratio = self.jitter_ratio.min(1.0);
        // map [0, 1) -> [-ratio, +ratio]
        let offset = (rand_unit * 2.0 - 1.0) * ratio;
        let base_secs = base.as_secs_f32();
        let jittered = (base_secs * (1.0 + offset)).max(0.0);
        Duration::from_secs_f32(jittered)
    }

    /// Returns true if the caller should give up at this attempt number
    /// (1-indexed) and elapsed wall-clock time since the first attempt.
    pub fn should_give_up(&self, attempt: u32, elapsed: Duration) -> bool {
        if let Some(max) = self.max_attempts {
            if attempt >= max {
                return true;
            }
        }
        if let Some(deadline) = self.give_up_after {
            if elapsed >= deadline {
                return true;
            }
        }
        false
    }
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self::const_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_historical_constants() {
        let cfg = ReconnectConfig::default();

        // Golden values — do NOT change without updating RELEASE_STATUS.md
        // and coordinating with the mobile teams.
        assert_eq!(cfg.initial_delay, Duration::from_secs(5));
        assert_eq!(cfg.max_delay, Duration::from_secs(60));
        assert_eq!(cfg.backoff_multiplier, 2.0);
        assert_eq!(cfg.jitter_ratio, 0.0);
        assert_eq!(cfg.max_attempts, None);
        assert_eq!(cfg.give_up_after, None);
    }

    #[test]
    fn const_default_is_const() {
        const CFG: ReconnectConfig = ReconnectConfig::const_default();
        assert_eq!(CFG.initial_delay, Duration::from_secs(5));
    }

    #[test]
    fn default_sequence_matches_legacy_push_listener() {
        // Legacy behavior was: retry_delay = (retry_delay * 2).min(max_delay)
        // starting at 5s, capped at 60s.
        let cfg = ReconnectConfig::default();
        let mut d = cfg.initial_delay;
        assert_eq!(d, Duration::from_secs(5));
        d = cfg.next_delay(d);
        assert_eq!(d, Duration::from_secs(10));
        d = cfg.next_delay(d);
        assert_eq!(d, Duration::from_secs(20));
        d = cfg.next_delay(d);
        assert_eq!(d, Duration::from_secs(40));
        d = cfg.next_delay(d);
        // 80 -> clamped to 60
        assert_eq!(d, Duration::from_secs(60));
        d = cfg.next_delay(d);
        // stays at 60
        assert_eq!(d, Duration::from_secs(60));
    }

    #[test]
    fn backoff_respects_max_delay_cap() {
        let cfg = ReconnectConfig {
            initial_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(500),
            backoff_multiplier: 10.0,
            ..ReconnectConfig::default()
        };
        // 100ms * 10 = 1s, should clamp to 500ms
        assert_eq!(cfg.next_delay(cfg.initial_delay), Duration::from_millis(500));
        // Still clamped on subsequent calls
        assert_eq!(
            cfg.next_delay(Duration::from_millis(500)),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn max_attempts_gives_up() {
        let cfg = ReconnectConfig {
            max_attempts: Some(3),
            ..ReconnectConfig::default()
        };
        assert!(!cfg.should_give_up(1, Duration::ZERO));
        assert!(!cfg.should_give_up(2, Duration::ZERO));
        // attempt 3 is the 3rd retry, >= max_attempts -> give up
        assert!(cfg.should_give_up(3, Duration::ZERO));
        assert!(cfg.should_give_up(4, Duration::ZERO));
    }

    #[test]
    fn give_up_after_wins_when_hit_first() {
        let cfg = ReconnectConfig {
            max_attempts: Some(1_000), // effectively infinite for this test
            give_up_after: Some(Duration::from_secs(10)),
            ..ReconnectConfig::default()
        };
        // Attempt count not exceeded
        assert!(!cfg.should_give_up(1, Duration::from_secs(5)));
        // Wall-clock exceeded — give up despite many attempts left
        assert!(cfg.should_give_up(1, Duration::from_secs(10)));
        assert!(cfg.should_give_up(1, Duration::from_secs(20)));
    }

    #[test]
    fn no_jitter_when_ratio_zero() {
        let cfg = ReconnectConfig::default();
        let base = Duration::from_secs(7);
        // Any rand_unit value should yield the base delay unchanged
        assert_eq!(cfg.with_jitter(base, 0.0), base);
        assert_eq!(cfg.with_jitter(base, 0.5), base);
        assert_eq!(cfg.with_jitter(base, 0.99), base);
    }

    #[test]
    fn jitter_within_expected_bounds() {
        let cfg = ReconnectConfig {
            jitter_ratio: 0.2,
            ..ReconnectConfig::default()
        };
        let base = Duration::from_secs(10);
        // Extremes: rand_unit=0 gives -20%, rand_unit→1 gives +20%
        let lo = cfg.with_jitter(base, 0.0);
        let hi = cfg.with_jitter(base, 0.9999);
        assert!(lo.as_secs_f32() >= 8.0 - 0.01);
        assert!(lo.as_secs_f32() <= 8.0 + 0.01);
        assert!(hi.as_secs_f32() >= 12.0 - 0.01);
        assert!(hi.as_secs_f32() <= 12.0 + 0.01);
        // Mid: rand_unit=0.5 -> 0% offset
        let mid = cfg.with_jitter(base, 0.5);
        assert!((mid.as_secs_f32() - 10.0).abs() < 0.01);
    }

    #[test]
    fn jitter_ratio_clamped_to_1() {
        let cfg = ReconnectConfig {
            jitter_ratio: 5.0, // nonsensical, should clamp to 1.0
            ..ReconnectConfig::default()
        };
        let base = Duration::from_secs(10);
        // At rand_unit=0 we'd otherwise go negative; floor at 0.
        let lo = cfg.with_jitter(base, 0.0);
        assert_eq!(lo, Duration::ZERO);
    }

    #[test]
    fn no_give_up_by_default() {
        let cfg = ReconnectConfig::default();
        // Default has no limits — should never give up.
        assert!(!cfg.should_give_up(0, Duration::ZERO));
        assert!(!cfg.should_give_up(1_000_000, Duration::from_secs(86_400)));
    }
}
