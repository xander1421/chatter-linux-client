//! Ephemeral service.
//!
//! Lifecycle management for self-destructing ephemeral messages.

use std::sync::Arc;

use crate::domain::ephemeral::{EphemeralMessage, MessageTTL};
use crate::domain::ids::MessageId;
use crate::error::StorageError;
use crate::storage::repositories::ephemeral_repo::EphemeralRepository;
use crate::storage::repositories::settings_repo::{SettingsRepository, keys};

/// Ephemeral service errors.
#[derive(Debug, thiserror::Error)]
pub enum EphemeralError {
    /// Operation requires authentication.
    #[error("not logged in")]
    NotLoggedIn,

    /// Message to track was not found.
    #[error("message not found: {0}")]
    MessageNotFound(MessageId),

    /// Message is not an ephemeral message.
    #[error("message is not ephemeral: {0}")]
    NotEphemeral(MessageId),

    /// Storage operation failed.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
}

/// Ephemeral service for privacy features.
///
/// Manages the lifecycle of ephemeral (self-destructing) messages:
/// - Tracking new ephemeral messages
/// - Starting countdown when messages are read
/// - Cleaning up expired messages
pub struct EphemeralService {
    settings_repo: Arc<SettingsRepository>,
    ephemeral_repo: Arc<EphemeralRepository>,
}

impl EphemeralService {
    /// Create a new EphemeralService with dependencies.
    pub fn new(
        settings_repo: Arc<SettingsRepository>,
        ephemeral_repo: Arc<EphemeralRepository>,
    ) -> Self {
        Self {
            settings_repo,
            ephemeral_repo,
        }
    }

    /// Check that the user is logged in.
    fn check_logged_in(&self) -> Result<(), EphemeralError> {
        let user_id = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(EphemeralError::Storage)?;

        if user_id.is_none() {
            return Err(EphemeralError::NotLoggedIn);
        }
        Ok(())
    }

    /// Track a new ephemeral message.
    ///
    /// Called after an ephemeral message has been sent or received.
    /// Creates local tracking for the message's TTL state.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of the message to track
    /// * `ttl` - Time-to-live for the message
    ///
    /// # Returns
    ///
    /// The created EphemeralMessage tracking entry.
    pub fn track(
        &self,
        message_id: MessageId,
        ttl: MessageTTL,
    ) -> Result<EphemeralMessage, EphemeralError> {
        self.check_logged_in()?;

        let ephemeral = EphemeralMessage::new(message_id, ttl);
        self.ephemeral_repo.insert(&ephemeral)?;

        Ok(ephemeral)
    }

    /// Track a message armed with a conversation disappearing timer.
    ///
    /// This is the arming path used on send and receive: the message already
    /// carries an armed `ephemeral_ttl` (V29). The repository starts the clock
    /// now for a read message (our own sends) and on read otherwise (Signal
    /// read-start), and the periodic reaper (`disappearing::reap_expired`)
    /// removes it once the ttl elapses — including on the next boot after
    /// a restart, since `expires_at` is persisted (migration v015).
    ///
    /// Unlike [`track`](Self::track) this accepts arbitrary `ttl_seconds`
    /// (a conversation timer is not restricted to the `MessageTTL` menu).
    /// `ttl_seconds == 0` is a no-op (timer disabled).
    pub fn track_armed(
        &self,
        message_id: MessageId,
        ttl_seconds: u32,
    ) -> Result<(), EphemeralError> {
        if ttl_seconds == 0 {
            return Ok(());
        }
        self.check_logged_in()?;
        self.ephemeral_repo.insert_armed(&message_id, ttl_seconds)?;
        Ok(())
    }

    /// Mark an ephemeral message as read.
    ///
    /// This starts the countdown timer. The message will be deleted
    /// after the TTL expires from the time of reading.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of the message to mark as read
    ///
    /// # Returns
    ///
    /// The updated EphemeralMessage with countdown started.
    pub fn mark_read(&self, message_id: &MessageId) -> Result<EphemeralMessage, EphemeralError> {
        self.check_logged_in()?;

        // Mark as read in repository (starts countdown)
        self.ephemeral_repo.mark_read(message_id)?;

        // Get the updated state
        let ephemeral = self
            .ephemeral_repo
            .get(message_id)?
            .ok_or_else(|| EphemeralError::MessageNotFound(*message_id))?;

        Ok(ephemeral)
    }

    /// Get the ephemeral state for a message.
    ///
    /// Returns None if the message is not ephemeral.
    ///
    /// # Arguments
    ///
    /// * `message_id` - ID of the message to check
    ///
    /// # Returns
    ///
    /// The EphemeralMessage if the message is ephemeral, None otherwise.
    pub fn get_state(
        &self,
        message_id: &MessageId,
    ) -> Result<Option<EphemeralMessage>, EphemeralError> {
        self.check_logged_in()?;

        let ephemeral = self.ephemeral_repo.get(message_id)?;
        Ok(ephemeral)
    }

    /// List all pending (unread) ephemeral messages.
    ///
    /// Returns ephemeral messages that have been received but not yet read,
    /// meaning their countdown timer has not yet started.
    ///
    /// # Returns
    ///
    /// List of pending EphemeralMessage entries.
    pub fn list_pending(&self) -> Result<Vec<EphemeralMessage>, EphemeralError> {
        self.check_logged_in()?;

        let pending = self.ephemeral_repo.list_pending()?;
        Ok(pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ephemeral_error_display() {
        let err = EphemeralError::NotLoggedIn;
        assert!(err.to_string().contains("not logged in"));

        let msg_id = MessageId::new();
        let err = EphemeralError::MessageNotFound(msg_id.clone());
        assert!(err.to_string().contains("message not found"));

        let err = EphemeralError::NotEphemeral(msg_id);
        assert!(err.to_string().contains("not ephemeral"));
    }

    #[test]
    fn test_message_ttl_to_prost_duration() {
        let ttl = MessageTTL::Minutes5;
        let duration = prost_types::Duration {
            seconds: ttl.as_secs() as i64,
            nanos: 0,
        };
        assert_eq!(duration.seconds, 300);
        assert_eq!(duration.nanos, 0);
    }
}
