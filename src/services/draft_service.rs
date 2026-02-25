//! Draft service.
//!
//! Manage message drafts with auto-save support.

use std::sync::Arc;

use crate::domain::draft::{Draft, DraftKey};
use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::error::StorageError;
use crate::services::current_user::{CurrentUserError, CurrentUserProvider};
use crate::storage::repositories::draft_repo::DraftRepository;
use crate::storage::repositories::settings_repo::SettingsRepository;

/// Draft service errors.
#[derive(Debug, thiserror::Error)]
pub enum DraftError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Draft not found.
    #[error("draft not found")]
    NotFound,

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

impl From<CurrentUserError> for DraftError {
    fn from(err: CurrentUserError) -> Self {
        match err {
            CurrentUserError::Storage(e) => DraftError::Storage(e),
            CurrentUserError::NotLoggedIn => DraftError::NotLoggedIn,
        }
    }
}

/// Draft service for managing message drafts.
///
/// Provides save, load, delete, and list operations for message drafts.
/// Drafts are keyed by channel + optional thread, allowing one draft per
/// conversation context.
pub struct DraftService {
    settings_repo: Arc<SettingsRepository>,
    draft_repo: Arc<DraftRepository>,
}

impl DraftService {
    /// Create a new DraftService with dependencies.
    pub fn new(settings_repo: Arc<SettingsRepository>, draft_repo: Arc<DraftRepository>) -> Self {
        Self {
            settings_repo,
            draft_repo,
        }
    }

    /// Save or update a draft.
    ///
    /// If a draft already exists for the given channel/thread, it will be updated.
    /// Otherwise a new draft will be created.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel the draft belongs to
    /// * `thread_id` - Optional thread ID (None for main channel)
    /// * `content` - Draft text content
    /// * `reply_to` - Optional message being replied to
    /// * `attachments` - Attachment IDs included in the draft
    ///
    /// # Returns
    ///
    /// The saved draft.
    pub fn save_draft(
        &self,
        channel_id: ChannelId,
        thread_id: Option<MessageId>,
        content: String,
        reply_to: Option<MessageId>,
        attachments: Vec<AttachmentId>,
    ) -> Result<Draft, DraftError> {
        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        // Build draft key
        let key = match thread_id {
            Some(tid) => DraftKey::for_thread(channel_id, tid),
            None => DraftKey::for_channel(channel_id),
        };

        // Check if draft exists
        let draft = match self.draft_repo.get_by_key(&key)? {
            Some(mut existing) => {
                // Update existing draft
                existing.update_content(content);
                existing.reply_to = reply_to;
                existing.attachments = attachments;
                existing
            }
            None => {
                // Create new draft
                let mut draft = Draft::new(key, content);
                draft.reply_to = reply_to;
                draft.attachments = attachments;
                draft
            }
        };

        // Upsert to storage
        self.draft_repo.upsert(&draft)?;

        Ok(draft)
    }

    /// Get a draft by channel and optional thread.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel the draft belongs to
    /// * `thread_id` - Optional thread ID (None for main channel)
    ///
    /// # Returns
    ///
    /// The draft if it exists, None otherwise.
    pub fn get_draft(
        &self,
        channel_id: ChannelId,
        thread_id: Option<MessageId>,
    ) -> Result<Option<Draft>, DraftError> {
        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        // Build draft key
        let key = match thread_id {
            Some(tid) => DraftKey::for_thread(channel_id, tid),
            None => DraftKey::for_channel(channel_id),
        };

        Ok(self.draft_repo.get_by_key(&key)?)
    }

    /// Delete a draft.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel the draft belongs to
    /// * `thread_id` - Optional thread ID (None for main channel)
    ///
    /// # Errors
    ///
    /// Returns `DraftError::NotFound` if no draft exists for the given key.
    pub fn delete_draft(
        &self,
        channel_id: ChannelId,
        thread_id: Option<MessageId>,
    ) -> Result<(), DraftError> {
        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        // Build draft key
        let key = match thread_id {
            Some(tid) => DraftKey::for_thread(channel_id, tid),
            None => DraftKey::for_channel(channel_id),
        };

        // Delete from storage, map NotFound error
        self.draft_repo.delete_by_key(&key).map_err(|e| {
            match e {
                StorageError::NotFound(_) => DraftError::NotFound,
                other => DraftError::Storage(other),
            }
        })
    }

    /// List all drafts for a channel.
    ///
    /// Returns drafts for both the main channel and any threads within it.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel to list drafts for
    ///
    /// # Returns
    ///
    /// Vector of drafts, sorted by most recently updated first.
    pub fn list_drafts_for_channel(
        &self,
        channel_id: &ChannelId,
    ) -> Result<Vec<Draft>, DraftError> {
        // Check logged in
        let _ = self.settings_repo.current_user_id()?;

        Ok(self.draft_repo.list_for_channel(channel_id)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_draft_error_display() {
        let err = DraftError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let err = DraftError::NotFound;
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_draft_key_construction() {
        let channel_id = ChannelId::new();
        let thread_id = MessageId::new();

        // Channel-only key
        let key1 = DraftKey::for_channel(channel_id);
        assert!(!key1.is_thread());

        // Thread key
        let key2 = DraftKey::for_thread(channel_id, thread_id);
        assert!(key2.is_thread());
    }
}
