use crate::domain::UserId;
use crate::error::MailboxError;

impl super::DmConversationManager {
    /// Delete all state for a conversation.
    ///
    /// Useful when user deletes a conversation.
    pub fn delete_conversation(&self, peer_id: &UserId) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let peer_id_str = peer_id.to_string();

        conn.execute(
            "DELETE FROM dm_device_pending WHERE conversation_id = ?1",
            rusqlite::params![peer_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        conn.execute(
            "DELETE FROM dm_device_asked WHERE conversation_id = ?1",
            rusqlite::params![peer_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        conn.execute(
            "DELETE FROM dm_mailboxes WHERE conversation_id = ?1",
            rusqlite::params![peer_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        conn.execute(
            "DELETE FROM dm_peer_mailboxes WHERE conversation_id = ?1",
            rusqlite::params![peer_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        conn.execute(
            "DELETE FROM dm_conversation_keys WHERE conversation_id = ?1",
            rusqlite::params![peer_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }
}
