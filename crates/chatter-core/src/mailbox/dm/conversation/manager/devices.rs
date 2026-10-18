use chrono::Utc;

use crate::domain::UserId;
use crate::error::MailboxError;

use super::super::{ConversationKey, PeerMailbox};

impl super::DmConversationManager {
    /// Record the half `peer_device_id` announced in its bootstrap: its
    /// mailbox and ephemeral, checked against our half first (a low-order
    /// ephemeral is refused). Replaces only that device's previous half —
    /// the F53 re-bootstrap rule, per device.
    ///
    /// `peer_device_id` must be authenticated by the caller (the Signal layer
    /// that carried the bootstrap names it): it decides whose row changes.
    pub fn record_peer_device(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        mailbox_public_key: [u8; 32],
        their_ephemeral_public: [u8; 32],
    ) -> Result<PeerMailbox, MailboxError> {
        let conversation = self
            .load_conversation(peer_id)?
            .ok_or(MailboxError::ConversationNotFound)?;
        // Expiry is not on the wire.
        let mailbox = PeerMailbox::new(
            mailbox_public_key,
            peer_device_id,
            their_ephemeral_public,
            None,
        );
        let mut key = conversation
            .conversation_key
            .ok_or(MailboxError::ConversationNotEstablished)?;
        key.derive_shared_secret(their_ephemeral_public)?;
        self.save_peer_mailbox(peer_id, &mailbox)?;
        self.clear_device_asked(peer_id, peer_device_id)?;
        Ok(mailbox)
    }

    /// The device answered (or is gone): the next ask is a new one.
    fn clear_device_asked(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        conn.execute(
            "DELETE FROM dm_device_asked WHERE conversation_id = ?1 AND peer_device_id = ?2",
            rusqlite::params![peer_id.to_string(), peer_device_id],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Point `peer_device_id`'s half at its rotated mailbox. False when that
    /// device has no half on record.
    pub fn rotate_peer_mailbox(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        new_public_key: [u8; 32],
    ) -> Result<bool, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let rows = conn
            .execute(
                "UPDATE dm_peer_mailboxes SET peer_mailbox_public_key = ?1, received_at = ?2
                 WHERE conversation_id = ?3 AND peer_device_id = ?4",
                rusqlite::params![
                    new_public_key.as_slice(),
                    Utc::now().to_rfc3339(),
                    peer_id.to_string(),
                    peer_device_id
                ],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        Ok(rows > 0)
    }

    /// Drop a peer device from the conversation (the server refused it as
    /// unknown): its half and anything still owed to it.
    pub fn forget_peer_device(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        for table in ["dm_peer_mailboxes", "dm_device_pending", "dm_device_asked"] {
            conn.execute(
                &format!("DELETE FROM {table} WHERE conversation_id = ?1 AND peer_device_id = ?2"),
                rusqlite::params![peer_id.to_string(), peer_device_id],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        }
        Ok(())
    }

    /// Save our half of the conversation key.
    ///
    /// A NEW ephemeral drops every peer device's half: each was paired with
    /// the old one, so its secret is one that device no longer shares with
    /// us. Each device re-answers the bootstrap that announces the new one.
    pub fn save_conversation_key(
        &self,
        peer_id: &UserId,
        key: &ConversationKey,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let peer_id_str = peer_id.to_string();

        conn.execute(
            "DELETE FROM dm_peer_mailboxes WHERE conversation_id = ?1
               AND NOT EXISTS (SELECT 1 FROM dm_conversation_keys
                               WHERE conversation_id = ?1 AND our_ephemeral_public = ?2)",
            rusqlite::params![peer_id_str, key.our_ephemeral_public.as_slice()],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;
        conn.execute(
            "INSERT OR REPLACE INTO dm_conversation_keys
             (conversation_id, our_ephemeral_public, our_ephemeral_private, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                peer_id_str,
                key.our_ephemeral_public.as_slice(),
                key.our_ephemeral_private.as_slice(),
                key.created_at.to_rfc3339(),
            ],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }
}
