//! App lock: a local PIN in front of the UI, plus an optional duress PIN.
//!
//! Follows Signal-Android's screen lock for UX and timing
//! (`service/KeyCachingService.java` `startTimeoutIfAppropriate`): the app
//! locks on launch, and again once it has been in the background for the
//! chosen timeout. The lock computes the deadline from when the app was
//! hidden. The notification service also watches the deadline while the app
//! is in the background. While locked, notifications carry no sender
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
//! The background timeout uses wall-clock time (`SystemTime`), because
//! `Instant` (CLOCK_MONOTONIC) stops while an Android device sleeps and would
//! keep the app unlocked across a night in a pocket. Any clock change while the
//! app is hidden only locks it sooner: backwards fails closed, forwards expires
//! the timeout.
//!
//! The attempt backoff must not let a clock change *shorten* it, so it never
//! reads wall time. It stores [`boot_clock`] (time since boot, counting
//! suspend; the settings app and `date` cannot move it) at the last failure.
//! Within one boot, now minus that is the real time elapsed. After a reboot the
//! clock restarted, so the difference, or the current reading alone when it is
//! smaller, undercounts the real time elapsed: a reboot never shortens a
//! backoff, it can only make the holder wait longer. A clock read that fails
//! counts as a full backoff: no PIN is checked.
//!
//! Accepted residual (Linux desktop): an unprivileged process can start the app
//! in a time namespace (`unshare -Ur -T --boottime <offset>`), which shifts
//! `CLOCK_BOOTTIME` forward and skips the backoff, one relaunch per attempt.
//! That needs code running as the user, which already reaches the database key
//! through the keyring and could `LD_PRELOAD` the clock anyway; reading
//! `/proc/self/timens_offsets` would not hold either, since the same user can
//! mount over it. Android apps cannot create time namespaces.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::sync::watch;
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
    /// [`boot_clock`] milliseconds at the last wrong attempt.
    #[serde(default)]
    last_failure_boot_ms: u64,
}

/// In-memory lock state; never persisted (a fresh process starts locked).
struct Runtime {
    locked: bool,
    lock_generation: u64,
    /// When the app left the screen, while it is off screen.
    hidden_since: Option<SystemTime>,
    /// Set once a duress wipe began: nothing unlocks this process again.
    wiping: bool,
}

/// The app lock. One per client; shared with the notification service.
pub struct AppLock {
    settings: Arc<SettingsRepository>,
    runtime: Mutex<Runtime>,
    changes: watch::Sender<()>,
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

/// Time since boot, counting suspend, from a clock the device holder cannot set
/// (module docs, "Clock"). `None` if the read failed.
fn boot_clock() -> Option<Duration> {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        const CLOCK: libc::clockid_t = libc::CLOCK_BOOTTIME;
        // Apple's CLOCK_MONOTONIC keeps counting while the device sleeps.
        #[cfg(target_vendor = "apple")]
        const CLOCK: libc::clockid_t = libc::CLOCK_MONOTONIC;
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `ts` is a valid, writable timespec.
        if unsafe { libc::clock_gettime(CLOCK, &mut ts) } != 0 {
            return None;
        }
        Some(Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
    }
    #[cfg(target_os = "windows")]
    {
        // Milliseconds since boot, sleep and hibernation included.
        Some(Duration::from_millis(unsafe {
            windows::Win32::System::SystemInformation::GetTickCount64()
        }))
    }
}

/// The boot clock for an attempt; a failed read throttles instead of letting
/// a stamp of zero (or a "now" of zero) skew the backoff either way.
fn attempt_clock(now: Option<Duration>) -> Result<Duration, AppLockError> {
    now.ok_or(AppLockError::Throttled(MAX_BACKOFF))
}

fn as_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
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

    /// Remaining backoff at boot-clock time `now`. The elapsed time is a lower
    /// bound on the real one even across a reboot (module docs, "Clock").
    fn remaining_backoff(&self, now: Duration) -> Duration {
        let now = as_ms(now);
        let elapsed = now.checked_sub(self.last_failure_boot_ms).unwrap_or(now);
        backoff(self.failed_attempts).saturating_sub(Duration::from_millis(elapsed))
    }
}

/// The remaining backoff, if one is running.
fn throttle(stored: &StoredLock, now: Duration) -> Option<Duration> {
    let wait = stored.remaining_backoff(now);
    (!wait.is_zero()).then_some(wait)
}

impl AppLock {
    /// Load the lock. A process starts locked whenever the lock is enabled.
    pub fn new(settings: Arc<SettingsRepository>) -> Self {
        let lock = Self {
            settings,
            runtime: Mutex::new(Runtime {
                locked: false,
                lock_generation: 0,
                hidden_since: None,
                wiping: false,
            }),
            changes: watch::channel(()).0,
            attempt: Mutex::new(()),
        };
        // A row that cannot be read fails closed: locked.
        let locked = !matches!(lock.stored(), Ok(None));
        if locked {
            lock.engage(&mut lock.runtime.lock());
        }
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
        self.notification_lock_state_at(|| now).0
    }

    pub(crate) fn notification_lock_state(&self) -> (bool, u64, Option<Duration>) {
        self.notification_lock_state_at(SystemTime::now)
    }

    pub(crate) fn subscribe_to_changes(&self) -> watch::Receiver<()> {
        self.changes.subscribe()
    }

    fn notification_lock_state_at(
        &self,
        now: impl FnOnce() -> SystemTime,
    ) -> (bool, u64, Option<Duration>) {
        let mut runtime = self.runtime.lock();
        let remaining = self.update_timeout(&mut runtime, now());
        (
            runtime.locked || runtime.wiping,
            runtime.lock_generation,
            remaining,
        )
    }

    fn engage(&self, runtime: &mut Runtime) {
        if !runtime.locked {
            runtime.locked = true;
            runtime.lock_generation += 1;
            self.changes.send_replace(());
        }
    }

    fn update_timeout(&self, runtime: &mut Runtime, now: SystemTime) -> Option<Duration> {
        if runtime.locked || runtime.wiping {
            return None;
        }
        let hidden = runtime.hidden_since?;
        let timeout = match self.stored() {
            Ok(Some(s)) => s.timeout.duration(),
            Ok(None) => return None,
            Err(_) => Duration::ZERO,
        };
        if let Ok(elapsed) = now.duration_since(hidden)
            && let Some(remaining) = timeout.checked_sub(elapsed).filter(|d| !d.is_zero())
        {
            return Some(remaining);
        }
        self.engage(runtime);
        None
    }

    /// Platform lifecycle: the app left or returned to the screen.
    pub fn set_visible(&self, visible: bool) {
        self.set_visible_at(visible, SystemTime::now());
    }

    pub(crate) fn set_visible_at(&self, visible: bool, now: SystemTime) {
        let mut runtime = self.runtime.lock();
        if visible {
            // Fold an expired timeout into `locked` before forgetting when the
            // app was hidden.
            self.update_timeout(&mut runtime, now);
            runtime.hidden_since = None;
        } else if runtime.hidden_since.is_none() {
            runtime.hidden_since = Some(now);
            self.update_timeout(&mut runtime, now);
        }
        self.changes.send_replace(());
    }

    pub fn lock_now(&self) {
        let mut runtime = self.runtime.lock();
        if !matches!(self.stored(), Ok(None)) {
            self.engage(&mut runtime);
        }
    }

    /// One attempt from the lock screen.
    pub fn attempt_unlock(&self, pin: &str) -> Result<UnlockAttempt, AppLockError> {
        self.attempt_unlock_with(pin, boot_clock())
    }

    fn attempt_unlock_with(
        &self,
        pin: &str,
        now: Option<Duration>,
    ) -> Result<UnlockAttempt, AppLockError> {
        match attempt_clock(now) {
            Ok(now) => self.attempt_unlock_at(pin, now),
            Err(_) => Ok(UnlockAttempt::Throttled {
                retry_after: MAX_BACKOFF,
            }),
        }
    }

    /// `now` is a [`boot_clock`] reading.
    pub(crate) fn attempt_unlock_at(
        &self,
        pin: &str,
        now: Duration,
    ) -> Result<UnlockAttempt, AppLockError> {
        let _attempt = self.attempt.lock();
        if self.runtime.lock().wiping {
            return Ok(UnlockAttempt::Throttled {
                retry_after: MAX_BACKOFF,
            });
        }
        let Some(mut stored) = self.stored()? else {
            self.runtime.lock().locked = false;
            self.changes.send_replace(());
            return Ok(UnlockAttempt::Unlocked);
        };
        if let Some(wait) = throttle(&stored, now) {
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
                    stored.last_failure_boot_ms = 0;
                    self.store(&stored)?;
                }
                let mut runtime = self.runtime.lock();
                runtime.locked = false;
                runtime.hidden_since = None;
                self.changes.send_replace(());
                Ok(UnlockAttempt::Unlocked)
            }
            PinMatch::Duress => {
                let mut runtime = self.runtime.lock();
                runtime.wiping = true;
                self.engage(&mut runtime);
                Ok(UnlockAttempt::Duress)
            }
            PinMatch::None => {
                let retry_after = self.record_failure(&mut stored, now)?;
                Ok(UnlockAttempt::Wrong { retry_after })
            }
        }
    }

    fn record_failure(
        &self,
        stored: &mut StoredLock,
        now: Duration,
    ) -> Result<Option<Duration>, AppLockError> {
        stored.failed_attempts = stored.failed_attempts.saturating_add(1);
        stored.last_failure_boot_ms = as_ms(now);
        self.store(stored)?;
        let wait = backoff(stored.failed_attempts);
        Ok((!wait.is_zero()).then_some(wait))
    }

    /// Check `pin` against the lock PIN for a settings change. Counts toward
    /// the same backoff as the lock screen. The duress PIN never matches here.
    fn require_lock_pin(&self, pin: &str, now: Duration) -> Result<StoredLock, AppLockError> {
        let _attempt = self.attempt.lock();
        let mut stored = self.stored()?.ok_or(AppLockError::NotEnabled)?;
        if let Some(wait) = throttle(&stored, now) {
            return Err(AppLockError::Throttled(wait));
        }
        let derived = derive(pin, &stored.salt)?;
        if validate(pin).is_ok() && stored.classify(&derived) == PinMatch::Lock {
            if stored.failed_attempts != 0 {
                stored.failed_attempts = 0;
                stored.last_failure_boot_ms = 0;
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
            self.require_lock_pin(current.unwrap_or_default(), attempt_clock(boot_clock())?)?;
        }
        let salt = random_salt()?;
        let lock_hash = *derive(pin, &salt)?;
        self.store(&StoredLock {
            timeout,
            salt,
            lock_hash,
            duress_hash: None,
            failed_attempts: 0,
            last_failure_boot_ms: 0,
        })?;
        self.changes.send_replace(());
        Ok(())
    }

    /// Turn the lock off. Drops the duress PIN with it.
    pub fn disable(&self, current: &str) -> Result<(), AppLockError> {
        self.require_lock_pin(current, attempt_clock(boot_clock())?)?;
        self.settings.delete(keys::APP_LOCK)?;
        let mut runtime = self.runtime.lock();
        runtime.locked = false;
        runtime.hidden_since = None;
        self.changes.send_replace(());
        Ok(())
    }

    pub fn set_timeout(&self, timeout: LockTimeout) -> Result<(), AppLockError> {
        let mut stored = self.stored()?.ok_or(AppLockError::NotEnabled)?;
        stored.timeout = timeout;
        self.store(&stored)?;
        self.changes.send_replace(());
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
        let mut stored = self.require_lock_pin(current, attempt_clock(boot_clock())?)?;
        if !differs_from_account_pin(duress) {
            return Err(AppLockError::DuressEqualsAccountPin);
        }
        stored.duress_hash = Some(*derive(duress, &stored.salt)?);
        self.store(&stored)?;
        Ok(())
    }

    pub fn clear_duress_pin(&self, current: &str) -> Result<(), AppLockError> {
        let mut stored = self.require_lock_pin(current, attempt_clock(boot_clock())?)?;
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
        // Boot-clock readings, not wall time.
        let t0 = Duration::from_secs(1000);
        let at = |secs: u64| t0 + Duration::from_secs(secs);
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
            lock.attempt_unlock_at(PIN, at(10)).unwrap(),
            UnlockAttempt::Throttled { .. }
        ));

        // A restarted process keeps the counter and the boot-clock stamp.
        let restarted = AppLock::new(lock.settings.clone());
        assert_eq!(
            restarted.attempt_unlock_at(PIN, at(20)).unwrap(),
            UnlockAttempt::Throttled {
                retry_after: Duration::from_secs(10)
            }
        );
        assert_eq!(
            restarted.attempt_unlock_at(PIN, at(31)).unwrap(),
            UnlockAttempt::Unlocked
        );
        assert_eq!(restarted.stored().unwrap().unwrap().failed_attempts, 0);
    }

    #[test]
    fn wall_clock_and_reboot_never_shorten_the_backoff() {
        let (lock, _dir) = lock();
        lock.enable(None, PIN, PIN, LockTimeout::Immediate).unwrap();
        let t0 = Duration::from_secs(1000);
        for _ in 0..5 {
            lock.attempt_unlock_at("111111", t0).unwrap();
        }
        // The stored stamp is a boot-clock reading: nothing a wall-clock jump
        // reaches. The live entry point reads the real boot clock.
        let stored = lock.stored().unwrap().unwrap();
        assert_eq!(stored.last_failure_boot_ms, 1_000_000);
        let raw: serde_json::Value = lock.settings.get(keys::APP_LOCK).unwrap().unwrap();
        assert!(raw.get("last_failure").is_none());

        // Reboot: the boot clock restarted below the stamp. Only the time
        // since this boot counts, so 5 s in, 25 s remain.
        let rebooted = AppLock::new(lock.settings.clone());
        assert_eq!(
            rebooted.attempt_unlock_at(PIN, Duration::from_secs(5)).unwrap(),
            UnlockAttempt::Throttled {
                retry_after: Duration::from_secs(25)
            }
        );
        // A later boot whose clock passed the stamp counts only the
        // difference, which is less than the real time since the failure.
        assert_eq!(
            rebooted.attempt_unlock_at(PIN, t0 + Duration::from_secs(29)).unwrap(),
            UnlockAttempt::Throttled {
                retry_after: Duration::from_secs(1)
            }
        );
        assert_eq!(
            rebooted.attempt_unlock_at(PIN, t0 + Duration::from_secs(30)).unwrap(),
            UnlockAttempt::Unlocked
        );

        let a = boot_clock().unwrap();
        assert!(a > Duration::ZERO);
        assert!(boot_clock().unwrap() >= a);
    }

    #[test]
    fn a_failed_clock_read_throttles_without_checking_the_pin() {
        let (lock, _dir) = lock();
        lock.enable(None, PIN, PIN, LockTimeout::Immediate).unwrap();
        lock.lock_now();
        assert_eq!(
            lock.attempt_unlock_with(PIN, None).unwrap(),
            UnlockAttempt::Throttled {
                retry_after: MAX_BACKOFF
            }
        );
        assert_eq!(lock.attempt_unlock_with("111111", None).unwrap(), UnlockAttempt::Throttled {
            retry_after: MAX_BACKOFF
        });
        // Nothing was recorded, and the lock is still engaged.
        assert_eq!(lock.stored().unwrap().unwrap().failed_attempts, 0);
        assert!(lock.is_locked());
        assert!(matches!(attempt_clock(None), Err(AppLockError::Throttled(d)) if d == MAX_BACKOFF));
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
