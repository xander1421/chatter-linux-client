use chrono::{DateTime, Utc};

use crate::domain::UserId;
use crate::error::MailboxError;

use super::super::{ConversationKey, DmConversation, DmMailbox, PeerMailbox};

impl super::DmConversationManager {
    /// Load conversation state from database.
    ///
    /// Returns None if no state exists for this peer.
    pub fn load_conversation(
        &self,
        peer_id: &UserId,
    ) -> Result<Option<DmConversation>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let peer_id_str = peer_id.to_string();

        // Load our mailbox
        let our_mailbox = self.load_our_mailbox_internal(&conn, &peer_id_str)?;

        let peer_mailboxes = self.load_peer_mailboxes_internal(&conn, &peer_id_str)?;
        let conversation_key = self.load_conversation_key_internal(&conn, &peer_id_str)?;

        // If no state exists, return None
        if our_mailbox.is_none() && peer_mailboxes.is_empty() && conversation_key.is_none() {
            return Ok(None);
        }

        Ok(Some(DmConversation {
            peer_id: *peer_id,
            our_mailbox,
            conversation_key,
            peer_mailboxes,
        }))
    }

    /// Load our mailbox from database.
    fn load_our_mailbox_internal(
        &self,
        conn: &rusqlite::Connection,
        peer_id_str: &str,
    ) -> Result<Option<DmMailbox>, MailboxError> {
        let mut stmt = conn
            .prepare(
                // During rotation two mailboxes coexist for a conversation; the
                // NEWEST is the active one that sends must target. (All non-expired
                // rows are still polled for receive via get_active_mailbox_keys.)
                "SELECT id, mailbox_public_key, mailbox_private_key, signature,
                        token_id, created_at, expires_at, last_polled_at,
                        mailbox_encryption_private_key
                 FROM dm_mailboxes WHERE conversation_id = ?1
                 ORDER BY created_at DESC LIMIT 1",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![peer_id_str], |row| {
            let id: String = row.get(0)?;
            let public_key: Vec<u8> = row.get(1)?;
            let private_key: Vec<u8> = row.get(2)?;
            let signature: Vec<u8> = row.get(3)?;
            let token_id: String = row.get(4)?;
            let created_at: String = row.get(5)?;
            let expires_at: String = row.get(6)?;
            let last_polled_at: Option<String> = row.get(7)?;
            let encryption_private_key: Vec<u8> = row.get(8)?;

            Ok((
                id,
                public_key,
                private_key,
                signature,
                token_id,
                created_at,
                expires_at,
                last_polled_at,
                encryption_private_key,
            ))
        });

        match result {
            Ok((
                id,
                public_key,
                private_key,
                signature,
                token_id,
                created_at,
                expires_at,
                last_polled_at,
                encryption_private_key,
            )) => {
                let public_key_arr: [u8; 32] = public_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid public key length".into()))?;
                let private_key_arr: [u8; 64] = private_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid private key length".into()))?;
                let encryption_private_key: [u8; 32] = encryption_private_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid encryption key length".into()))?;

                let created_at = DateTime::parse_from_rfc3339(&created_at)
                    .map_err(|e| MailboxError::Storage(format!("Invalid created_at: {}", e)))?
                    .with_timezone(&Utc);
                let expires_at = DateTime::parse_from_rfc3339(&expires_at)
                    .map_err(|e| MailboxError::Storage(format!("Invalid expires_at: {}", e)))?
                    .with_timezone(&Utc);
                let last_polled_at = last_polled_at.and_then(|s| {
                    DateTime::parse_from_rfc3339(&s)
                        .map(|dt| dt.with_timezone(&Utc))
                        .ok()
                });

                Ok(Some(DmMailbox {
                    id,
                    public_key: public_key_arr,
                    private_key: private_key_arr,
                    encryption_private_key,
                    signature,
                    token_id,
                    created_at,
                    expires_at,
                    last_polled_at,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MailboxError::Storage(e.to_string())),
        }
    }

    /// Load every peer device's mailbox, lowest device id first.
    fn load_peer_mailboxes_internal(
        &self,
        conn: &rusqlite::Connection,
        peer_id_str: &str,
    ) -> Result<Vec<PeerMailbox>, MailboxError> {
        let storage = |e: rusqlite::Error| MailboxError::Storage(e.to_string());
        let mut stmt = conn
            .prepare(
                "SELECT peer_mailbox_public_key, peer_device_id, their_ephemeral_public, received_at, expires_at
                 FROM dm_peer_mailboxes WHERE conversation_id = ?1
                 ORDER BY peer_device_id",
            )
            .map_err(storage)?;
        let rows = stmt
            .query_map(rusqlite::params![peer_id_str], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(storage)?;

        let mut mailboxes = Vec::new();
        for row in rows {
            let (public_key, device_id, their_ephemeral, received_at, expires_at) =
                row.map_err(storage)?;
            let parse = |s: &str| DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc));
            mailboxes.push(PeerMailbox {
                public_key: public_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid peer public key length".into()))?,
                device_id,
                their_ephemeral_public: their_ephemeral
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid peer ephemeral length".into()))?,
                received_at: parse(&received_at)
                    .map_err(|e| MailboxError::Storage(format!("Invalid received_at: {}", e)))?,
                expires_at: expires_at.and_then(|s| parse(&s).ok()),
            });
        }
        Ok(mailboxes)
    }

    /// Load our half of the conversation key.
    fn load_conversation_key_internal(
        &self,
        conn: &rusqlite::Connection,
        peer_id_str: &str,
    ) -> Result<Option<ConversationKey>, MailboxError> {
        let result = conn.query_row(
            "SELECT our_ephemeral_public, our_ephemeral_private, created_at
             FROM dm_conversation_keys WHERE conversation_id = ?1",
            rusqlite::params![peer_id_str],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        );

        match result {
            Ok((our_public, our_private, created_at)) => Ok(Some(ConversationKey {
                our_ephemeral_public: our_public
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid our public key length".into()))?,
                our_ephemeral_private: our_private
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid our private key length".into()))?,
                their_ephemeral_public: None,
                shared_secret: None,
                version: 1,
                created_at: DateTime::parse_from_rfc3339(&created_at)
                    .map_err(|e| MailboxError::Storage(format!("Invalid created_at: {}", e)))?
                    .with_timezone(&Utc),
                established_at: None,
            })),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MailboxError::Storage(e.to_string())),
        }
    }
}
