use chrono::{Duration, Utc};

use crate::domain::UserId;
use crate::error::MailboxError;

use super::super::{DmMailbox, PeerMailbox};

impl super::DmConversationManager {
    /// Save/update our mailbox for a conversation.
    pub fn save_our_mailbox(
        &self,
        peer_id: &UserId,
        mailbox: &DmMailbox,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let peer_id_str = peer_id.to_string();

        conn.execute(
            "INSERT OR REPLACE INTO dm_mailboxes
             (id, conversation_id, mailbox_public_key, mailbox_private_key, signature,
              token_id, created_at, expires_at, last_polled_at,
              mailbox_encryption_private_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                mailbox.id,
                peer_id_str,
                mailbox.public_key.as_slice(),
                mailbox.private_key.as_slice(),
                mailbox.signature,
                mailbox.token_id,
                mailbox.created_at.to_rfc3339(),
                mailbox.expires_at.to_rfc3339(),
                mailbox.last_polled_at.map(|dt| dt.to_rfc3339()),
                mailbox.encryption_private_key.as_slice()
            ],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Save/update one peer device's half. Only that device's row changes.
    pub fn save_peer_mailbox(
        &self,
        peer_id: &UserId,
        mailbox: &PeerMailbox,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        conn.execute(
            "INSERT OR REPLACE INTO dm_peer_mailboxes
             (conversation_id, peer_device_id, peer_mailbox_public_key, their_ephemeral_public, received_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                peer_id.to_string(),
                mailbox.device_id,
                mailbox.public_key.as_slice(),
                mailbox.their_ephemeral_public.as_slice(),
                mailbox.received_at.to_rfc3339(),
                mailbox.expires_at.map(|dt| dt.to_rfc3339())
            ],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }
    /// Update last_polled_at timestamp for a mailbox.
    pub fn update_poll_time(&self, mailbox_id: &str) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now = Utc::now().to_rfc3339();

        let rows = conn
            .execute(
                "UPDATE dm_mailboxes SET last_polled_at = ?1 WHERE id = ?2",
                rusqlite::params![now, mailbox_id],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        if rows == 0 {
            return Err(MailboxError::TokenNotFound(mailbox_id.to_string()));
        }

        Ok(())
    }

    /// Get all active mailbox keys (for polling multiple conversations).
    ///
    /// Returns tuples of (peer_id, mailbox_public_key, mailbox_private_key, mailbox_id).
    #[allow(clippy::type_complexity)]
    pub fn get_active_mailbox_keys(
        &self,
    ) -> Result<Vec<(UserId, [u8; 32], [u8; 64], String)>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now = Utc::now().to_rfc3339();

        let mut stmt = conn
            .prepare(
                "SELECT conversation_id, mailbox_public_key, mailbox_private_key, id
                 FROM dm_mailboxes WHERE expires_at > ?1",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut rows = stmt
            .query(rusqlite::params![now])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut results = Vec::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| MailboxError::Storage(e.to_string()))?
        {
            let conversation_id: String = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let public_key: Vec<u8> = row
                .get(1)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let private_key: Vec<u8> = row
                .get(2)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let mailbox_id: String = row
                .get(3)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            let peer_id: UserId = conversation_id.parse().map_err(|e: uuid::Error| {
                MailboxError::Storage(format!("Invalid peer_id: {}", e))
            })?;

            let public_key_arr: [u8; 32] = public_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid public key length".into()))?;
            let private_key_arr: [u8; 64] = private_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid private key length".into()))?;

            results.push((peer_id, public_key_arr, private_key_arr, mailbox_id));
        }

        Ok(results)
    }

    /// List conversations needing mailbox refresh (expiring soon).
    ///
    /// Returns peer IDs of conversations with mailboxes expiring within given hours.
    pub fn get_expiring_mailboxes(&self, within_hours: i64) -> Result<Vec<UserId>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let threshold = (Utc::now() + Duration::hours(within_hours)).to_rfc3339();
        let now = Utc::now().to_rfc3339();

        // Re-rotation guard: a conversation is a candidate only if its NEWEST
        // row is expiring within the window. Once rotation has minted a fresh
        // row (expires_at > threshold) the conversation is excluded, so a
        // lingering old row in the overlap window can't re-trigger rotation
        // every cycle and drain the token budget.
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT conversation_id FROM dm_mailboxes
                 WHERE expires_at > ?1 AND expires_at <= ?2
                   AND conversation_id NOT IN (
                       SELECT conversation_id FROM dm_mailboxes WHERE expires_at > ?2
                   )",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut rows = stmt
            .query(rusqlite::params![now, threshold])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut results = Vec::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| MailboxError::Storage(e.to_string()))?
        {
            let conversation_id: String = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            let peer_id: UserId = conversation_id.parse().map_err(|e: uuid::Error| {
                MailboxError::Storage(format!("Invalid peer_id: {}", e))
            })?;

            results.push(peer_id);
        }

        Ok(results)
    }

    /// Delete expired mailbox state.
    ///
    /// Returns the number of expired mailboxes cleaned up.
    pub fn cleanup_expired(&self) -> Result<u32, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now = Utc::now().to_rfc3339();

        let rows = conn
            .execute(
                "DELETE FROM dm_mailboxes WHERE expires_at <= ?1",
                rusqlite::params![now],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(rows as u32)
    }
}
