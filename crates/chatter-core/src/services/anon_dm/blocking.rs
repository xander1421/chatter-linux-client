use crate::domain::anon_dm::{AnonDmId, AnonDmStatus};
use crate::domain::ids::{AttachmentId, ChannelId, MessageId};
use crate::domain::message::ConversationType;
use crate::domain::UserId;

use super::{AnonDmError, AnonDmService};

impl AnonDmService {
    /// Block a conversation (can be done at any time).
    pub fn block_conversation(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        self.anon_dm_repo
            .update_status(conversation_id, AnonDmStatus::Blocked)?;
        Ok(())
    }

    /// Unblock a conversation.
    pub fn unblock_conversation(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        self.anon_dm_repo
            .update_status(conversation_id, AnonDmStatus::Active)?;
        tracing::info!("Unblocked conversation {}", conversation_id);
        Ok(())
    }

    /// Check if a conversation is blocked.
    pub fn is_conversation_blocked(&self, conversation_id: &AnonDmId) -> Result<bool, AnonDmError> {
        match self.anon_dm_repo.get_conversation(conversation_id)? {
            Some(dm) => Ok(dm.status == AnonDmStatus::Blocked),
            None => Err(AnonDmError::ConversationNotFound(*conversation_id)),
        }
    }

    /// Block a user globally (block all anonymous DMs from this user).
    pub fn block_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<(), AnonDmError> {
        self.anon_dm_repo.block_user(user_id, reason)?;
        tracing::info!("Globally blocked user {} for anonymous DMs", user_id);
        Ok(())
    }

    /// Unblock a user globally.
    pub fn unblock_user(&self, user_id: &UserId) -> Result<(), AnonDmError> {
        self.anon_dm_repo.unblock_user(user_id)?;
        tracing::info!("Unblocked user {} for anonymous DMs", user_id);
        Ok(())
    }

    /// Check if a user is globally blocked.
    pub fn is_user_blocked(&self, user_id: &UserId) -> Result<bool, AnonDmError> {
        Ok(self.anon_dm_repo.is_user_blocked(user_id)?)
    }

    /// Get all globally blocked users.
    pub fn get_blocked_users(&self) -> Result<Vec<UserId>, AnonDmError> {
        Ok(self.anon_dm_repo.get_blocked_users()?)
    }

    // ========================================================================
    // Rate Limiting
    // ========================================================================

    /// Default rate limit: 100 messages per minute per conversation.
    const RATE_LIMIT_MESSAGES: u32 = 100;
    /// Default rate limit window: 1 minute.
    const RATE_LIMIT_WINDOW: chrono::Duration = chrono::Duration::minutes(1);

    /// Check if sending a message would exceed rate limits.
    pub fn check_rate_limit(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        let count = self.anon_dm_repo.increment_rate_limit(conversation_id, Self::RATE_LIMIT_WINDOW)?;

        if count > Self::RATE_LIMIT_MESSAGES {
            tracing::warn!(
                "Rate limit exceeded for conversation {}: {} messages in window",
                conversation_id,
                count
            );
            return Err(AnonDmError::RateLimitExceeded(Self::RATE_LIMIT_MESSAGES));
        }

        Ok(())
    }

    /// Check if the conversation is blocked (cannot send).
    pub fn check_conversation_not_blocked(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        let dm = self
            .anon_dm_repo
            .get_conversation(conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(*conversation_id))?;

        if dm.status == AnonDmStatus::Blocked {
            return Err(AnonDmError::ConversationBlocked(*conversation_id));
        }

        Ok(())
    }

    /// Delete a conversation and everything stored for it on this device: its
    /// messages (reactions and shared-content rows cascade), the attachments
    /// no other message still carries (row and encrypted file), and the
    /// conversation row (mailboxes cascade). A peer block is keyed by peer id
    /// and stays — Signal's Delete on a blocked chat — so it takes the peer's
    /// name with it for Friends › Blocked.
    pub fn delete_conversation(&self, conversation_id: &AnonDmId) -> Result<(), AnonDmError> {
        let dm = self
            .anon_dm_repo
            .get_conversation(conversation_id)?
            .ok_or(AnonDmError::ConversationNotFound(*conversation_id))?;
        if let Some(name) = dm.peer_display_name.as_deref().filter(|n| !n.is_empty()) {
            self.anon_dm_repo.set_blocked_display_name(&dm.peer_id, name)?;
        }

        let channel_id: ChannelId = (*conversation_id).into();
        let messages =
            self.message_repo
                .list(&channel_id, &ConversationType::AnonDm, None, u32::MAX)?;
        let attachments: std::collections::HashSet<AttachmentId> =
            messages.iter().flat_map(|m| m.attachments.iter().copied()).collect();
        let ids: Vec<MessageId> = messages.into_iter().map(|m| m.id).collect();
        self.message_repo.delete_batch(&ids)?;

        for id in &attachments {
            if self.attachment_repo.is_referenced_by_message(id)? {
                continue;
            }
            if let Some(path) = self.attachment_repo.get_file_path(id)? {
                if let Err(e) = std::fs::remove_file(&path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        tracing::warn!(attachment_id = %id, error = %e, "anon DM delete: attachment file not removed");
                    }
                }
            }
            match self.attachment_repo.delete(id) {
                Ok(()) | Err(crate::error::StorageError::NotFound(_)) => {}
                Err(e) => return Err(e.into()),
            }
        }

        self.anon_dm_repo.delete_conversation(conversation_id)?;
        Ok(())
    }
}
