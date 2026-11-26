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

    pub(super) fn local_account_owner(&self) -> Result<Option<UserId>, StorageError> {
        match self.unlinked_account() {
            Some(owner) => Ok(Some(owner)),
            None => crate::storage::unlink::local_account_owner(self.settings_repo.database()),
        }
    }

    /// Saved Messages exists for every account, with channel id equal to the
    /// user id. Cache it even if this device has not synced channels yet.
    pub(super) fn cache_saved_messages_channel(&self, owner: UserId) -> Result<(), StorageError> {
        use crate::domain::channel::{Channel, ChannelType};
        use crate::storage::ChannelRepository;

        let mut channel = Channel::new(
            owner.into(),
            format!("saved_messages_{owner}"),
            ChannelType::SavedMessages,
            owner,
        );
        channel.config.max_members = Some(1);
        ChannelRepository::new(self.settings_repo.database().clone()).insert_or_ignore(&channel)?;
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

/// Delete the account rows and the attachment files they held the keys to.
pub(crate) fn wipe_unlinked_install(db: &Database) -> Result<(), StorageError> {
    #[cfg(not(test))]
    let dir = crate::services::attachment::attachments_dir();
    // Unit tests share a process-wide attachment directory. Use the fixture's
    // database directory for the wipe so other tests keep their files.
    #[cfg(test)]
    let dir = db.path().with_file_name("attachments");
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(StorageError::InvalidData(format!(
                "could not delete the attachment files of the previous account: {e}"
            )));
        }
    }
    crate::services::attachment::ensure_attachments_dir_at(&dir).map_err(|e| {
        StorageError::InvalidData(format!("could not recreate the attachments directory: {e}"))
    })?;
    // Keep the ownership row until file cleanup succeeds so a retry still
    // knows it must wipe.
    crate::storage::unlink::wipe_account_data(db)?;
    tracing::info!("deleted the local data of the previous account");
    Ok(())
}
