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

/// Saved Messages exists for every account, with channel id equal to the
/// user id. Cache it even if this device has not synced channels yet.
pub(crate) fn cache_saved_messages_channel(
    settings: &SettingsRepository,
    owner: UserId,
) -> Result<(), StorageError> {
    use crate::domain::channel::{
        Channel, ChannelType, MemberRole, Membership, NotificationPreference,
    };
    use crate::storage::ChannelRepository;

    let mut channel = Channel::new(
        owner.into(),
        format!("saved_messages_{owner}"),
        ChannelType::SavedMessages,
        owner,
    );
    channel.config.max_members = Some(1);
    let repo = ChannelRepository::new(settings.database().clone());
    repo.insert_or_ignore(&channel)?;
    if let Some(saved) = repo.get(&channel.id)?
        && saved.channel_type == ChannelType::SavedMessages
        && saved.owner_id == owner
    {
        repo.insert_or_ignore_membership(&Membership {
            channel_id: saved.id,
            user_id: owner,
            role: MemberRole::Owner,
            joined_at: saved.created_at,
            last_read_message_id: None,
            is_muted: false,
            muted_until: None,
            notification_preference: NotificationPreference::default(),
        })?;
    }
    Ok(())
}

pub(crate) fn unlinked_account(settings: &SettingsRepository) -> Option<UserId> {
    settings
        .get_string(keys::UNLINKED_ACCOUNT_ID)
        .ok()
        .flatten()
        .and_then(|id| id.parse().ok())
}

fn attachment_error(error: std::io::Error) -> StorageError {
    StorageError::InvalidData(format!(
        "could not update the local attachment directory: {error}"
    ))
}

/// A crash before the database wipe keeps the Saved Messages owner and allows
/// the same account to put its attachments back.
pub(crate) fn restore_staged_attachments(db: &Database) -> Result<(), StorageError> {
    let dir = db.path().with_file_name("attachments");
    let staged = db.path().with_file_name("attachments.account-wipe");
    if staged.try_exists().map_err(attachment_error)? {
        // Startup may have created a new empty directory in the meantime.
        match std::fs::remove_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(attachment_error(error)),
        }
        std::fs::rename(staged, dir).map_err(attachment_error)?;
    }
    Ok(())
}

pub(crate) fn wipe_unlinked_install(db: &Database) -> Result<(), StorageError> {
    wipe_install(db, false)
}

pub(crate) fn wipe_switched_account(db: &Database) -> Result<(), StorageError> {
    wipe_install(db, true)
}

fn wipe_install(db: &Database, include_mesh: bool) -> Result<(), StorageError> {
    let dir = db.path().with_file_name("attachments");
    let staged = db.path().with_file_name("attachments.account-wipe");
    if !staged.try_exists().map_err(attachment_error)? {
        match std::fs::rename(&dir, &staged) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(attachment_error(error)),
        }
    }
    if include_mesh {
        crate::storage::unlink::wipe_all_rows(db)?;
    } else {
        crate::storage::unlink::wipe_account_data(db)?;
    }
    for path in [staged, dir.clone()] {
        match std::fs::remove_dir_all(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(attachment_error(error)),
        }
    }
    crate::services::attachment::ensure_attachments_dir_at(&dir).map_err(attachment_error)?;
    tracing::info!("deleted the local data of the previous account");
    Ok(())
}
