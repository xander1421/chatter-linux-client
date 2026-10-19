//! What this device does once the server says another device of the account
//! unlinked it. See `storage::unlink` for the Signal-Desktop behaviour this
//! follows and why it runs only on that answer.

use super::*;
use crate::storage::Database;

impl AuthService {
    /// Signal-Desktop `unlinkAndDisconnect`: stop authenticating, and delete
    /// this device's credentials, keys and sessions while keeping its history,
    /// remembering `former_owner` for the next link or sign-in.
    pub(crate) fn forget_unlinked_device(&self, former_owner: UserId) -> Result<(), AuthError> {
        self.auth_interceptor.clear();
        crate::storage::unlink::forget_unlinked_device(
            self.settings_repo.database(),
            former_owner,
        )?;
        Ok(())
    }

    /// The account this install was unlinked from, until it is linked or
    /// signed in again.
    pub fn unlinked_account(&self) -> Option<UserId> {
        unlinked_account(&self.settings_repo)
    }
}

pub(crate) fn unlinked_account(settings: &SettingsRepository) -> Option<UserId> {
    settings
        .get_string(keys::UNLINKED_ACCOUNT_ID)
        .ok()
        .flatten()
        .and_then(|id| id.parse().ok())
}

/// Signal's `removeAllData` for an unlinked install: every account row
/// (`storage::unlink::wipe_account_data`) and the attachment files those rows
/// held the keys to. Callers run it only when [`unlinked_account`] is set.
pub(crate) fn wipe_unlinked_install(db: &Database) -> Result<(), StorageError> {
    crate::storage::unlink::wipe_account_data(db)?;
    let dir = crate::services::attachment::attachments_dir();
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(StorageError::InvalidData(format!(
                "could not delete the attachment files of the unlinked account: {e}"
            )));
        }
    }
    crate::services::attachment::ensure_attachments_dir().map_err(|e| {
        StorageError::InvalidData(format!("could not recreate the attachments directory: {e}"))
    })?;
    tracing::info!("deleted the local data of the account this install was unlinked from");
    Ok(())
}
