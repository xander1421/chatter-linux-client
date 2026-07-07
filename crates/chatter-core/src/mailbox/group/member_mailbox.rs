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
use crate::mailbox::dm::ReceivedMessage;
use crate::network::grpc::MailboxClient;
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

    /// Load our mailbox from database (server-managed groups only).
    ///
    /// For anonymous groups, use `load_anon_group_mailbox` instead.
    fn load_our_mailbox_internal(
        &self,
        conn: &rusqlite::Connection,
        group_id_str: &str,
        group_id: &ChannelId,
    ) -> Result<Option<GroupMailbox>, MailboxError> {
        let mut stmt = conn
            .prepare(
                // During rotation two mailboxes coexist for a group; the NEWEST is
                // the active one that sends must target. (All non-expired rows are
                // still polled for receive via get_all_group_mailboxes.)
                "SELECT id, mailbox_public_key, mailbox_private_key, signature, randomizer,
                        token_id, created_at, expires_at, last_polled_at
                 FROM group_mailboxes WHERE group_id = ?1
                 ORDER BY created_at DESC LIMIT 1",
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
        tracing::info!(
            group_id = %mailbox.group_id,
            mailbox_id = %mailbox.id,
            our_pk_hex = %hex::encode(mailbox.public_key),
            expires_at = %mailbox.expires_at.to_rfc3339(),
            "[GROUP_MAILBOX] Saving our mailbox to database"
        );

        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let group_id_str = mailbox.group_id.to_string();

        let rows_affected = conn.execute(
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

        tracing::info!(
            group_id = %mailbox.group_id,
            mailbox_id = %mailbox.id,
            rows_affected = rows_affected,
            "[GROUP_MAILBOX] Mailbox saved successfully"
        );

        Ok(())
    }

    /// Save/update a member's mailbox address.
    pub fn save_member_mailbox(
        &self,
        group_id: &ChannelId,
        mailbox: &MemberMailbox,
    ) -> Result<(), MailboxError> {
        // Debug: Log the member mailbox being stored
        tracing::info!(
            group_id = %group_id,
            member_id = %mailbox.user_id,
            member_pk_hex = %hex::encode(mailbox.public_key),
            "[STORE_MEMBER_MAILBOX] Storing member mailbox"
        );

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
    ///
    /// Tries both `group_mailboxes` (server-managed) and `anon_group_mailboxes` (anonymous groups).
    pub fn update_poll_time(&self, mailbox_id: &str) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now = Utc::now().to_rfc3339();

        // Try server-managed group mailboxes first
        let rows = conn
            .execute(
                "UPDATE group_mailboxes SET last_polled_at = ?1 WHERE id = ?2",
                rusqlite::params![now, mailbox_id],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        if rows > 0 {
            return Ok(());
        }

        // Try anonymous group mailboxes
        let rows = conn
            .execute(
                "UPDATE anon_group_mailboxes SET last_polled_at = ?1 WHERE id = ?2",
                rusqlite::params![now, mailbox_id],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        if rows > 0 {
            return Ok(());
        }

        Err(MailboxError::TokenNotFound(mailbox_id.to_string()))
    }

    /// Get mailbox keys by mailbox ID.
    ///
    /// Looks up keys from both `group_mailboxes` and `anon_group_mailboxes` tables.
    /// Returns (public_key, private_key) if found.
    pub fn get_mailbox_keys_by_id(
        &self,
        mailbox_id: &str,
    ) -> Result<([u8; 32], [u8; 64]), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        // Try server-managed group mailboxes first
        let result: Result<(Vec<u8>, Vec<u8>), _> = conn.query_row(
            "SELECT mailbox_public_key, mailbox_private_key FROM group_mailboxes WHERE id = ?1",
            rusqlite::params![mailbox_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );

        if let Ok((public_key, private_key)) = result {
            let public_key_arr: [u8; 32] = public_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid public key length".into()))?;
            let private_key_arr: [u8; 64] = private_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid private key length".into()))?;
            return Ok((public_key_arr, private_key_arr));
        }

        // Try anonymous group mailboxes
        let result: Result<(Vec<u8>, Vec<u8>), _> = conn.query_row(
            "SELECT mailbox_public_key, mailbox_private_key FROM anon_group_mailboxes WHERE id = ?1",
            rusqlite::params![mailbox_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );

        match result {
            Ok((public_key, private_key)) => {
                let public_key_arr: [u8; 32] = public_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid public key length".into()))?;
                let private_key_arr: [u8; 64] = private_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid private key length".into()))?;
                Ok((public_key_arr, private_key_arr))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                Err(MailboxError::TokenNotFound(mailbox_id.to_string()))
            }
            Err(e) => Err(MailboxError::Storage(e.to_string())),
        }
    }

    /// Get all group mailboxes for batch polling.
    ///
    /// Returns (group_id, public_key, private_key, mailbox_id) for all active groups.
    /// Queries both server-managed groups (group_mailboxes) and anonymous groups (anon_group_mailboxes).
    #[allow(clippy::type_complexity)]
    pub fn get_all_group_mailboxes(
        &self,
    ) -> Result<Vec<(ChannelId, [u8; 32], [u8; 64], String)>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let now = Utc::now().to_rfc3339();

        // Count total rows from both tables (for debugging)
        let server_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM group_mailboxes",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let anon_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM anon_group_mailboxes",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        tracing::debug!(
            server_count = server_count,
            anon_count = anon_count,
            now = %now,
            "[GROUP_MAILBOX] Querying all group mailboxes (server: {}, anon: {})",
            server_count,
            anon_count
        );

        // Query from both tables using UNION
        // Both group_mailboxes.group_id and anon_group_mailboxes.group_id are TEXT UUIDs.
        let mut stmt = conn
            .prepare(
                "SELECT group_id, mailbox_public_key, mailbox_private_key, id, expires_at, 'server' as source
                 FROM group_mailboxes WHERE expires_at > ?1
                 UNION ALL
                 SELECT group_id, mailbox_public_key, mailbox_private_key, id, expires_at, 'anon' as source
                 FROM anon_group_mailboxes WHERE expires_at > ?1",
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
            let expires_at: String = row
                .get(4)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let source: String = row
                .get(5)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            tracing::debug!(
                group_id = %group_id_str,
                mailbox_id = %mailbox_id,
                expires_at = %expires_at,
                source = %source,
                "[GROUP_MAILBOX] Found active mailbox"
            );

            // Parse group_id - server groups are UUID strings, anon groups are hex-encoded bytes
            let group_id: ChannelId = if group_id_str.contains('-') {
                // Standard UUID format: "8c3cbbe0-3342-4bd8-bbd5-ce669d986cbe"
                group_id_str
                    .parse()
                    .map_err(|e: uuid::Error| MailboxError::Storage(format!("Invalid group_id: {}", e)))?
            } else {
                // Hex-encoded 16-byte UUID from anon_group_mailboxes: "8C3CBBE033424BD8BBD5CE669D986CBE"
                let bytes = hex::decode(&group_id_str)
                    .map_err(|e| MailboxError::Storage(format!("Invalid hex group_id: {}", e)))?;
                let bytes_arr: [u8; 16] = bytes
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid group_id length".into()))?;
                uuid::Uuid::from_bytes(bytes_arr).into()
            };

            let public_key_arr: [u8; 32] = public_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid public key length".into()))?;
            let private_key_arr: [u8; 64] = private_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid private key length".into()))?;

            results.push((group_id, public_key_arr, private_key_arr, mailbox_id));
        }

        if results.is_empty() {
            tracing::trace!(
                "[GROUP_MAILBOX] No active group mailboxes for polling"
            );
        } else {
            tracing::info!(
                count = results.len(),
                "[GROUP_MAILBOX] Returning {} active group mailboxes for polling",
                results.len()
            );
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

        // Re-rotation guard: a group is a candidate only if its NEWEST row is
        // expiring within the window. Once rotation has minted a fresh row
        // (expires_at > threshold) the group is excluded, so a lingering old
        // row in the overlap window can't re-trigger rotation every cycle and
        // drain the token budget (gate G5).
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT group_id FROM group_mailboxes
                 WHERE expires_at > ?1 AND expires_at <= ?2
                   AND group_id NOT IN (
                       SELECT group_id FROM group_mailboxes WHERE expires_at > ?2
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
    /// Performs challenge-response authentication against the server and
    /// fetches pending encrypted envelopes for this group's mailbox.
    ///
    /// # Arguments
    ///
    /// * `group_id` - Group to poll
    /// * `client` - Mailbox client for server communication
    ///
    /// # Returns
    ///
    /// Vector of received encrypted messages and a boolean indicating more messages may be available.
    pub async fn poll_group_mailbox(
        &self,
        group_id: &ChannelId,
        client: &mut MailboxClient,
    ) -> Result<(Vec<ReceivedMessage>, bool), MailboxError> {
        // Load our mailbox state for this group
        let group_state = self
            .load_group_state(group_id)?
            .ok_or(MailboxError::GroupNotFound)?;

        let our_mailbox = group_state
            .our_mailbox
            .ok_or(MailboxError::NoMailboxConfigured)?;

        // Check expiration
        if our_mailbox.is_expired() {
            return Err(MailboxError::TokenExpired);
        }

        // Challenge-response authentication
        let challenge_response = client
            .get_mailbox_challenge(our_mailbox.public_key.to_vec())
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        let signature = our_mailbox.sign_challenge(&challenge_response.challenge)?;

        // Poll for messages
        let poll_limit = 50i32;
        let poll_response = client
            .poll_ephemeral_mailbox(
                our_mailbox.public_key.to_vec(),
                signature,
                challenge_response.challenge,
                None,
                poll_limit,
            )
            .await
            .map_err(|e| MailboxError::Network(e.to_string()))?;

        // Update poll timestamp
        self.update_poll_time(&our_mailbox.id)?;

        // Convert to ReceivedMessage
        let messages: Vec<ReceivedMessage> = poll_response
            .messages
            .into_iter()
            .map(|m| ReceivedMessage {
                message_id: m.id,
                encrypted_envelope: m.encrypted_envelope,
                envelope_type: m.envelope_type,
                server_timestamp: m.timestamp,
            })
            .collect();

        let has_more = messages.len() == poll_limit as usize;

        Ok((messages, has_more))
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

        // Also delete pending channel messages
        conn.execute(
            "DELETE FROM pending_channel_messages WHERE channel_id = ?1",
            rusqlite::params![group_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    // =========================================================================
    // Pending Channel Message Queue (Bootstrap Support)
    // =========================================================================

    /// Queue a pending message for a member whose mailbox isn't available yet.
    ///
    /// Messages are queued during channel bootstrap when member mailboxes
    /// haven't been exchanged yet. They're flushed when the member's mailbox
    /// address is received via GroupMailboxAddress.
    ///
    /// # Arguments
    ///
    /// * `channel_id` - Channel the message is for
    /// * `member_id` - Target member who we don't have a mailbox for yet
    /// * `plaintext` - Encrypted message content (will be re-encrypted when flushed)
    /// * `timestamp` - Message timestamp (for ordering and TTL)
    pub fn queue_pending_message(
        &self,
        channel_id: &ChannelId,
        member_id: &UserId,
        plaintext: &[u8],
        timestamp: u64,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let channel_id_str = channel_id.to_string();
        let member_id_str = member_id.to_string();
        let now = Utc::now().to_rfc3339();

        conn.execute(
            "INSERT INTO pending_channel_messages (channel_id, member_id, plaintext, timestamp, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![channel_id_str, member_id_str, plaintext, timestamp as i64, now],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        tracing::info!(
            channel_id = %channel_id,
            member_id = %member_id,
            timestamp = timestamp,
            "[PENDING_CHANNEL_MSG] Queued message for member (mailbox not yet available)"
        );

        Ok(())
    }

    /// Drain all pending messages for a specific channel/member.
    ///
    /// Called when we receive a member's mailbox address. Returns all queued
    /// messages and deletes them from the queue.
    ///
    /// # Returns
    ///
    /// Vector of (plaintext, timestamp) tuples for messages to send.
    pub fn drain_pending_messages(
        &self,
        channel_id: &ChannelId,
        member_id: &UserId,
    ) -> Result<Vec<(Vec<u8>, u64)>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let channel_id_str = channel_id.to_string();
        let member_id_str = member_id.to_string();

        // Load pending messages
        let mut stmt = conn
            .prepare(
                "SELECT plaintext, timestamp FROM pending_channel_messages
                 WHERE channel_id = ?1 AND member_id = ?2
                 ORDER BY timestamp ASC",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut rows = stmt
            .query(rusqlite::params![channel_id_str, member_id_str])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut messages = Vec::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| MailboxError::Storage(e.to_string()))?
        {
            let plaintext: Vec<u8> = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let timestamp: i64 = row
                .get(1)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            messages.push((plaintext, timestamp as u64));
        }

        // Drop the rows before executing delete
        drop(rows);
        drop(stmt);

        // Delete the messages
        if !messages.is_empty() {
            conn.execute(
                "DELETE FROM pending_channel_messages WHERE channel_id = ?1 AND member_id = ?2",
                rusqlite::params![channel_id_str, member_id_str],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

            tracing::info!(
                channel_id = %channel_id,
                member_id = %member_id,
                count = messages.len(),
                "[PENDING_CHANNEL_MSG] Drained {} pending message(s) for member",
                messages.len()
            );
        }

        Ok(messages)
    }

    /// Get count of pending messages for a channel.
    ///
    /// Useful for debugging and monitoring bootstrap progress.
    pub fn pending_message_count(&self, channel_id: &ChannelId) -> Result<usize, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let channel_id_str = channel_id.to_string();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pending_channel_messages WHERE channel_id = ?1",
                rusqlite::params![channel_id_str],
                |row| row.get(0),
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(count as usize)
    }

    /// Cleanup old pending messages (TTL expired).
    ///
    /// Removes pending messages older than the specified TTL.
    /// Default TTL is 1 hour (same as DM pending messages).
    ///
    /// # Arguments
    ///
    /// * `max_age_secs` - Maximum age in seconds (messages older than this are deleted)
    ///
    /// # Returns
    ///
    /// Number of messages cleaned up.
    pub fn cleanup_old_pending_messages(&self, max_age_secs: i64) -> Result<u32, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        // Calculate cutoff timestamp (now - max_age in nanoseconds)
        let cutoff_nanos = (Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64)
            .saturating_sub((max_age_secs as u64) * 1_000_000_000);

        let rows = conn
            .execute(
                "DELETE FROM pending_channel_messages WHERE timestamp < ?1",
                rusqlite::params![cutoff_nanos as i64],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        if rows > 0 {
            tracing::info!(
                count = rows,
                max_age_secs = max_age_secs,
                "[PENDING_CHANNEL_MSG] Cleaned up {} expired pending message(s)",
                rows
            );
        }

        Ok(rows as u32)
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

    // ========================================================================
    // L4 group rotation: two-mailbox overlap semantics (DB-backed)
    // ========================================================================

    /// The token every test mailbox references (group_mailboxes.token_id FK →
    /// mailbox_tokens.id → mailbox_server_keys.key_id). Seeded by `setup_db`.
    const TEST_TOKEN_ID: &str = "test-token";

    fn setup_db() -> Arc<Database> {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("group_mailbox_test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();

        // Seed the FK parents so saving a mailbox (which references a token)
        // satisfies group_mailboxes.token_id → mailbox_tokens.id → server key.
        let conn = db.connection().unwrap();
        conn.execute(
            "INSERT INTO mailbox_server_keys (key_id, public_key_der, valid_until, created_at)
             VALUES ('test-key', X'00', 0, ?1)",
            rusqlite::params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mailbox_tokens
             (id, ephemeral_public_key, ephemeral_private_key, signature, server_key_id, created_at)
             VALUES (?1, X'01', X'02', X'03', 'test-key', ?2)",
            rusqlite::params![TEST_TOKEN_ID, Utc::now().to_rfc3339()],
        )
        .unwrap();
        drop(conn);

        Box::leak(Box::new(temp_dir));
        db
    }

    fn make_group_mailbox(
        id: &str,
        group_id: ChannelId,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> GroupMailbox {
        GroupMailbox {
            id: id.to_string(),
            group_id,
            // Distinct public key per id so callers can assert which row won.
            public_key: [id.as_bytes()[0]; 32],
            private_key: [7u8; 64],
            signature: vec![1, 2, 3],
            randomizer: None,
            token_id: TEST_TOKEN_ID.to_string(),
            created_at,
            expires_at,
            last_polled_at: None,
        }
    }

    #[test]
    fn test_two_group_mailboxes_both_polled() {
        // Two coexisting mailbox rows for one group → get_all_group_mailboxes
        // returns BOTH (overlap polling is automatic once two rows coexist).
        let db = setup_db();
        let mgr = GroupMailboxManager::new(db);
        let group = ChannelId::new();

        let now = Utc::now();
        let old = make_group_mailbox("a", group, now - Duration::hours(2), now + Duration::hours(22));
        let new = make_group_mailbox("b", group, now, now + Duration::hours(24));
        mgr.save_our_mailbox(&old).unwrap();
        mgr.save_our_mailbox(&new).unwrap();

        let active = mgr.get_all_group_mailboxes().unwrap();
        let ids: Vec<String> = active.iter().map(|(_, _, _, id)| id.clone()).collect();
        assert_eq!(active.len(), 2, "both non-expired rows must be polled");
        assert!(ids.contains(&"a".to_string()));
        assert!(ids.contains(&"b".to_string()));
    }

    #[test]
    fn test_group_send_targets_newest_mailbox() {
        // load_group_state().our_mailbox (via load_our_mailbox_internal) returns
        // the NEWEST row (larger created_at) so sends target the rotated mailbox.
        let db = setup_db();
        let mgr = GroupMailboxManager::new(db);
        let group = ChannelId::new();

        let now = Utc::now();
        let old = make_group_mailbox("a", group, now - Duration::hours(2), now + Duration::hours(22));
        let new = make_group_mailbox("b", group, now, now + Duration::hours(24));
        // Insert old last to prove ordering is by created_at, not insertion order.
        mgr.save_our_mailbox(&new).unwrap();
        mgr.save_our_mailbox(&old).unwrap();

        let state = mgr.load_group_state(&group).unwrap().unwrap();
        let our = state.our_mailbox.expect("our mailbox present");
        assert_eq!(our.id, "b", "send mailbox must be the newest by created_at");
    }

    #[test]
    fn test_expired_group_row_excluded() {
        // A row past its expires_at is excluded from active polling — the old
        // mailbox retires naturally once its TTL elapses.
        let db = setup_db();
        let mgr = GroupMailboxManager::new(db);
        let group = ChannelId::new();

        let now = Utc::now();
        let expired = make_group_mailbox("a", group, now - Duration::hours(48), now - Duration::hours(1));
        let live = make_group_mailbox("b", group, now, now + Duration::hours(24));
        mgr.save_our_mailbox(&expired).unwrap();
        mgr.save_our_mailbox(&live).unwrap();

        let active = mgr.get_all_group_mailboxes().unwrap();
        let ids: Vec<String> = active.iter().map(|(_, _, _, id)| id.clone()).collect();
        assert_eq!(active.len(), 1, "expired row must be excluded from polling");
        assert_eq!(ids, vec!["b".to_string()]);
    }

    #[test]
    fn test_group_rotation_candidate_not_reselected_when_fresh() {
        // Re-rotation guard (gate G5): while only an expiring row exists the
        // group is a candidate; once a fresh (post-rotation) row coexists the
        // group is NO LONGER selected — so a lingering old row can't re-trigger
        // rotation every cycle and drain the token budget. Also proves the old
        // row survives the rotate (loss-free overlap).
        let db = setup_db();
        let mgr = GroupMailboxManager::new(db);
        let group = ChannelId::new();

        let now = Utc::now();
        // Threshold is 24h; this row expires in 12h → within the window.
        let expiring = make_group_mailbox("a", group, now - Duration::hours(36), now + Duration::hours(12));
        mgr.save_our_mailbox(&expiring).unwrap();

        let candidates = mgr.get_expiring_group_mailboxes(24).unwrap();
        assert_eq!(candidates, vec![group], "expiring group must be a rotation candidate");

        // Rotation mints a fresh row alongside the old (both coexist).
        let fresh = make_group_mailbox("b", group, now, now + Duration::hours(48));
        mgr.save_our_mailbox(&fresh).unwrap();

        // Old row survives (loss-free overlap) …
        let active = mgr.get_all_group_mailboxes().unwrap();
        assert_eq!(active.len(), 2, "old row must survive rotation for overlap");

        // … but the group is no longer re-selected for rotation.
        let candidates = mgr.get_expiring_group_mailboxes(24).unwrap();
        assert!(
            candidates.is_empty(),
            "group with a fresh row must not be re-selected for rotation"
        );
    }
}
