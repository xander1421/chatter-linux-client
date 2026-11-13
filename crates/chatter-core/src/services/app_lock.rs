//! App lock: a local PIN in front of the UI, plus an optional duress PIN.
//!
//! Follows Signal-Android's screen lock for UX and timing
//! (`service/KeyCachingService.java` `startTimeoutIfAppropriate`): the app
//! locks on launch, and again once it has been in the background for the
//! chosen timeout. Signal arms an `ELAPSED_REALTIME` alarm on backgrounding;
//! this computes the same thing lazily from the moment the app was hidden, so
//! no timer has to survive Doze. While locked, notifications carry no sender
//! and no content (`notifications/v2/NotificationItem.kt` `getPrimaryTextActual`
//! returns "Locked message" while `KeyCachingService.isLocked`).
//!
//! Signal delegates the secret itself to the OS keyguard. Chatter keeps its
//! own PIN so it can offer a duress PIN (owner decision 2026-11-13): entering
//! it unlocks into a full local wipe instead of the app.
//!
//! # What is stored
//!
//! One settings row ([`keys::APP_LOCK`]) inside the SQLCipher database:
//! a random salt, `Argon2id(pin, salt)` for the lock PIN and, when set, for the
//! duress PIN (same salt, so every attempt costs exactly one KDF run whether or
//! not a duress PIN exists), and the failed-attempt counter. Parameters are the
//! SVR PIN's ([`crate::crypto::svr::get_argon2_params`]: 128 MiB, t=8, p=4).
//!
//! # What this is not
//!
//! A UI gate, not encryption: the database is opened at startup with the
//! keyring/Keystore key as before, so a process that can read app memory
//! while locked sees everything. That is Signal-Android's screen-lock model
//! too (its passphrase mode is the separate, legacy, encrypting one).
//!
//! # Clock
//!
//! The background timeout and the attempt backoff use wall-clock time
//! (`SystemTime`), because `Instant` (CLOCK_MONOTONIC) stops while an Android
//! device sleeps and would keep the app unlocked across a night in a pocket.
//! A clock that moves backwards fails closed: the app locks, and a backoff
//! restarts from now. A clock moved *forward* by someone holding the device
//! shortens a backoff that spans a restart; inside one process the backoff is
//! also held against `Instant`, so that needs a restart per attempt.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::error::StorageError;
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};

/// Shortest lock or duress PIN accepted.
pub const MIN_APP_LOCK_PIN_LENGTH: usize = 6;
/// Longest lock or duress PIN accepted (bounds the KDF input).
pub const MAX_APP_LOCK_PIN_LENGTH: usize = 64;
/// Wrong attempts allowed before the backoff starts.
const FREE_ATTEMPTS: u32 = 5;
/// The first backoff; each further failure doubles it.
const FIRST_BACKOFF: Duration = Duration::from_secs(30);
/// Ceiling on a single backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

/// How long the app may sit in the background before it locks again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockTimeout {
    Immediate,
    OneMinute,
    FiveMinutes,
    ThirtyMinutes,
}

impl LockTimeout {
    pub const ALL: [LockTimeout; 4] = [
        LockTimeout::Immediate,
        LockTimeout::OneMinute,
        LockTimeout::FiveMinutes,
        LockTimeout::ThirtyMinutes,
    ];

    pub fn duration(self) -> Duration {
        match self {
            LockTimeout::Immediate => Duration::ZERO,
            LockTimeout::OneMinute => Duration::from_secs(60),
            LockTimeout::FiveMinutes => Duration::from_secs(5 * 60),
            LockTimeout::ThirtyMinutes => Duration::from_secs(30 * 60),
        }
    }

    /// Stable key for settings UIs (matches the serde form).
    pub fn key(self) -> &'static str {
        match self {
            LockTimeout::Immediate => "immediate",
            LockTimeout::OneMinute => "one_minute",
            LockTimeout::FiveMinutes => "five_minutes",
            LockTimeout::ThirtyMinutes => "thirty_minutes",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.key() == key)
    }
}

/// What the settings screen shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppLockSettings {
    pub enabled: bool,
    pub timeout: LockTimeout,
    pub duress_enabled: bool,
}

/// Which stored PIN an attempt matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinMatch {
    Lock,
    Duress,
    None,
}

/// The outcome of one attempt on the lock screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockAttempt {
    /// Right PIN; the app is unlocked.
    Unlocked,
    /// The duress PIN. The caller wipes; the app stays locked meanwhile.
    Duress,
    /// Wrong PIN. `retry_after` is the backoff it started, if any.
    Wrong { retry_after: Option<Duration> },
    /// Still inside a backoff; no PIN was checked.
    Throttled { retry_after: Duration },
}

#[derive(Debug, thiserror::Error)]
pub enum AppLockError {
    #[error("PIN must be {MIN_APP_LOCK_PIN_LENGTH} to {MAX_APP_LOCK_PIN_LENGTH} characters")]
    InvalidPin,
    #[error("the two PINs do not match")]
    ConfirmationMismatch,
    #[error("the app lock is not set up")]
    NotEnabled,
    #[error("wrong PIN")]
    WrongPin,
    #[error("too many wrong attempts; try again in {} s", .0.as_secs().max(1))]
    Throttled(Duration),
    #[error("the duress PIN must differ from the lock PIN")]
    DuressEqualsLockPin,
    #[error("the duress PIN must differ from your account PIN")]
    DuressEqualsAccountPin,
    #[error("PIN derivation failed: {0}")]
    Kdf(String),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// The persisted record. Absent means the lock is off.
#[derive(Clone, Serialize, Deserialize)]
struct StoredLock {
    timeout: LockTimeout,
    salt: [u8; 16],
    lock_hash: [u8; 32],
    #[serde(default)]
    duress_hash: Option<[u8; 32]>,
    #[serde(default)]
    failed_attempts: u32,
    /// Unix seconds of the last wrong attempt.
    #[serde(default)]
    last_failure: u64,
}

/// In-memory lock state; never persisted (a fresh process starts locked).
struct Runtime {
    locked: bool,
    /// When the app left the screen, while it is off screen.
    hidden_since: Option<SystemTime>,
    /// In-process backoff floor, immune to wall-clock changes.
    throttled_until: Option<Instant>,
    /// Set once a duress wipe began: nothing unlocks this process again.
    wiping: bool,
}

/// The app lock. One per client; shared with the notification service.
pub struct AppLock {
    settings: Arc<SettingsRepository>,
    runtime: Mutex<Runtime>,
    /// Serialises attempts so two taps cannot both pass a backoff check.
    attempt: Mutex<()>,
}

/// Backoff after `failed` wrong attempts in a row.
fn backoff(failed: u32) -> Duration {
    if failed < FREE_ATTEMPTS {
        return Duration::ZERO;
    }
    let doublings = (failed - FREE_ATTEMPTS).min(16);
    FIRST_BACKOFF
        .saturating_mul(1u32 << doublings)
        .min(MAX_BACKOFF)
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn validate(pin: &str) -> Result<(), AppLockError> {
    let len = pin.chars().count();
    if !(MIN_APP_LOCK_PIN_LENGTH..=MAX_APP_LOCK_PIN_LENGTH).contains(&len) {
        return Err(AppLockError::InvalidPin);
    }
    Ok(())
}

fn derive(pin: &str, salt: &[u8; 16]) -> Result<Zeroizing<[u8; 32]>, AppLockError> {
    let argon2 = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        crate::crypto::svr::get_argon2_params(),
    );
    let mut out = Zeroizing::new([0u8; 32]);
    argon2
        .hash_password_into(pin.as_bytes(), salt, out.as_mut())
        .map_err(|e| AppLockError::Kdf(e.to_string()))?;
    Ok(out)
}

fn random_salt() -> Result<[u8; 16], AppLockError> {
    use rand::TryRngCore;
    let mut salt = [0u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut salt)
        .map_err(|e| AppLockError::Kdf(format!("OS CSPRNG failed: {e}")))?;
    Ok(salt)
}

impl StoredLock {
    /// Constant-time check against both stored hashes. Both comparisons always
    /// run (against a zero hash when there is no duress PIN), so the duress
    /// path costs the same as any other attempt.
    fn classify(&self, derived: &[u8; 32]) -> PinMatch {
        let is_lock = derived.ct_eq(&self.lock_hash);
        let duress_hash = self.duress_hash.unwrap_or([0u8; 32]);
        let has_duress = subtle::Choice::from(u8::from(self.duress_hash.is_some()));
        let is_duress = derived.ct_eq(&duress_hash) & has_duress;
        if bool::from(is_lock) {
            PinMatch::Lock
        } else if bool::from(is_duress) {
            PinMatch::Duress
        } else {
            PinMatch::None
        }
    }

    /// Remaining backoff at `now`, failing closed on a clock that went back.
    fn remaining_backoff(&self, now: SystemTime) -> Duration {
        let wait = backoff(self.failed_attempts);
        if wait.is_zero() {
            return Duration::ZERO;
        }
        let now = unix_secs(now);
        match now.checked_sub(self.last_failure) {
            Some(elapsed) => wait.saturating_sub(Duration::from_secs(elapsed)),
            None => wait,
        }
    }
}

impl AppLock {
    /// Load the lock. A process starts locked whenever the lock is enabled.
    pub fn new(settings: Arc<SettingsRepository>) -> Self {
        let lock = Self {
            settings,
            runtime: Mutex::new(Runtime {
                locked: false,
                hidden_since: None,
                throttled_until: None,
                wiping: false,
            }),
            attempt: Mutex::new(()),
        };
        // A row that cannot be read fails closed: locked.
        let locked = !matches!(lock.stored(), Ok(None));
        lock.runtime.lock().locked = locked;
        lock
    }

    fn stored(&self) -> Result<Option<StoredLock>, StorageError> {
        self.settings.get::<StoredLock>(keys::APP_LOCK)
    }

    fn store(&self, stored: &StoredLock) -> Result<(), StorageError> {
        self.settings.set(keys::APP_LOCK, stored)
    }

    pub fn settings(&self) -> Result<AppLockSettings, StorageError> {
        Ok(match self.stored()? {
            Some(s) => AppLockSettings {
                enabled: true,
                timeout: s.timeout,
                duress_enabled: s.duress_hash.is_some(),
            },
            None => AppLockSettings {
                enabled: false,
                timeout: LockTimeout::OneMinute,
                duress_enabled: false,
            },
        })
    }

    /// Whether the UI must show the lock screen (and notifications must hide
    /// sender and content).
    pub fn is_locked(&self) -> bool {
        self.is_locked_at(SystemTime::now())
    }

    pub(crate) fn is_locked_at(&self, now: SystemTime) -> bool {
        let mut runtime = self.runtime.lock();
        if runtime.locked || runtime.wiping {
            return true;
        }
        let Some(hidden) = runtime.hidden_since else {
            return false;
        };
        let timeout = match self.stored() {
            Ok(Some(s)) => s.timeout.duration(),
            Ok(None) => return false,
            Err(_) => Duration::ZERO,
        };
        let expired = match now.duration_since(hidden) {
            Ok(elapsed) => elapsed >= timeout,
            Err(_) => true,
        };
        if expired {
            runtime.locked = true;
        }
        expired
    }

    /// Platform lifecycle: the app left or returned to the screen.
    pub fn set_visible(&self, visible: bool) {
        self.set_visible_at(visible, SystemTime::now());
    }

    pub(crate) fn set_visible_at(&self, visible: bool, now: SystemTime) {
        if visible {
            // Fold an expired timeout into `locked` before forgetting when the
            // app was hidden.
            let _ = self.is_locked_at(now);
            self.runtime.lock().hidden_since = None;
        } else {
            let mut runtime = self.runtime.lock();
            if runtime.hidden_since.is_none() {
                runtime.hidden_since = Some(now);
            }
        }
    }

    /// One attempt from the lock screen.
    pub fn attempt_unlock(&self, pin: &str) -> Result<UnlockAttempt, AppLockError> {
        self.attempt_unlock_at(pin, SystemTime::now())
    }

    pub(crate) fn attempt_unlock_at(
        &self,
        pin: &str,
        now: SystemTime,
    ) -> Result<UnlockAttempt, AppLockError> {
        let _attempt = self.attempt.lock();
        if self.runtime.lock().wiping {
            return Ok(UnlockAttempt::Throttled {
                retry_after: MAX_BACKOFF,
            });
        }
        let Some(mut stored) = self.stored()? else {
            self.runtime.lock().locked = false;
            return Ok(UnlockAttempt::Unlocked);
        };
        if let Some(wait) = self.throttle(&stored, now) {
            return Ok(UnlockAttempt::Throttled { retry_after: wait });
        }
        // A PIN outside the accepted shape still pays for the KDF, so length
        // is not observable from timing.
        let derived = derive(pin, &stored.salt)?;
        let matched = if validate(pin).is_ok() {
            stored.classify(&derived)
        } else {
            PinMatch::None
        };
        match matched {
            PinMatch::Lock => {
                if stored.failed_attempts != 0 {
                    stored.failed_attempts = 0;
                    stored.last_failure = 0;
                    self.store(&stored)?;
                }
                let mut runtime = self.runtime.lock();
                runtime.locked = false;
                runtime.hidden_since = None;
                runtime.throttled_until = None;
                Ok(UnlockAttempt::Unlocked)
            }
            PinMatch::Duress => {
                self.runtime.lock().wiping = true;
                Ok(UnlockAttempt::Duress)
            }
            PinMatch::None => {
                let retry_after = self.record_failure(&mut stored, now)?;
                Ok(UnlockAttempt::Wrong { retry_after })
            }
        }
    }

    /// The remaining backoff, if one is running.
    fn throttle(&self, stored: &StoredLock, now: SystemTime) -> Option<Duration> {
        let wall = stored.remaining_backoff(now);
        let mono = self
            .runtime
            .lock()
            .throttled_until
            .map(|until| until.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
        let wait = wall.max(mono);
        (!wait.is_zero()).then_some(wait)
    }

    fn record_failure(
        &self,
        stored: &mut StoredLock,
        now: SystemTime,
    ) -> Result<Option<Duration>, AppLockError> {
        stored.failed_attempts = stored.failed_attempts.saturating_add(1);
        stored.last_failure = unix_secs(now);
        self.store(stored)?;
        let wait = backoff(stored.failed_attempts);
        if wait.is_zero() {
            return Ok(None);
        }
        self.runtime.lock().throttled_until = Some(Instant::now() + wait);
        Ok(Some(wait))
    }

    /// Check `pin` against the lock PIN for a settings change. Counts toward
    /// the same backoff as the lock screen. The duress PIN never matches here.
    fn require_lock_pin(&self, pin: &str, now: SystemTime) -> Result<StoredLock, AppLockError> {
        let _attempt = self.attempt.lock();
        let mut stored = self.stored()?.ok_or(AppLockError::NotEnabled)?;
        if let Some(wait) = self.throttle(&stored, now) {
            return Err(AppLockError::Throttled(wait));
        }
        let derived = derive(pin, &stored.salt)?;
        if validate(pin).is_ok() && stored.classify(&derived) == PinMatch::Lock {
            if stored.failed_attempts != 0 {
                stored.failed_attempts = 0;
                stored.last_failure = 0;
                self.store(&stored)?;
            }
            return Ok(stored);
        }
        self.record_failure(&mut stored, now)?;
        Err(AppLockError::WrongPin)
    }

    /// Turn the lock on (or replace the PIN when it is already on, after the
    /// current one). The duress PIN is dropped on a PIN change: it was checked
    /// against the old lock PIN only.
    pub fn enable(
        &self,
        current: Option<&str>,
        pin: &str,
        confirm: &str,
        timeout: LockTimeout,
    ) -> Result<(), AppLockError> {
        validate(pin)?;
        if pin != confirm {
            return Err(AppLockError::ConfirmationMismatch);
        }
        if self.stored()?.is_some() {
            self.require_lock_pin(current.unwrap_or_default(), SystemTime::now())?;
        }
        let salt = random_salt()?;
        let lock_hash = *derive(pin, &salt)?;
        self.store(&StoredLock {
            timeout,
            salt,
            lock_hash,
            duress_hash: None,
            failed_attempts: 0,
            last_failure: 0,
        })?;
        Ok(())
    }

    /// Turn the lock off. Drops the duress PIN with it.
    pub fn disable(&self, current: &str) -> Result<(), AppLockError> {
        self.require_lock_pin(current, SystemTime::now())?;
        self.settings.delete(keys::APP_LOCK)?;
        let mut runtime = self.runtime.lock();
        runtime.locked = false;
        runtime.hidden_since = None;
        Ok(())
    }

    pub fn set_timeout(&self, timeout: LockTimeout) -> Result<(), AppLockError> {
        let mut stored = self.stored()?.ok_or(AppLockError::NotEnabled)?;
        stored.timeout = timeout;
        self.store(&stored)?;
        Ok(())
    }

    /// Set the duress PIN. `differs_from_account_pin` is the caller's check
    /// against the SVR/registration PIN (it needs the account's salt).
    pub fn set_duress_pin(
        &self,
        current: &str,
        duress: &str,
        confirm: &str,
        differs_from_account_pin: impl FnOnce(&str) -> bool,
    ) -> Result<(), AppLockError> {
        validate(duress)?;
        if duress != confirm {
            return Err(AppLockError::ConfirmationMismatch);
        }
        if duress == current {
            return Err(AppLockError::DuressEqualsLockPin);
        }
        let mut stored = self.require_lock_pin(current, SystemTime::now())?;
        if !differs_from_account_pin(duress) {
            return Err(AppLockError::DuressEqualsAccountPin);
        }
        stored.duress_hash = Some(*derive(duress, &stored.salt)?);
        self.store(&stored)?;
        Ok(())
    }

    pub fn clear_duress_pin(&self, current: &str) -> Result<(), AppLockError> {
        let mut stored = self.require_lock_pin(current, SystemTime::now())?;
        stored.duress_hash = None;
        self.store(&stored)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;

    fn lock() -> (AppLock, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("lock.db"), "testpass").unwrap();
        db.run_migrations().unwrap();
        let settings = Arc::new(SettingsRepository::new(Arc::new(db)));
        (AppLock::new(settings), dir)
    }

    const PIN: &str = "482916";
    const DURESS: &str = "731052";

    #[test]
    fn disabled_lock_never_locks() {
        let (lock, _dir) = lock();
        assert!(!lock.is_locked());
        lock.set_visible(false);
        assert!(!lock.is_locked());
        assert!(!lock.settings().unwrap().enabled);
    }

    #[test]
    fn kdf_hash_verifies_and_is_salted() {
        let (lock, _dir) = lock();
        lock.enable(None, PIN, PIN, LockTimeout::Immediate).unwrap();
        let stored = lock.stored().unwrap().unwrap();
        assert_ne!(stored.lock_hash, [0u8; 32]);
        // The raw PIN is nowhere in the stored row.
        let raw: serde_json::Value = lock.settings.get(keys::APP_LOCK).unwrap().unwrap();
        assert!(!raw.to_string().contains(PIN));
        assert_eq!(*derive(PIN, &stored.salt).unwrap(), stored.lock_hash);
        assert_ne!(*derive(PIN, &[9u8; 16]).unwrap(), stored.lock_hash);
        // Turning the lock on does not lock the running app; leaving it does.
        assert!(!lock.is_locked());
        lock.set_visible(false);
        lock.set_visible(true);
        assert_eq!(
            lock.attempt_unlock("000000").unwrap(),
            UnlockAttempt::Wrong { retry_after: None }
        );
        assert!(lock.is_locked());
        assert_eq!(lock.attempt_unlock(PIN).unwrap(), UnlockAttempt::Unlocked);
        assert!(!lock.is_locked());
    }

    #[test]
    fn a_new_process_starts_locked() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("l.db"), "p").unwrap());
        db.run_migrations().unwrap();
        let settings = Arc::new(SettingsRepository::new(db));
        AppLock::new(settings.clone())
            .enable(None, PIN, PIN, LockTimeout::ThirtyMinutes)
            .unwrap();
        assert!(AppLock::new(settings).is_locked());
    }

    #[test]
    fn background_timeout_relocks_and_clock_rollback_fails_closed() {
        let (lock, _dir) = lock();
        lock.enable(None, PIN, PIN, LockTimeout::OneMinute).unwrap();
        lock.attempt_unlock(PIN).unwrap();
        let t0 = SystemTime::now();
        lock.set_visible_at(false, t0);
        assert!(!lock.is_locked_at(t0 + Duration::from_secs(59)));
        lock.set_visible_at(true, t0 + Duration::from_secs(30));
        assert!(!lock.is_locked_at(t0 + Duration::from_secs(30)));

        lock.set_visible_at(false, t0 + Duration::from_secs(40));
        assert!(lock.is_locked_at(t0 + Duration::from_secs(101)));
        // Stays locked after coming back.
        lock.set_visible_at(true, t0 + Duration::from_secs(102));
        assert!(lock.is_locked());

        lock.attempt_unlock(PIN).unwrap();
        lock.set_visible_at(false, t0);
        assert!(lock.is_locked_at(t0 - Duration::from_secs(5)));
    }

    #[test]
    fn immediate_timeout_locks_on_hide() {
        let (lock, _dir) = lock();
        lock.enable(None, PIN, PIN, LockTimeout::Immediate).unwrap();
        lock.attempt_unlock(PIN).unwrap();
        lock.set_visible(false);
        assert!(lock.is_locked());
    }

    #[test]
    fn throttling_backs_off_exponentially_and_survives_a_restart() {
        assert_eq!(backoff(4), Duration::ZERO);
        assert_eq!(backoff(5), Duration::from_secs(30));
        assert_eq!(backoff(6), Duration::from_secs(60));
        assert_eq!(backoff(7), Duration::from_secs(120));
        assert_eq!(backoff(40), MAX_BACKOFF);

        let (lock, _dir) = lock();
        lock.enable(None, PIN, PIN, LockTimeout::Immediate).unwrap();
        let t0 = SystemTime::now();
        for _ in 0..4 {
            assert_eq!(
                lock.attempt_unlock_at("111111", t0).unwrap(),
                UnlockAttempt::Wrong { retry_after: None }
            );
        }
        assert_eq!(
            lock.attempt_unlock_at("111111", t0).unwrap(),
            UnlockAttempt::Wrong {
                retry_after: Some(Duration::from_secs(30))
            }
        );
        // Even the right PIN is not checked inside the backoff.
        assert!(matches!(
            lock.attempt_unlock_at(PIN, t0 + Duration::from_secs(10)).unwrap(),
            UnlockAttempt::Throttled { .. }
        ));

        // A restarted process keeps the counter (wall clock), and a clock set
        // back restarts the full wait.
        let restarted = AppLock::new(lock.settings.clone());
        assert_eq!(
            restarted.attempt_unlock_at(PIN, t0 + Duration::from_secs(20)).unwrap(),
            UnlockAttempt::Throttled {
                retry_after: Duration::from_secs(10)
            }
        );
        assert_eq!(
            restarted.attempt_unlock_at(PIN, t0 - Duration::from_secs(100)).unwrap(),
            UnlockAttempt::Throttled {
                retry_after: Duration::from_secs(30)
            }
        );
        assert_eq!(
            restarted.attempt_unlock_at(PIN, t0 + Duration::from_secs(31)).unwrap(),
            UnlockAttempt::Unlocked
        );
        assert_eq!(restarted.stored().unwrap().unwrap().failed_attempts, 0);
    }

    #[test]
    fn duress_pin_is_recognised_and_ends_unlocking() {
        let (lock, _dir) = lock();
        lock.enable(None, PIN, PIN, LockTimeout::Immediate).unwrap();
        lock.set_duress_pin(PIN, DURESS, DURESS, |_| true).unwrap();
        assert!(lock.settings().unwrap().duress_enabled);
        assert_eq!(lock.attempt_unlock(DURESS).unwrap(), UnlockAttempt::Duress);
        assert!(lock.is_locked());
        // Nothing unlocks a process whose wipe has begun.
        assert!(matches!(
            lock.attempt_unlock(PIN).unwrap(),
            UnlockAttempt::Throttled { .. }
        ));
    }

    #[test]
    fn duress_pin_rules() {
        let (lock, _dir) = lock();
        assert!(matches!(
            lock.set_duress_pin(PIN, DURESS, DURESS, |_| true),
            Err(AppLockError::NotEnabled)
        ));
        lock.enable(None, PIN, PIN, LockTimeout::Immediate).unwrap();
        assert!(matches!(
            lock.set_duress_pin(PIN, PIN, PIN, |_| true),
            Err(AppLockError::DuressEqualsLockPin)
        ));
        assert!(matches!(
            lock.set_duress_pin(PIN, DURESS, "731053", |_| true),
            Err(AppLockError::ConfirmationMismatch)
        ));
        assert!(matches!(
            lock.set_duress_pin(PIN, DURESS, DURESS, |_| false),
            Err(AppLockError::DuressEqualsAccountPin)
        ));
        assert!(matches!(
            lock.set_duress_pin("999999", DURESS, DURESS, |_| true),
            Err(AppLockError::WrongPin)
        ));
        assert!(!lock.settings().unwrap().duress_enabled);
        // A duress PIN never authorises a settings change.
        lock.set_duress_pin(PIN, DURESS, DURESS, |_| true).unwrap();
        assert!(matches!(lock.disable(DURESS), Err(AppLockError::WrongPin)));
        // Changing the lock PIN drops the duress PIN.
        lock.enable(Some(PIN), "555123", "555123", LockTimeout::Immediate)
            .unwrap();
        assert!(!lock.settings().unwrap().duress_enabled);
        lock.disable("555123").unwrap();
        assert!(!lock.settings().unwrap().enabled);
        assert!(!lock.is_locked());
    }

    #[test]
    fn timeout_keys_round_trip_and_match_serde() {
        for t in LockTimeout::ALL {
            assert_eq!(LockTimeout::from_key(t.key()), Some(t));
            assert_eq!(serde_json::to_string(&t).unwrap(), format!("\"{}\"", t.key()));
        }
        assert_eq!(LockTimeout::from_key("never"), None);
    }

    #[test]
    fn pin_shape_is_enforced() {
        let (lock, _dir) = lock();
        assert!(matches!(
            lock.enable(None, "12345", "12345", LockTimeout::Immediate),
            Err(AppLockError::InvalidPin)
        ));
        assert!(matches!(
            lock.enable(None, PIN, "482917", LockTimeout::Immediate),
            Err(AppLockError::ConfirmationMismatch)
        ));
    }
}
