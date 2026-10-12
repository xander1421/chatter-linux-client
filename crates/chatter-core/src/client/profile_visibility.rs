//! "Who can see my photo and name" — Settings › Privacy.
//!
//! The policy (who may receive our profile key) is
//! [`crate::services::profile_visibility`]; this is the orchestration of a
//! change, which touches the SVR backup, our other devices, the server UAK
//! and the server profile.
//!
//! Loosening only starts sharing. Restricting cannot take the key back from
//! whoever already holds it, so it ROTATES the key (Signal rotates on block
//! for the same reason), in an order a crash can resume:
//!
//! 1. a fresh key is persisted as `PROFILE_KEY_NEXT` (a retry reuses it);
//! 2. the SVR backup is rewritten with it — first, because it is the one step
//!    that needs the PIN, which a resume after a crash would not have;
//! 3. commit: `PROFILE_KEY_ROTATION_PENDING` goes up, the setting is stored,
//!    the next key becomes current — from here nothing is shared with anyone;
//! 4. our other devices get the key (`ProfileKeySync`), so none keeps
//!    sharing or re-uploading under the old one;
//! 5. the server UAK moves to the new key;
//! 6. our profile is re-encrypted and re-uploaded under it;
//! 7. the pending flag clears and sharing resumes, per the new setting.
//!
//! Steps 4-6 are idempotent and re-run by [`ChatterClient::finish_profile_key_rotation`]
//! at every `start()` until they all succeed.

use crate::services::profile_visibility::{self as policy, ProfileVisibility};

use super::{ChatterClient, ClientError};

fn storage(e: crate::error::StorageError) -> ClientError {
    ClientError::Validation(format!("settings storage failed: {e}"))
}

impl ChatterClient {
    /// The current "who can see my photo and name" setting.
    pub fn profile_visibility(&self) -> ProfileVisibility {
        ProfileVisibility::load(&self.messages().settings_repo)
    }

    /// Whether moving to `visibility` needs the PIN: it restricts (so the key
    /// rotates) and the account has an SVR backup that holds the key. The UI
    /// asks for the PIN before calling [`Self::set_profile_visibility`].
    pub async fn profile_visibility_needs_pin(
        &self,
        visibility: ProfileVisibility,
    ) -> Result<bool, ClientError> {
        if visibility <= self.profile_visibility() {
            return Ok(false);
        }
        self.svr_backup()
            .has_key_backup()
            .await
            .map_err(|e| ClientError::Validation(format!("couldn't check the key backup: {e}")))
    }

    /// Change who can see our photo and name. `pin` is required when this
    /// restricts the setting on an account with an SVR backup: the backup
    /// holds the profile key and must move to the new one.
    pub async fn set_profile_visibility(
        &self,
        visibility: ProfileVisibility,
        pin: Option<&str>,
    ) -> Result<(), ClientError> {
        let settings = &self.messages().settings_repo;
        self.finish_profile_key_rotation().await?;

        let current = ProfileVisibility::load(settings);
        if visibility <= current {
            if visibility != current {
                visibility.store(settings).map_err(storage)?;
                self.sync_profile_key_to_own_devices().await;
            }
            return Ok(());
        }

        use crate::services::current_user::CurrentUserProvider;

        // A freshly linked device has the provisioned key but may not have
        // opened its profile UI yet. Fetch/decrypt missing profile fields
        // BEFORE replacing that key or its backup: after the commit the old
        // server ciphertext can no longer be read with our current key.
        let me = settings
            .current_user_id()
            .map_err(|e| ClientError::Validation(e.to_string()))?;
        if self
            .messages()
            .user_repo
            .get_profile(&me)
            .map_err(storage)?
            .is_none()
        {
            self.users().get_user_profile(&me).await.map_err(|e| {
                ClientError::Validation(format!(
                    "couldn't load your profile before changing visibility: {e}"
                ))
            })?;
        }

        let next = policy::next_profile_key(settings).map_err(storage)?;
        let has_backup =
            self.svr_backup().has_key_backup().await.map_err(|e| {
                ClientError::Validation(format!("couldn't check the key backup: {e}"))
            })?;
        if has_backup {
            let pin =
                pin.ok_or_else(|| ClientError::Validation("Enter your PIN to change this".into()))?;
            let username = self.svr_salt_username()?;
            // A wrong PIN must never reach the upload: the backup would be
            // replaced by one the real PIN cannot open.
            self.check_pin_before_backup_write(pin).await?;
            self.svr_backup()
                .backup_with_profile_key(pin, &username, &next)
                .await
                .map_err(|e| ClientError::Validation(format!("key backup failed: {e}")))?;
        }

        policy::commit_rotation(settings, visibility).map_err(storage)?;
        tracing::info!(
            visibility = visibility.as_str(),
            "profile key rotated locally"
        );
        self.finish_profile_key_rotation().await
    }

    /// Check `pin` against the local fingerprint, or — on a device that has
    /// none, i.e. a linked one — against the SVR backup itself (Signal's model:
    /// the backup is the PIN's only ground truth). The server meters that
    /// read; it does not report attempts left on a miss, so the user is told
    /// the tries are limited rather than a count. On success the fingerprint
    /// is stored, so the next check is local.
    async fn check_pin_before_backup_write(&self, pin: &str) -> Result<(), ClientError> {
        let result = self.verify_pin_with_backup(pin).await;
        if result == crate::services::link_device_service::PinVerification::Match {
            Ok(())
        } else {
            let message = if result
                == crate::services::link_device_service::PinVerification::NotSetUpOnThisDevice
            {
                "This account has no key backup to check the PIN against. Set up your PIN again."
            } else {
                result.message()
            };
            Err(ClientError::Validation(message.into()))
        }
    }

    pub(super) async fn verify_pin_with_backup(
        &self,
        pin: &str,
    ) -> crate::services::link_device_service::PinVerification {
        use crate::services::link_device_service::{
            PinVerification as V, PinVerificationError as E,
        };
        use crate::storage::repositories::settings_repo::keys;

        // Argon2 is expensive: keep the local check off the async executor.
        let service = self.link_device().clone();
        let secret = zeroize::Zeroizing::new(pin.to_owned());
        let local = match tokio::task::spawn_blocking(move || service.verify_pin(&secret)).await {
            Ok(v) => v,
            Err(_) => return V::Error(E::Crypto),
        };
        if local != V::NotSetUpOnThisDevice {
            return local;
        }
        let username = match self
            .messages()
            .settings_repo
            .get_string(keys::SVR_SALT_USERNAME)
        {
            Ok(Some(s)) if !s.trim().is_empty() => s,
            Ok(_) => return V::NotSetUpOnThisDevice,
            Err(_) => return V::Error(E::Storage),
        };
        finish_pin_backup_check(
            &self.messages().settings_repo,
            self.svr_backup()
                .verify_pin_against_backup(pin, &username)
                .await,
        )
    }

    /// Finish a committed rotation (steps 4-6 above); a no-op when none is
    /// pending. Called by `start()` so an interrupted rotation completes.
    pub async fn finish_profile_key_rotation(&self) -> Result<(), ClientError> {
        let settings = &self.messages().settings_repo;
        if !policy::rotation_pending(settings) {
            return Ok(());
        }
        policy::finish_interrupted_commit(settings).map_err(storage)?;
        let key = policy::own_profile_key(settings)
            .ok_or_else(|| ClientError::Validation("no profile key".into()))?;

        self.sync_profile_key_to_own_devices().await;
        self.auth().set_unidentified_access_key(&key).await?;
        self.reupload_own_profile().await?;

        policy::clear_rotation_pending(settings).map_err(storage)?;
        tracing::info!("profile key rotation finished");
        Ok(())
    }

    /// Tell our other devices the current key and setting. Best effort, like
    /// every own-device sync; a device that misses it keeps the old key until
    /// the next change.
    async fn sync_profile_key_to_own_devices(&self) {
        use crate::proto::content_v1::{Content, ProfileKeySync, content};

        let settings = &self.messages().settings_repo;
        let Some(key) = policy::own_profile_key(settings) else {
            return;
        };
        let mut sync = ProfileKeySync {
            profile_key: key.to_vec(),
            ..Default::default()
        };
        sync.set_visibility(ProfileVisibility::load(settings).to_proto());
        let content = Content {
            timestamp: crate::util::time::TimestampNanos::now().as_u64(),
            content: Some(content::Content::ProfileKeySync(sync)),
            ..Default::default()
        };
        let messages = self.messages();
        let devices = messages
            .sync_to_own_devices(&mut messages.auth_client(), content)
            .await;
        tracing::info!(devices, "ProfileKeySync sent to our other devices");
    }

    /// Re-encrypt every profile field under the current key and upload it,
    /// so nothing the server holds is still readable with the old key. Status
    /// goes up even when empty: skipping it would leave the old ciphertext.
    async fn reupload_own_profile(&self) -> Result<(), ClientError> {
        use crate::services::current_user::CurrentUserProvider;
        use crate::storage::repositories::settings_repo::keys;

        let m = self.messages();
        let me = m
            .settings_repo
            .current_user_id()
            .map_err(|e| ClientError::Validation(e.to_string()))?;
        let name = m
            .settings_repo
            .get_string(keys::DISPLAY_NAME)
            .ok()
            .flatten()
            .filter(|n| !n.is_empty());
        let status = m
            .user_repo
            .get_profile(&me)
            .ok()
            .flatten()
            .and_then(|p| p.status_message)
            .unwrap_or_else(|| crate::domain::user::StatusMessage::new(String::new()));
        self.users()
            .update_user_profile(name, Some(status))
            .await
            .map_err(|e| ClientError::Validation(format!("profile re-upload failed: {e}")))?;
        Ok(())
    }
}

/// Persist only a proven SVR fingerprint, using the same setting as login.
fn finish_pin_backup_check(
    settings: &crate::storage::repositories::settings_repo::SettingsRepository,
    result: Result<[u8; 32], crate::services::svr_backup_service::SvrBackupError>,
) -> crate::services::link_device_service::PinVerification {
    use crate::services::link_device_service::{PinVerification as V, PinVerificationError as E};
    use crate::services::svr_backup_service::SvrBackupError;
    use crate::storage::repositories::settings_repo::keys;
    let fingerprint = match result {
        Ok(fp) => zeroize::Zeroizing::new(fp),
        Err(SvrBackupError::WrongPin) => return V::Mismatch,
        Err(SvrBackupError::BackupNotFound) => return V::NotSetUpOnThisDevice,
        Err(SvrBackupError::RateLimited) => return V::Error(E::RateLimited),
        Err(SvrBackupError::BackupDestroyed) => return V::Error(E::BackupDestroyed),
        Err(SvrBackupError::Storage(_)) => return V::Error(E::Storage),
        Err(SvrBackupError::Network(_)) => return V::Error(E::Network),
        Err(_) => return V::Error(E::Crypto),
    };
    match settings.set_bytes(keys::PIN_FINGERPRINT, &*fingerprint) {
        Ok(()) => V::Match,
        Err(_) => V::Error(E::Storage),
    }
}

#[cfg(test)]
mod pin_tests {
    use super::finish_pin_backup_check;
    use crate::services::link_device_service::{PinVerification as V, PinVerificationError as E};
    use crate::services::svr_backup_service::SvrBackupError;
    use crate::storage::{
        Database,
        repositories::settings_repo::{SettingsRepository, keys},
    };
    use std::sync::Arc;

    #[test]
    fn shield_pin_svr_success_populates_linked_device_fingerprint() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db = Arc::new(Database::open(file.path(), "test_passphrase").unwrap());
        db.run_migrations().unwrap();
        let settings = SettingsRepository::new(db);
        settings
            .set_string(keys::SVR_SALT_USERNAME, "test-user")
            .unwrap();
        assert_eq!(settings.get_bytes(keys::PIN_FINGERPRINT).unwrap(), None);
        let fp = crate::crypto::svr::derive_pin_fingerprint("test-pin-only", "test-user").unwrap();
        assert_eq!(finish_pin_backup_check(&settings, Ok(fp)), V::Match);
        assert_eq!(
            settings.get_bytes(keys::PIN_FINGERPRINT).unwrap(),
            Some(fp.to_vec())
        );
        assert_eq!(
            finish_pin_backup_check(&settings, Err(SvrBackupError::WrongPin)),
            V::Mismatch
        );
        assert_eq!(
            finish_pin_backup_check(&settings, Err(SvrBackupError::BackupNotFound)),
            V::NotSetUpOnThisDevice
        );
        assert_eq!(
            finish_pin_backup_check(&settings, Err(SvrBackupError::RateLimited)),
            V::Error(E::RateLimited)
        );
        assert_eq!(
            settings.get_bytes(keys::PIN_FINGERPRINT).unwrap(),
            Some(fp.to_vec())
        );
        assert_eq!(
            finish_pin_backup_check(&settings, Err(SvrBackupError::BackupDestroyed)),
            V::Error(E::BackupDestroyed)
        );
        let crypto = crate::error::CryptoError::KeyDerivationFailed("synthetic failure".into());
        assert_eq!(
            finish_pin_backup_check(&settings, Err(SvrBackupError::Crypto(crypto))),
            V::Error(E::Crypto)
        );
        assert_ne!(V::Mismatch.message(), V::NotSetUpOnThisDevice.message());
        assert_ne!(V::Mismatch.message(), V::Error(E::Storage).message());
        assert_ne!(
            V::NotSetUpOnThisDevice.message(),
            V::Error(E::Storage).message()
        );
        settings
            .database()
            .connection()
            .unwrap()
            .execute_batch("DROP TABLE settings")
            .unwrap();
        assert_eq!(
            finish_pin_backup_check(&settings, Ok(fp)),
            V::Error(E::Storage)
        );
    }
}
