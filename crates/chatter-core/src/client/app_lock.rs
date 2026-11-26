//! App lock facade and the one full local wipe. Logic lives in
//! `services::app_lock` (PIN, timeout, backoff) and `storage::local_wipe`.

use super::*;
use crate::error::StorageError;
use crate::services::app_lock::{AppLockError, AppLockSettings, LockTimeout, UnlockAttempt};

/// What the lock screen does after an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppUnlock {
    Unlocked,
    /// Wrong PIN; `retry_after_secs` is non-zero when a backoff started.
    Wrong { retry_after_secs: u64 },
    /// Inside a backoff; nothing was checked.
    Throttled { retry_after_secs: u64 },
    /// The duress PIN: local data is gone. The platform relaunches into the
    /// fresh-install screen.
    Wiped,
}

impl ChatterClient {
    pub fn app_lock_settings(&self) -> Result<AppLockSettings, StorageError> {
        self.services.app_lock.settings()
    }

    /// Whether the lock screen must cover the app now.
    pub fn app_lock_is_locked(&self) -> bool {
        self.services.app_lock.is_locked()
    }

    pub fn lock_app_now(&self) {
        self.services.app_lock.lock_now();
    }

    /// Turn the lock on, or change its PIN (`current` required then).
    pub async fn enable_app_lock(
        &self,
        current: Option<String>,
        pin: String,
        confirm: String,
        timeout: LockTimeout,
    ) -> Result<(), AppLockError> {
        let lock = self.services.app_lock.clone();
        blocking(move || lock.enable(current.as_deref(), &pin, &confirm, timeout)).await
    }

    pub async fn disable_app_lock(&self, current: String) -> Result<(), AppLockError> {
        let lock = self.services.app_lock.clone();
        blocking(move || lock.disable(&current)).await
    }

    pub fn set_app_lock_timeout(&self, timeout: LockTimeout) -> Result<(), AppLockError> {
        self.services.app_lock.set_timeout(timeout)
    }

    /// Set the duress PIN. It must differ from the lock PIN and from the
    /// account (SVR/registration) PIN; the latter is checked against this
    /// device's local PIN fingerprint, so it is enforced only where that
    /// fingerprint exists (a device that has proven the PIN once).
    pub async fn set_duress_pin(
        &self,
        current: String,
        duress: String,
        confirm: String,
    ) -> Result<(), AppLockError> {
        let lock = self.services.app_lock.clone();
        let link = self.link_device().clone();
        blocking(move || {
            lock.set_duress_pin(&current, &duress, &confirm, |candidate| {
                link.verify_pin(candidate) != crate::services::link_device_service::PinVerification::Match
            })
        })
        .await
    }

    pub async fn clear_duress_pin(&self, current: String) -> Result<(), AppLockError> {
        let lock = self.services.app_lock.clone();
        blocking(move || lock.clear_duress_pin(&current)).await
    }

    /// One attempt from the lock screen. The duress PIN runs the full local
    /// wipe before this returns; until then the caller shows the same
    /// "checking" state as for any attempt.
    pub async fn unlock_app(&self, pin: String) -> Result<AppUnlock, AppLockError> {
        let lock = self.services.app_lock.clone();
        let attempt = blocking(move || lock.attempt_unlock(&pin)).await?;
        Ok(match attempt {
            UnlockAttempt::Unlocked => AppUnlock::Unlocked,
            UnlockAttempt::Wrong { retry_after } => AppUnlock::Wrong {
                retry_after_secs: retry_after.map_or(0, |d| d.as_secs().max(1)),
            },
            UnlockAttempt::Throttled { retry_after } => AppUnlock::Throttled {
                retry_after_secs: retry_after.as_secs().max(1),
            },
            UnlockAttempt::Duress => {
                if let Err(error) = self.full_local_wipe().await {
                    // The keys go first, so a later step failing still leaves
                    // the data unreadable. Relaunch regardless.
                    tracing::error!(%error, "full local wipe finished with an error");
                }
                AppUnlock::Wiped
            }
        })
    }

    /// Destroy everything this install holds locally: keys first, then the
    /// mesh, then every row and file (`storage::local_wipe` lists the
    /// stores). The account stays on the server. The process must relaunch
    /// afterwards; this client is unusable.
    ///
    /// The single full wipe: the duress PIN is its caller.
    pub async fn full_local_wipe(&self) -> Result<(), StorageError> {
        // Nothing may write back: stop every session task first. No offline
        // presence is published (no network round trip before the wipe).
        {
            let _lifecycle = self.auth_lifecycle_lock.lock().await;
            self.auth_state().invalidate_session();
            self.shutdown_session(self.auth_state().get().session_revision, false)
                .await;
        }
        self.clear_composer_session();
        let directories = self.directories().clone();
        let database = self.services.database.clone();

        let mut result = crate::storage::local_wipe::destroy_keys(&directories);
        #[cfg(feature = "mesh")]
        if let Err(error) = self.mesh().panic_wipe() {
            tracing::error!(%error, "mesh panic wipe failed during the full wipe");
            result = result.and(Err(StorageError::InvalidData(format!("mesh wipe: {error}"))));
        }
        let rows_and_files = tokio::task::spawn_blocking(move || {
            crate::storage::local_wipe::wipe_rows_and_files(&database, &directories)
        })
        .await
        .map_err(|e| StorageError::InvalidData(format!("wipe task: {e}")))?;
        result = result.and(rows_and_files);
        tracing::warn!("full local wipe done");
        result
    }
}

/// Argon2id (128 MiB, ~0.5 s) off the async workers.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, AppLockError> + Send + 'static,
) -> Result<T, AppLockError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppLockError::Kdf(format!("app lock task: {e}")))?
}
