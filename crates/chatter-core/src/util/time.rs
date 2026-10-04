//! Time utilities.
//!
//! Time formatting, relative time, and type-safe timestamp wrappers.
//!
//! # Timestamp Types
//!
//! This module provides newtype wrappers to prevent unit mismatch bugs:
//!
//! - [`TimestampNanos`]: Server/proto timestamps (nanoseconds since epoch)

use std::fmt;
use std::ops::{Add, Sub};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, TimeZone, Utc};

#[cfg(test)]
use chrono::Datelike;

// ============================================================================
// Type-Safe Timestamp Wrappers
// ============================================================================

/// Server/proto timestamps in nanoseconds since Unix epoch.
///
/// Used for:
/// - Protobuf `valid_until`, `expires_at` fields
/// - Server communication timestamps
/// - Internal deduplication keys
///
/// # Example
///
/// ```ignore
/// let server_expiry = TimestampNanos::from(response.valid_until);
/// if server_expiry > TimestampNanos::now() {
///     // Key is still valid
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TimestampNanos(pub u64);

// --- TimestampNanos Implementation ---

impl TimestampNanos {
    /// Create a timestamp representing the current time.
    #[inline]
    pub fn now() -> Self {
        Self(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before Unix epoch")
                .as_nanos() as u64,
        )
    }

    /// Create from raw nanoseconds value.
    #[inline]
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// Get the raw nanoseconds value.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// The current time floored to the whole second, for a timestamp the
    /// server sees in the clear (`SealedSenderMessage.timestamp`).
    ///
    /// The server only checks it for freshness (minutes of tolerance,
    /// `sealed_sender.go`), but a nanosecond reading of our clock on every
    /// anonymous send is a per-device fingerprint (clock offset and drift)
    /// that can link otherwise unlinkable sends. One second keeps freshness
    /// and drops the fingerprint.
    pub fn now_anonymized() -> Self {
        let now = Self::now().0;
        Self(now - now % 1_000_000_000)
    }

    /// Convert to `DateTime<Utc>`.
    ///
    /// Returns `None` if timestamp is 0 or conversion fails.
    pub fn to_datetime(self) -> Option<DateTime<Utc>> {
        if self.0 == 0 {
            return None;
        }
        let secs = (self.0 / 1_000_000_000) as i64;
        let nsecs = (self.0 % 1_000_000_000) as u32;
        Utc.timestamp_opt(secs, nsecs).single()
    }

    /// Check if this timestamp is in the past.
    #[inline]
    pub fn is_expired(self) -> bool {
        self < Self::now()
    }

    /// Check if this timestamp is in the future.
    #[inline]
    pub fn is_valid(self) -> bool {
        self > Self::now()
    }
}

impl From<u64> for TimestampNanos {
    #[inline]
    fn from(nanos: u64) -> Self {
        Self(nanos)
    }
}

impl From<TimestampNanos> for u64 {
    #[inline]
    fn from(ts: TimestampNanos) -> Self {
        ts.0
    }
}

impl fmt::Display for TimestampNanos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(dt) = self.to_datetime() {
            write!(f, "{}", dt.format("%Y-%m-%d %H:%M:%S%.3f UTC"))
        } else {
            write!(f, "0ns (epoch)")
        }
    }
}

impl Add<Duration> for TimestampNanos {
    type Output = Self;

    #[inline]
    fn add(self, rhs: Duration) -> Self::Output {
        Self(self.0.saturating_add(rhs.as_nanos() as u64))
    }
}

impl Sub<Duration> for TimestampNanos {
    type Output = Self;

    #[inline]
    fn sub(self, rhs: Duration) -> Self::Output {
        Self(self.0.saturating_sub(rhs.as_nanos() as u64))
    }
}

impl Sub<TimestampNanos> for TimestampNanos {
    type Output = Duration;

    #[inline]
    fn sub(self, rhs: TimestampNanos) -> Self::Output {
        Duration::from_nanos(self.0.saturating_sub(rhs.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timestamp_nanos_now() {
        let t1 = TimestampNanos::now();
        let t2 = TimestampNanos::now();
        assert!(t2 >= t1);
    }

    #[test]
    fn test_timestamp_nanos_rapid_unique() {
        use std::collections::HashSet;

        let timestamps: Vec<TimestampNanos> = (0..1000).map(|_| TimestampNanos::now()).collect();
        let unique: HashSet<_> = timestamps.iter().collect();
        assert_eq!(unique.len(), timestamps.len());
    }

    #[test]
    fn test_timestamp_nanos_to_datetime() {
        let nanos = TimestampNanos::from_nanos(1_704_067_200_000_000_000);
        let dt = nanos.to_datetime().unwrap();
        assert_eq!(dt.year(), 2024);
        assert_eq!(dt.month(), 1);
        assert_eq!(dt.day(), 1);
    }

    #[test]
    fn test_timestamp_nanos_is_expired() {
        let past = TimestampNanos::now() - Duration::from_secs(60);
        let future = TimestampNanos::now() + Duration::from_secs(60);
        assert!(past.is_expired());
        assert!(!future.is_expired());
    }

    #[test]
    fn test_timestamp_nanos_arithmetic() {
        let t1 = TimestampNanos::from_nanos(1_000_000_000);
        let t2 = t1 + Duration::from_secs(1);
        assert_eq!(t2.as_u64(), 2_000_000_000);

        let t3 = t2 - Duration::from_millis(500);
        assert_eq!(t3.as_u64(), 1_500_000_000);

        let diff = t2 - t1;
        assert_eq!(diff, Duration::from_secs(1));
    }

    #[test]
    fn test_timestamp_nanos_display() {
        let ts = TimestampNanos::from_nanos(0);
        assert_eq!(format!("{}", ts), "0ns (epoch)");

        let ts = TimestampNanos::from_nanos(1_704_067_200_000_000_000);
        assert!(format!("{}", ts).contains("2024-01-01"));
    }

    #[test]
    fn anonymized_timestamp_is_a_whole_second_and_fresh() {
        let before = TimestampNanos::now();
        let ts = TimestampNanos::now_anonymized();
        assert_eq!(ts.as_u64() % 1_000_000_000, 0);
        assert!(ts <= TimestampNanos::now());
        assert!(before.as_u64().saturating_sub(ts.as_u64()) < 1_000_000_000);
    }

    #[test]
    fn test_from_trait() {
        let nanos: TimestampNanos = 12345u64.into();
        assert_eq!(nanos.as_u64(), 12345);

        let n: u64 = nanos.into();
        assert_eq!(n, 12345);
    }
}
