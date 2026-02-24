//! Per-member group mailbox state and management.
//!
//! Per-member, per-group ephemeral mailbox management for privacy-preserving group messaging.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use uuid::Uuid;

use crate::domain::{ChannelId, UserId};
use crate::error::MailboxError;
use crate::storage::Database;

/// Our ephemeral mailbox for a specific group.
///
/// Created using a blind-signed token from TokenManager.
/// Each group we're in has a unique mailbox address to prevent
/// server correlation of group membership.
#[derive(Clone)]
pub struct GroupMailbox {
    /// Unique mailbox identifier.
    pub id: String,
    /// Group this mailbox is for.
    pub group_id: ChannelId,
    /// Ed25519 public key (32 bytes) - mailbox address.
    pub public_key: [u8; 32],
    /// Ed25519 private key (64 bytes) - for signing challenges.
    pub private_key: [u8; 64],
    /// Unblinded RSA signature over the public key.
    pub signature: Vec<u8>,
    /// Message randomizer for signature verification (optional).
    pub randomizer: Option<Vec<u8>>,
    /// Token ID this mailbox was created from (for tracking).
    pub token_id: String,
    /// When this mailbox was created.
    pub created_at: DateTime<Utc>,
    /// When server will delete this mailbox.
    pub expires_at: DateTime<Utc>,
    /// Last time we polled this mailbox for messages.
    pub last_polled_at: Option<DateTime<Utc>>,
}

/// Another member's mailbox address in a group.
///
/// Received when members exchange mailbox addresses within the group.
#[derive(Clone)]
pub struct MemberMailbox {
    /// The member's user ID.
    pub user_id: UserId,
    /// Their Ed25519 public key (32 bytes) - their mailbox address.
    pub public_key: [u8; 32],
    /// When we received this address.
    pub received_at: DateTime<Utc>,
    /// When they said it expires (if known).
    pub expires_at: Option<DateTime<Utc>>,
}

/// Full mailbox state for a group.
///
/// Tracks our mailbox (for receiving) and all member mailboxes (for sending).
pub struct GroupMailboxState {
    /// Group this state is for.
    pub group_id: ChannelId,
    /// Our mailbox for receiving messages in this group.
    pub our_mailbox: Option<GroupMailbox>,
    /// Member mailboxes for sending to other group members.
    pub member_mailboxes: HashMap<UserId, MemberMailbox>,
}

impl GroupMailbox {
    /// Check if this mailbox has expired.
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }

    /// Check if this mailbox expires soon (within given duration).
    pub fn expires_soon(&self, within: Duration) -> bool {
        Utc::now() + within >= self.expires_at
    }

    /// Sign a challenge with our Ed25519 private key.
    ///
    /// Used for challenge-response authentication when polling.
    pub fn sign_challenge(&self, challenge: &[u8]) -> Result<Vec<u8>, MailboxError> {
        // Reconstruct signing key from stored bytes
        // Ed25519 "private key" is 64 bytes: 32-byte secret + 32-byte public
        if self.private_key.len() != 64 {
            return Err(MailboxError::InvalidServerKey(format!(
                "Invalid private key length: expected 64, got {}",
                self.private_key.len()
            )));
        }

        // Extract the 32-byte secret key portion
        let secret_bytes: [u8; 32] = self.private_key[..32]
            .try_into()
            .map_err(|_| MailboxError::InvalidServerKey("Failed to extract secret key".into()))?;

        let signing_key = SigningKey::from_bytes(&secret_bytes);
        let signature: Signature = signing_key.sign(challenge);

        Ok(signature.to_bytes().to_vec())
    }

    /// Get the verifying (public) key for this mailbox.
    pub fn verifying_key(&self) -> Result<VerifyingKey, MailboxError> {
        VerifyingKey::from_bytes(&self.public_key)
            .map_err(|e| MailboxError::InvalidServerKey(format!("Invalid public key: {}", e)))
    }
}

impl MemberMailbox {
    /// Create a new member mailbox from received public key.
    pub fn new(user_id: UserId, public_key: [u8; 32], expires_at: Option<DateTime<Utc>>) -> Self {
        Self {
            user_id,
            public_key,
            received_at: Utc::now(),
            expires_at,
        }
    }

    /// Check if this member mailbox might be expired.
    pub fn is_possibly_expired(&self) -> bool {
        self.expires_at.is_some_and(|exp| Utc::now() >= exp)
    }
}

impl GroupMailboxState {
    /// Create a new group mailbox state for a group.
    pub fn new(group_id: ChannelId) -> Self {
        Self {
            group_id,
            our_mailbox: None,
            member_mailboxes: HashMap::new(),
        }
    }

    /// Check if we need to create a mailbox for this group.
    pub fn needs_mailbox(&self) -> bool {
        self.our_mailbox.is_none()
    }

    /// Check if we're ready to send messages (have member mailboxes).
    pub fn is_ready_to_send(&self) -> bool {
        !self.member_mailboxes.is_empty()
    }

    /// Check if we can receive messages (have non-expired mailbox).
    pub fn is_ready_to_receive(&self) -> bool {
        self.our_mailbox
            .as_ref()
            .is_some_and(|m| !m.is_expired())
    }

    /// Get a specific member's mailbox.
    pub fn get_member_mailbox(&self, user_id: &UserId) -> Option<&MemberMailbox> {
        self.member_mailboxes.get(user_id)
    }

    /// Get count of member mailboxes.
    pub fn member_count(&self) -> usize {
        self.member_mailboxes.len()
    }

    /// Check if our mailbox needs refresh (expiring soon).
    ///
    /// Returns true if mailbox expires within 24 hours.
    pub fn mailbox_needs_refresh(&self) -> bool {
        self.our_mailbox
            .as_ref()
            .is_some_and(|m| m.expires_soon(Duration::hours(24)))
    }
}

/// Manager for group mailbox state persistence.
///
/// Handles database operations for storing and retrieving group mailbox state.
pub struct GroupMailboxManager {
    /// Database connection for state storage.
    db: Arc<Database>,
}

impl GroupMailboxManager {
    /// Create a new GroupMailboxManager.
    ///
    /// # Arguments
    ///
    /// * `db` - Shared database connection
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Load complete group state from database.
    ///
    /// Returns None if no state exists for this group.
    pub fn load_group_state(
        &self,
        group_id: &ChannelId,
    ) -> Result<Option<GroupMailboxState>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let group_id_str = group_id.to_string();

        // Load our mailbox
        let our_mailbox = self.load_our_mailbox_internal(&conn, &group_id_str, group_id)?;

        // Load member mailboxes
        let member_mailboxes = self.load_member_mailboxes_internal(&conn, &group_id_str)?;

        // If no state exists, return None
        if our_mailbox.is_none() && member_mailboxes.is_empty() {
            return Ok(None);
        }

        Ok(Some(GroupMailboxState {
            group_id: *group_id,
            our_mailbox,
            member_mailboxes,
        }))
    }

    /// Load our mailbox from database.
    fn load_our_mailbox_internal(
        &self,
        conn: &rusqlite::Connection,
        group_id_str: &str,
        group_id: &ChannelId,
    ) -> Result<Option<GroupMailbox>, MailboxError> {
        let mut stmt = conn
            .prepare(
                "SELECT id, mailbox_public_key, mailbox_private_key, signature, randomizer,
                        token_id, created_at, expires_at, last_polled_at
                 FROM group_mailboxes WHERE group_id = ?1",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![group_id_str], |row| {
            let id: String = row.get(0)?;
            let public_key: Vec<u8> = row.get(1)?;
            let private_key: Vec<u8> = row.get(2)?;
            let signature: Vec<u8> = row.get(3)?;
            let randomizer: Option<Vec<u8>> = row.get(4)?;
            let token_id: String = row.get(5)?;
            let created_at: String = row.get(6)?;
            let expires_at: String = row.get(7)?;
            let last_polled_at: Option<String> = row.get(8)?;

            Ok((
                id,
                public_key,
                private_key,
                signature,
                randomizer,
                token_id,
                created_at,
                expires_at,
                last_polled_at,
            ))
        });

        match result {
            Ok((
                id,
                public_key,
                private_key,
                signature,
                randomizer,
                token_id,
                created_at,
                expires_at,
                last_polled_at,
            )) => {
                let public_key_arr: [u8; 32] = public_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid public key length".into()))?;
                let private_key_arr: [u8; 64] = private_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid private key length".into()))?;

                let created_at = DateTime::parse_from_rfc3339(&created_at)
                    .map_err(|e| MailboxError::Storage(format!("Invalid created_at: {}", e)))?
                    .with_timezone(&Utc);
                let expires_at = DateTime::parse_from_rfc3339(&expires_at)
                    .map_err(|e| MailboxError::Storage(format!("Invalid expires_at: {}", e)))?
                    .with_timezone(&Utc);
                let last_polled_at = last_polled_at
                    .and_then(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .map(|dt| dt.with_timezone(&Utc))
                            .ok()
                    });

                Ok(Some(GroupMailbox {
                    id,
                    group_id: *group_id,
                    public_key: public_key_arr,
                    private_key: private_key_arr,
                    signature,
                    randomizer,
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

    /// Load member mailboxes from database.
    fn load_member_mailboxes_internal(
        &self,
        conn: &rusqlite::Connection,
        group_id_str: &str,
    ) -> Result<HashMap<UserId, MemberMailbox>, MailboxError> {
        let mut stmt = conn
            .prepare(
                "SELECT user_id, mailbox_public_key, received_at, expires_at
                 FROM group_member_mailboxes WHERE group_id = ?1",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut rows = stmt
            .query(rusqlite::params![group_id_str])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut members = HashMap::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| MailboxError::Storage(e.to_string()))?
        {
            let user_id_str: String = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let public_key: Vec<u8> = row
                .get(1)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let received_at_str: String = row
                .get(2)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let expires_at_str: Option<String> = row
                .get(3)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            let user_id: UserId = user_id_str
                .parse()
                .map_err(|e: uuid::Error| MailboxError::Storage(format!("Invalid user_id: {}", e)))?;

            let public_key_arr: [u8; 32] = public_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid member public key length".into()))?;

            let received_at = DateTime::parse_from_rfc3339(&received_at_str)
                .map_err(|e| MailboxError::Storage(format!("Invalid received_at: {}", e)))?
                .with_timezone(&Utc);

            let expires_at = expires_at_str
                .and_then(|s| {
                    DateTime::parse_from_rfc3339(&s)
                        .map(|dt| dt.with_timezone(&Utc))
                        .ok()
                });

            members.insert(
                user_id,
                MemberMailbox {
                    user_id,
                    public_key: public_key_arr,
                    received_at,
                    expires_at,
                },
            );
        }

        Ok(members)
    }

    /// Save/update our mailbox for a group.
    pub fn save_our_mailbox(&self, mailbox: &GroupMailbox) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let group_id_str = mailbox.group_id.to_string();

        conn.execute(
            "INSERT OR REPLACE INTO group_mailboxes
             (id, group_id, mailbox_public_key, mailbox_private_key, signature,
              randomizer, token_id, created_at, expires_at, last_polled_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                mailbox.id,
                group_id_str,
                mailbox.public_key.as_slice(),
                mailbox.private_key.as_slice(),
                mailbox.signature,
                mailbox.randomizer,
                mailbox.token_id,
                mailbox.created_at.to_rfc3339(),
                mailbox.expires_at.to_rfc3339(),
                mailbox.last_polled_at.map(|dt| dt.to_rfc3339())
            ],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Save/update a member's mailbox address.
    pub fn save_member_mailbox(
        &self,
        group_id: &ChannelId,
        mailbox: &MemberMailbox,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let group_id_str = group_id.to_string();
        let user_id_str = mailbox.user_id.to_string();
        let id = Uuid::new_v4().to_string();

        conn.execute(
            "INSERT OR REPLACE INTO group_member_mailboxes
             (id, group_id, user_id, mailbox_public_key, received_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                id,
                group_id_str,
                user_id_str,
                mailbox.public_key.as_slice(),
                mailbox.received_at.to_rfc3339(),
                mailbox.expires_at.map(|dt| dt.to_rfc3339())
            ],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Remove a member's mailbox.
    pub fn remove_member_mailbox(
        &self,
        group_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let group_id_str = group_id.to_string();
        let user_id_str = user_id.to_string();

        conn.execute(
            "DELETE FROM group_member_mailboxes WHERE group_id = ?1 AND user_id = ?2",
            rusqlite::params![group_id_str, user_id_str],
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
                "UPDATE group_mailboxes SET last_polled_at = ?1 WHERE id = ?2",
                rusqlite::params![now, mailbox_id],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        if rows == 0 {
            return Err(MailboxError::TokenNotFound(mailbox_id.to_string()));
        }

        Ok(())
    }

    /// Get all group mailboxes for batch polling.
    ///
    /// Returns (group_id, public_key, private_key, mailbox_id) for all active groups.
    #[allow(clippy::type_complexity)]
    pub fn get_all_group_mailboxes(
        &self,
    ) -> Result<Vec<(ChannelId, [u8; 32], [u8; 64], String)>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now = Utc::now().to_rfc3339();

        let mut stmt = conn
            .prepare(
                "SELECT group_id, mailbox_public_key, mailbox_private_key, id
                 FROM group_mailboxes WHERE expires_at > ?1",
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
            let group_id_str: String = row
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

            let group_id: ChannelId = group_id_str
                .parse()
                .map_err(|e: uuid::Error| MailboxError::Storage(format!("Invalid group_id: {}", e)))?;

            let public_key_arr: [u8; 32] = public_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid public key length".into()))?;
            let private_key_arr: [u8; 64] = private_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid private key length".into()))?;

            results.push((group_id, public_key_arr, private_key_arr, mailbox_id));
        }

        Ok(results)
    }

    /// Get groups with mailboxes expiring soon.
    ///
    /// Returns group IDs of groups needing mailbox refresh.
    pub fn get_expiring_group_mailboxes(
        &self,
        within_hours: i64,
    ) -> Result<Vec<ChannelId>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let threshold = (Utc::now() + Duration::hours(within_hours)).to_rfc3339();
        let now = Utc::now().to_rfc3339();

        let mut stmt = conn
            .prepare(
                "SELECT group_id FROM group_mailboxes
                 WHERE expires_at > ?1 AND expires_at <= ?2",
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
            let group_id_str: String = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            let group_id: ChannelId = group_id_str
                .parse()
                .map_err(|e: uuid::Error| MailboxError::Storage(format!("Invalid group_id: {}", e)))?;

            results.push(group_id);
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
                "DELETE FROM group_mailboxes WHERE expires_at <= ?1",
                rusqlite::params![now],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(rows as u32)
    }

    /// List all active groups (groups where we have a non-expired mailbox).
    ///
    /// Returns list of group IDs.
    pub fn list_active_groups(&self) -> Result<Vec<ChannelId>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now = Utc::now().to_rfc3339();

        let mut stmt = conn
            .prepare("SELECT DISTINCT group_id FROM group_mailboxes WHERE expires_at > ?1")
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut rows = stmt
            .query(rusqlite::params![now])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut results = Vec::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| MailboxError::Storage(e.to_string()))?
        {
            let group_id_str: String = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            let group_id: ChannelId = group_id_str
                .parse()
                .map_err(|e: uuid::Error| MailboxError::Storage(format!("Invalid group_id: {}", e)))?;

            results.push(group_id);
        }

        Ok(results)
    }

    /// Poll a group's mailbox for messages.
    ///
    /// This is a placeholder that returns the count of messages.
    /// Actual implementation would call the mailbox client.
    ///
    /// Returns the number of messages retrieved.
    pub async fn poll_group_mailbox(&self, _group_id: &ChannelId) -> Result<u32, MailboxError> {
        // TODO: Implement actual mailbox polling via MailboxClient
        // For now, return 0 messages (no-op poll)
        Ok(0)
    }

    /// Delete all state for a group.
    ///
    /// Useful when user leaves a group.
    pub fn delete_group_state(&self, group_id: &ChannelId) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let group_id_str = group_id.to_string();

        // Delete from both tables
        conn.execute(
            "DELETE FROM group_mailboxes WHERE group_id = ?1",
            rusqlite::params![group_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        conn.execute(
            "DELETE FROM group_member_mailboxes WHERE group_id = ?1",
            rusqlite::params![group_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_group_mailbox_state_new() {
        let group_id = ChannelId::new();
        let state = GroupMailboxState::new(group_id);

        assert_eq!(state.group_id, group_id);
        assert!(state.our_mailbox.is_none());
        assert!(state.member_mailboxes.is_empty());
    }

    #[test]
    fn test_group_mailbox_state_needs_mailbox() {
        let group_id = ChannelId::new();
        let state = GroupMailboxState::new(group_id);

        assert!(state.needs_mailbox());
        assert!(!state.is_ready_to_send());
        assert!(!state.is_ready_to_receive());
    }

    #[test]
    fn test_member_mailbox_new() {
        let user_id = UserId::new();
        let public_key = [0u8; 32];
        let expires_at = Some(Utc::now() + Duration::days(7));

        let mailbox = MemberMailbox::new(user_id, public_key, expires_at);

        assert_eq!(mailbox.user_id, user_id);
        assert_eq!(mailbox.public_key, public_key);
        assert!(mailbox.expires_at.is_some());
        assert!(!mailbox.is_possibly_expired());
    }

    #[test]
    fn test_member_mailbox_expired() {
        let user_id = UserId::new();
        let public_key = [0u8; 32];
        let expires_at = Some(Utc::now() - Duration::hours(1));

        let mailbox = MemberMailbox::new(user_id, public_key, expires_at);

        assert!(mailbox.is_possibly_expired());
    }

    #[test]
    fn test_group_mailbox_expires_soon() {
        let mailbox = GroupMailbox {
            id: "test".to_string(),
            group_id: ChannelId::new(),
            public_key: [0u8; 32],
            private_key: [0u8; 64],
            signature: vec![],
            randomizer: None,
            token_id: "token".to_string(),
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::hours(12),
            last_polled_at: None,
        };

        assert!(!mailbox.is_expired());
        assert!(mailbox.expires_soon(Duration::hours(24)));
        assert!(!mailbox.expires_soon(Duration::hours(6)));
    }

    #[test]
    fn test_group_mailbox_is_expired() {
        let mailbox = GroupMailbox {
            id: "test".to_string(),
            group_id: ChannelId::new(),
            public_key: [0u8; 32],
            private_key: [0u8; 64],
            signature: vec![],
            randomizer: None,
            token_id: "token".to_string(),
            created_at: Utc::now() - Duration::days(8),
            expires_at: Utc::now() - Duration::hours(1),
            last_polled_at: None,
        };

        assert!(mailbox.is_expired());
    }

    #[test]
    fn test_group_mailbox_state_ready_states() {
        let group_id = ChannelId::new();
        let mut state = GroupMailboxState::new(group_id);

        // Add our mailbox
        state.our_mailbox = Some(GroupMailbox {
            id: "test".to_string(),
            group_id,
            public_key: [0u8; 32],
            private_key: [0u8; 64],
            signature: vec![],
            randomizer: None,
            token_id: "token".to_string(),
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(7),
            last_polled_at: None,
        });

        assert!(!state.needs_mailbox());
        assert!(state.is_ready_to_receive());
        assert!(!state.is_ready_to_send()); // No member mailboxes yet

        // Add a member mailbox
        let member_id = UserId::new();
        state.member_mailboxes.insert(
            member_id,
            MemberMailbox::new(member_id, [0u8; 32], None),
        );

        assert!(state.is_ready_to_send());
        assert_eq!(state.member_count(), 1);
        assert!(state.get_member_mailbox(&member_id).is_some());
    }

    #[test]
    fn test_group_mailbox_state_mailbox_needs_refresh() {
        let group_id = ChannelId::new();
        let mut state = GroupMailboxState::new(group_id);

        // No mailbox - doesn't need refresh
        assert!(!state.mailbox_needs_refresh());

        // Add mailbox expiring soon
        state.our_mailbox = Some(GroupMailbox {
            id: "test".to_string(),
            group_id,
            public_key: [0u8; 32],
            private_key: [0u8; 64],
            signature: vec![],
            randomizer: None,
            token_id: "token".to_string(),
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::hours(12),
            last_polled_at: None,
        });

        assert!(state.mailbox_needs_refresh());

        // Mailbox not expiring soon
        state.our_mailbox = Some(GroupMailbox {
            id: "test2".to_string(),
            group_id,
            public_key: [0u8; 32],
            private_key: [0u8; 64],
            signature: vec![],
            randomizer: None,
            token_id: "token".to_string(),
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(7),
            last_polled_at: None,
        });

        assert!(!state.mailbox_needs_refresh());
    }
}
