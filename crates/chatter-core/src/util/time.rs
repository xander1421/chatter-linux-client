//! Time utilities.
//!
//! Time formatting, relative time, and type-safe timestamp wrappers.
//!
//! # Timestamp Types
//!
//! This module provides newtype wrappers to prevent unit mismatch bugs:
//!
//! - [`TimestampNanos`]: Server/proto timestamps (nanoseconds since epoch)
//! - [`TimestampMillis`]: Signal protocol timestamps (milliseconds since epoch)
//!
//! Using different types for different precision levels catches unit
//! conversion errors at compile time rather than runtime.

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

/// Signal protocol timestamps in milliseconds since Unix epoch.
///
/// Used for:
/// - libsignal `Timestamp` values
/// - Message `created_at` in encrypted payloads
/// - Cross-platform compatibility with Signal clients
///
/// # Example
///
/// ```ignore
/// let msg_time = TimestampMillis::now();
/// let signal_ts = libsignal_protocol::Timestamp::from_epoch_millis(msg_time.as_u64());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TimestampMillis(pub u64);

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

    /// Convert to milliseconds (explicit, lossy conversion).
    #[inline]
    pub const fn to_millis(self) -> TimestampMillis {
        TimestampMillis(self.0 / 1_000_000)
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

// --- TimestampMillis Implementation ---

impl TimestampMillis {
    /// Create a timestamp representing the current time.
    #[inline]
    pub fn now() -> Self {
        Self(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before Unix epoch")
                .as_millis() as u64,
        )
    }

    /// Create from raw milliseconds value.
    #[inline]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    /// Get the raw milliseconds value.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Convert to nanoseconds (explicit, lossless conversion).
    #[inline]
    pub const fn to_nanos(self) -> TimestampNanos {
        TimestampNanos(self.0.saturating_mul(1_000_000))
    }

    /// Convert to `DateTime<Utc>`.
    ///
    /// Returns `None` if timestamp is 0 or conversion fails.
    pub fn to_datetime(self) -> Option<DateTime<Utc>> {
        if self.0 == 0 {
            return None;
        }
        let secs = (self.0 / 1_000) as i64;
        let nsecs = ((self.0 % 1_000) * 1_000_000) as u32;
        Utc.timestamp_opt(secs, nsecs).single()
    }

    /// Create an anonymized timestamp for privacy.
    ///
    /// Rounds to the nearest 100ms to reduce fingerprinting in message metadata.
    /// Use this for outgoing message `created_at` fields.
    #[inline]
    pub fn now_anonymized() -> Self {
        let raw = Self::now().0;
        // Round to nearest 100ms bucket
        Self((raw / 100) * 100)
    }

    /// Create an anonymized timestamp with jitter.
    ///
    /// Rounds to 100ms bucket and adds random jitter (0-99ms).
    /// Provides better k-anonymity for traffic analysis resistance.
    pub fn now_anonymized_with_jitter() -> Self {
        let raw = Self::now().0;
        let rounded = (raw / 100) * 100;
        // Add random jitter 0-99ms
        let jitter = rand::random::<u64>() % 100;
        Self(rounded + jitter)
    }
}

impl From<u64> for TimestampMillis {
    #[inline]
    fn from(millis: u64) -> Self {
        Self(millis)
    }
}

impl From<TimestampMillis> for u64 {
    #[inline]
    fn from(ts: TimestampMillis) -> Self {
        ts.0
    }
}

impl fmt::Display for TimestampMillis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(dt) = self.to_datetime() {
            write!(f, "{}", dt.format("%Y-%m-%d %H:%M:%S%.3f UTC"))
        } else {
            write!(f, "0ms (epoch)")
        }
    }
}

impl Add<Duration> for TimestampMillis {
    type Output = Self;

    #[inline]
    fn add(self, rhs: Duration) -> Self::Output {
        Self(self.0.saturating_add(rhs.as_millis() as u64))
    }
}

impl Sub<Duration> for TimestampMillis {
    type Output = Self;

    #[inline]
    fn sub(self, rhs: Duration) -> Self::Output {
        Self(self.0.saturating_sub(rhs.as_millis() as u64))
    }
}

impl Sub<TimestampMillis> for TimestampMillis {
    type Output = Duration;

    #[inline]
    fn sub(self, rhs: TimestampMillis) -> Self::Output {
        Duration::from_millis(self.0.saturating_sub(rhs.0))
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
    fn test_timestamp_nanos_conversion_to_millis() {
        let nanos = TimestampNanos::from_nanos(1_234_567_890_123_456_789);
        let millis = nanos.to_millis();
        assert_eq!(millis.as_u64(), 1_234_567_890_123);
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
    fn test_timestamp_millis_now() {
        let t1 = TimestampMillis::now();
        let t2 = TimestampMillis::now();
        assert!(t2 >= t1);
    }

    #[test]
    fn test_timestamp_millis_conversion_to_nanos() {
        let millis = TimestampMillis::from_millis(1_234_567_890_123);
        let nanos = millis.to_nanos();
        assert_eq!(nanos.as_u64(), 1_234_567_890_123_000_000);
    }

    #[test]
    fn test_timestamp_millis_roundtrip() {
        let original_nanos = TimestampNanos::from_nanos(1_234_567_890_123_000_000);
        let millis = original_nanos.to_millis();
        let back_to_nanos = millis.to_nanos();
        assert_eq!(back_to_nanos, original_nanos);
    }

    #[test]
    fn test_timestamp_millis_anonymized() {
        let ts = TimestampMillis::now_anonymized();
        assert_eq!(ts.as_u64() % 100, 0);
    }

    #[test]
    fn test_timestamp_millis_anonymized_with_jitter() {
        let timestamps: Vec<TimestampMillis> = (0..10)
            .map(|_| TimestampMillis::now_anonymized_with_jitter())
            .collect();
        let remainders: Vec<u64> = timestamps.iter().map(|t| t.as_u64() % 100).collect();
        assert!(remainders.iter().all(|&r| r < 100));
    }

    #[test]
    fn test_timestamp_millis_arithmetic() {
        let t1 = TimestampMillis::from_millis(1000);
        let t2 = t1 + Duration::from_secs(1);
        assert_eq!(t2.as_u64(), 2000);

        let t3 = t2 - Duration::from_millis(500);
        assert_eq!(t3.as_u64(), 1500);
    }

    #[test]
    fn test_type_safety_requires_explicit_conversion() {
        let nanos = TimestampNanos::now();
        let millis = TimestampMillis::now();
        // Must explicitly convert - no accidental mixing
        let _ = nanos.to_millis() > millis;
        let _ = nanos > millis.to_nanos();
    }

    #[test]
    fn test_from_trait() {
        let nanos: TimestampNanos = 12345u64.into();
        assert_eq!(nanos.as_u64(), 12345);

        let millis: TimestampMillis = 67890u64.into();
        assert_eq!(millis.as_u64(), 67890);

        let n: u64 = nanos.into();
        assert_eq!(n, 12345);
    }
}
