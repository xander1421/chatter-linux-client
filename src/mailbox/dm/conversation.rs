//! DM conversation state and management.
//!
//! Per-conversation ephemeral mailbox and key state for two-way anonymity.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use uuid::Uuid;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};

use crate::domain::UserId;
use crate::error::MailboxError;
use crate::storage::Database;

/// State of a DM conversation's mailbox system.
///
/// Tracks our ephemeral mailbox (for receiving), peer's mailbox address (for sending),
/// and shared conversation keys (for encryption).
pub struct DmConversation {
    /// Peer user ID (conversation_id).
    pub peer_id: UserId,
    /// Our mailbox for receiving messages from peer.
    pub our_mailbox: Option<DmMailbox>,
    /// Peer's mailbox for sending messages to them.
    pub peer_mailbox: Option<PeerMailbox>,
    /// Conversation encryption keys.
    pub conversation_key: Option<ConversationKey>,
}

/// Our ephemeral mailbox for this conversation.
///
/// Created using a blind-signed token from TokenManager.
#[derive(Clone)]
pub struct DmMailbox {
    /// Unique mailbox identifier.
    pub id: String,
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

/// Peer's mailbox address (for sending to them).
///
/// Received during DM bootstrap or key exchange.
#[derive(Clone)]
pub struct PeerMailbox {
    /// Peer's Ed25519 public key (32 bytes) - their mailbox address.
    pub public_key: [u8; 32],
    /// When we received this address.
    pub received_at: DateTime<Utc>,
    /// When they said it expires (if known).
    pub expires_at: Option<DateTime<Utc>>,
}

/// Conversation encryption keys (X25519 derived).
///
/// Used to derive shared secrets for message encryption/decryption.
#[derive(Clone)]
pub struct ConversationKey {
    /// Our ephemeral X25519 public key (32 bytes).
    pub our_ephemeral_public: [u8; 32],
    /// Our ephemeral X25519 private key (32 bytes).
    pub our_ephemeral_private: [u8; 32],
    /// Their ephemeral X25519 public key (NULL until received).
    pub their_ephemeral_public: Option<[u8; 32]>,
    /// X25519 shared secret (NULL until derived).
    pub shared_secret: Option<[u8; 32]>,
    /// Key version for rotation.
    pub version: u32,
    /// When this key was created.
    pub created_at: DateTime<Utc>,
    /// When both keys were present and secret derived.
    pub established_at: Option<DateTime<Utc>>,
}

impl DmConversation {
    /// Create a new DM conversation state for a peer.
    pub fn new(peer_id: UserId) -> Self {
        Self {
            peer_id,
            our_mailbox: None,
            peer_mailbox: None,
            conversation_key: None,
        }
    }

    /// Check if we can send messages to this peer.
    ///
    /// Requires peer mailbox address and established conversation key.
    pub fn is_ready_to_send(&self) -> bool {
        self.peer_mailbox.is_some()
            && self
                .conversation_key
                .as_ref()
                .is_some_and(|k| k.shared_secret.is_some())
    }

    /// Check if we can receive messages from this peer.
    ///
    /// Requires our mailbox to be active.
    pub fn is_ready_to_receive(&self) -> bool {
        self.our_mailbox
            .as_ref()
            .is_some_and(|m| !m.is_expired())
    }

    /// Check if this conversation needs initial bootstrap.
    ///
    /// True if we don't have our own mailbox yet.
    pub fn needs_bootstrap(&self) -> bool {
        self.our_mailbox.is_none()
    }

    /// Check if our mailbox needs refresh (expiring soon).
    ///
    /// Returns true if mailbox expires within 24 hours.
    pub fn mailbox_needs_refresh(&self) -> bool {
        self.our_mailbox
            .as_ref()
            .is_some_and(|m| m.expires_soon(Duration::hours(24)))
    }

    /// Check if peer's mailbox might be expired.
    ///
    /// Returns true if peer mailbox has known expiry that has passed.
    pub fn peer_mailbox_expired(&self) -> bool {
        self.peer_mailbox.as_ref().is_some_and(|p| {
            p.expires_at
                .is_some_and(|exp| exp < Utc::now())
        })
    }
}

impl DmMailbox {
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

impl PeerMailbox {
    /// Create a new peer mailbox from received public key.
    pub fn new(public_key: [u8; 32], expires_at: Option<DateTime<Utc>>) -> Self {
        Self {
            public_key,
            received_at: Utc::now(),
            expires_at,
        }
    }

    /// Check if this peer mailbox might be expired.
    pub fn is_possibly_expired(&self) -> bool {
        self.expires_at.is_some_and(|exp| Utc::now() >= exp)
    }
}

impl ConversationKey {
    /// Generate a new conversation key with fresh X25519 keypair.
    pub fn new() -> Self {
        // Generate random X25519 keypair
        let mut secret_bytes = [0u8; 32];
        rand::fill(&mut secret_bytes);
        let secret = X25519Secret::from(secret_bytes);
        let public = X25519PublicKey::from(&secret);

        Self {
            our_ephemeral_public: *public.as_bytes(),
            our_ephemeral_private: secret_bytes,
            their_ephemeral_public: None,
            shared_secret: None,
            version: 1,
            created_at: Utc::now(),
            established_at: None,
        }
    }

    /// Create a new conversation key with a specific version.
    pub fn new_with_version(version: u32) -> Self {
        let mut key = Self::new();
        key.version = version;
        key
    }

    /// Derive shared secret from their public key.
    ///
    /// Uses X25519 ECDH to compute the shared secret.
    pub fn derive_shared_secret(
        &mut self,
        their_public: [u8; 32],
    ) -> Result<(), MailboxError> {
        // Store their public key
        self.their_ephemeral_public = Some(their_public);

        // Reconstruct our secret
        let our_secret = X25519Secret::from(self.our_ephemeral_private);

        // Reconstruct their public
        let their_public_key = X25519PublicKey::from(their_public);

        // Compute shared secret via ECDH
        let shared = our_secret.diffie_hellman(&their_public_key);

        self.shared_secret = Some(*shared.as_bytes());
        self.established_at = Some(Utc::now());

        Ok(())
    }

    /// Check if this key has an established shared secret.
    pub fn is_established(&self) -> bool {
        self.shared_secret.is_some()
    }

    /// Get the shared secret if established.
    pub fn get_shared_secret(&self) -> Option<&[u8; 32]> {
        self.shared_secret.as_ref()
    }
}

impl Default for ConversationKey {
    fn default() -> Self {
        Self::new()
    }
}

/// Manager for DM conversation state persistence.
///
/// Handles database operations for storing and retrieving DM mailbox state.
pub struct DmConversationManager {
    /// Database connection for state storage.
    db: Arc<Database>,
}

impl DmConversationManager {
    /// Create a new DmConversationManager.
    ///
    /// # Arguments
    ///
    /// * `db` - Shared database connection
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

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

        // Load peer mailbox
        let peer_mailbox = self.load_peer_mailbox_internal(&conn, &peer_id_str)?;

        // Load conversation key
        let conversation_key = self.load_conversation_key_internal(&conn, &peer_id_str)?;

        // If no state exists, return None
        if our_mailbox.is_none() && peer_mailbox.is_none() && conversation_key.is_none() {
            return Ok(None);
        }

        Ok(Some(DmConversation {
            peer_id: *peer_id,
            our_mailbox,
            peer_mailbox,
            conversation_key,
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
                "SELECT id, mailbox_public_key, mailbox_private_key, signature, randomizer,
                        token_id, created_at, expires_at, last_polled_at
                 FROM dm_mailboxes WHERE conversation_id = ?1",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![peer_id_str], |row| {
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
                    .map(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .map(|dt| dt.with_timezone(&Utc))
                            .ok()
                    })
                    .flatten();

                Ok(Some(DmMailbox {
                    id,
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

    /// Load peer mailbox from database.
    fn load_peer_mailbox_internal(
        &self,
        conn: &rusqlite::Connection,
        peer_id_str: &str,
    ) -> Result<Option<PeerMailbox>, MailboxError> {
        let mut stmt = conn
            .prepare(
                "SELECT peer_mailbox_public_key, received_at, expires_at
                 FROM dm_peer_mailboxes WHERE conversation_id = ?1
                 ORDER BY received_at DESC LIMIT 1",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![peer_id_str], |row| {
            let public_key: Vec<u8> = row.get(0)?;
            let received_at: String = row.get(1)?;
            let expires_at: Option<String> = row.get(2)?;
            Ok((public_key, received_at, expires_at))
        });

        match result {
            Ok((public_key, received_at, expires_at)) => {
                let public_key_arr: [u8; 32] = public_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid peer public key length".into()))?;

                let received_at = DateTime::parse_from_rfc3339(&received_at)
                    .map_err(|e| MailboxError::Storage(format!("Invalid received_at: {}", e)))?
                    .with_timezone(&Utc);
                let expires_at = expires_at
                    .map(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .map(|dt| dt.with_timezone(&Utc))
                            .ok()
                    })
                    .flatten();

                Ok(Some(PeerMailbox {
                    public_key: public_key_arr,
                    received_at,
                    expires_at,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MailboxError::Storage(e.to_string())),
        }
    }

    /// Load conversation key from database.
    fn load_conversation_key_internal(
        &self,
        conn: &rusqlite::Connection,
        peer_id_str: &str,
    ) -> Result<Option<ConversationKey>, MailboxError> {
        let mut stmt = conn
            .prepare(
                "SELECT our_ephemeral_public, our_ephemeral_private, their_ephemeral_public,
                        shared_secret, key_version, created_at, established_at
                 FROM dm_conversation_keys WHERE conversation_id = ?1
                 ORDER BY key_version DESC LIMIT 1",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![peer_id_str], |row| {
            let our_public: Vec<u8> = row.get(0)?;
            let our_private: Vec<u8> = row.get(1)?;
            let their_public: Option<Vec<u8>> = row.get(2)?;
            let shared_secret: Option<Vec<u8>> = row.get(3)?;
            let version: i64 = row.get(4)?;
            let created_at: String = row.get(5)?;
            let established_at: Option<String> = row.get(6)?;
            Ok((
                our_public,
                our_private,
                their_public,
                shared_secret,
                version,
                created_at,
                established_at,
            ))
        });

        match result {
            Ok((
                our_public,
                our_private,
                their_public,
                shared_secret,
                version,
                created_at,
                established_at,
            )) => {
                let our_public_arr: [u8; 32] = our_public
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid our public key length".into()))?;
                let our_private_arr: [u8; 32] = our_private
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid our private key length".into()))?;

                let their_public_arr: Option<[u8; 32]> = their_public
                    .map(|v| {
                        v.try_into()
                            .map_err(|_| MailboxError::Storage("Invalid their public key".into()))
                    })
                    .transpose()?;
                let shared_secret_arr: Option<[u8; 32]> = shared_secret
                    .map(|v| {
                        v.try_into()
                            .map_err(|_| MailboxError::Storage("Invalid shared secret".into()))
                    })
                    .transpose()?;

                let created_at = DateTime::parse_from_rfc3339(&created_at)
                    .map_err(|e| MailboxError::Storage(format!("Invalid created_at: {}", e)))?
                    .with_timezone(&Utc);
                let established_at = established_at
                    .map(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .map(|dt| dt.with_timezone(&Utc))
                            .ok()
                    })
                    .flatten();

                Ok(Some(ConversationKey {
                    our_ephemeral_public: our_public_arr,
                    our_ephemeral_private: our_private_arr,
                    their_ephemeral_public: their_public_arr,
                    shared_secret: shared_secret_arr,
                    version: version as u32,
                    created_at,
                    established_at,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MailboxError::Storage(e.to_string())),
        }
    }

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
              randomizer, token_id, created_at, expires_at, last_polled_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                mailbox.id,
                peer_id_str,
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

    /// Save/update peer's mailbox address.
    pub fn save_peer_mailbox(
        &self,
        peer_id: &UserId,
        mailbox: &PeerMailbox,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let peer_id_str = peer_id.to_string();
        let id = Uuid::new_v4().to_string();

        conn.execute(
            "INSERT OR REPLACE INTO dm_peer_mailboxes
             (id, conversation_id, peer_mailbox_public_key, received_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                id,
                peer_id_str,
                mailbox.public_key.as_slice(),
                mailbox.received_at.to_rfc3339(),
                mailbox.expires_at.map(|dt| dt.to_rfc3339())
            ],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Save/update conversation keys.
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
        let id = Uuid::new_v4().to_string();

        conn.execute(
            "INSERT OR REPLACE INTO dm_conversation_keys
             (id, conversation_id, our_ephemeral_public, our_ephemeral_private,
              their_ephemeral_public, shared_secret, key_version, created_at, established_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                id,
                peer_id_str,
                key.our_ephemeral_public.as_slice(),
                key.our_ephemeral_private.as_slice(),
                key.their_ephemeral_public.map(|k| k.to_vec()),
                key.shared_secret.map(|s| s.to_vec()),
                key.version as i64,
                key.created_at.to_rfc3339(),
                key.established_at.map(|dt| dt.to_rfc3339())
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

            let peer_id: UserId = conversation_id
                .parse()
                .map_err(|e: uuid::Error| MailboxError::Storage(format!("Invalid peer_id: {}", e)))?;

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

        let mut stmt = conn
            .prepare(
                "SELECT conversation_id FROM dm_mailboxes
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
            let conversation_id: String = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            let peer_id: UserId = conversation_id
                .parse()
                .map_err(|e: uuid::Error| MailboxError::Storage(format!("Invalid peer_id: {}", e)))?;

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

    /// Delete all state for a conversation.
    ///
    /// Useful when user deletes a conversation.
    pub fn delete_conversation(&self, peer_id: &UserId) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let peer_id_str = peer_id.to_string();

        // Delete from all three tables
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dm_conversation_new() {
        let peer_id = UserId::new();
        let conv = DmConversation::new(peer_id);

        assert_eq!(conv.peer_id, peer_id);
        assert!(conv.our_mailbox.is_none());
        assert!(conv.peer_mailbox.is_none());
        assert!(conv.conversation_key.is_none());
    }

    #[test]
    fn test_dm_conversation_needs_bootstrap() {
        let peer_id = UserId::new();
        let conv = DmConversation::new(peer_id);

        assert!(conv.needs_bootstrap());
        assert!(!conv.is_ready_to_send());
        assert!(!conv.is_ready_to_receive());
    }

    #[test]
    fn test_peer_mailbox_new() {
        let public_key = [0u8; 32];
        let expires_at = Some(Utc::now() + Duration::days(7));

        let mailbox = PeerMailbox::new(public_key, expires_at);

        assert_eq!(mailbox.public_key, public_key);
        assert!(mailbox.expires_at.is_some());
        assert!(!mailbox.is_possibly_expired());
    }

    #[test]
    fn test_peer_mailbox_expired() {
        let public_key = [0u8; 32];
        let expires_at = Some(Utc::now() - Duration::hours(1));

        let mailbox = PeerMailbox::new(public_key, expires_at);

        assert!(mailbox.is_possibly_expired());
    }

    #[test]
    fn test_conversation_key_new() {
        let key = ConversationKey::new();

        assert_eq!(key.version, 1);
        assert!(key.their_ephemeral_public.is_none());
        assert!(key.shared_secret.is_none());
        assert!(!key.is_established());
        assert!(key.established_at.is_none());
    }

    #[test]
    fn test_conversation_key_derive_shared_secret() {
        let mut key1 = ConversationKey::new();
        let mut key2 = ConversationKey::new();

        // Exchange public keys
        let pub1 = key1.our_ephemeral_public;
        let pub2 = key2.our_ephemeral_public;

        // Derive shared secrets
        key1.derive_shared_secret(pub2).unwrap();
        key2.derive_shared_secret(pub1).unwrap();

        // Both should derive the same shared secret
        assert!(key1.is_established());
        assert!(key2.is_established());
        assert_eq!(key1.shared_secret, key2.shared_secret);
        assert!(key1.established_at.is_some());
    }

    #[test]
    fn test_conversation_key_with_version() {
        let key = ConversationKey::new_with_version(5);
        assert_eq!(key.version, 5);
    }

    #[test]
    fn test_dm_mailbox_expires_soon() {
        let mailbox = DmMailbox {
            id: "test".to_string(),
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
    fn test_dm_mailbox_is_expired() {
        let mailbox = DmMailbox {
            id: "test".to_string(),
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
    fn test_dm_conversation_ready_states() {
        let peer_id = UserId::new();
        let mut conv = DmConversation::new(peer_id);

        // Add our mailbox
        conv.our_mailbox = Some(DmMailbox {
            id: "test".to_string(),
            public_key: [0u8; 32],
            private_key: [0u8; 64],
            signature: vec![],
            randomizer: None,
            token_id: "token".to_string(),
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::days(7),
            last_polled_at: None,
        });

        assert!(!conv.needs_bootstrap());
        assert!(conv.is_ready_to_receive());
        assert!(!conv.is_ready_to_send()); // Still need peer mailbox and key

        // Add peer mailbox
        conv.peer_mailbox = Some(PeerMailbox::new([0u8; 32], None));

        assert!(!conv.is_ready_to_send()); // Still need conversation key

        // Add conversation key without shared secret
        conv.conversation_key = Some(ConversationKey::new());

        assert!(!conv.is_ready_to_send()); // Need shared secret

        // Establish shared secret
        let mut key = conv.conversation_key.take().unwrap();
        key.derive_shared_secret([1u8; 32]).unwrap();
        conv.conversation_key = Some(key);

        assert!(conv.is_ready_to_send());
    }
}
