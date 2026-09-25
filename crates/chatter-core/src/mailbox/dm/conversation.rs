//! DM conversation state and management.
//!
//! Per-conversation ephemeral mailbox and key state for two-way anonymity.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};
use zeroize::Zeroize;

use crate::crypto::kdf::hkdf_sha256;

use crate::domain::UserId;
use crate::error::MailboxError;
use crate::storage::Database;

/// State of a DM conversation's mailbox system.
///
/// One half per side: ours is per peer — one mailbox and one ephemeral,
/// announced to every peer device — and theirs is per peer DEVICE: each
/// device that answered the bootstrap owns a mailbox and an ephemeral of its
/// own, so each device has its own conversation secret. A device that
/// answers late only adds its own half; it never replaces another's.
pub struct DmConversation {
    /// Peer user ID (conversation_id).
    pub peer_id: UserId,
    /// Our mailbox for receiving messages from every peer device.
    pub our_mailbox: Option<DmMailbox>,
    /// Our half of every device secret: our ephemeral only, never
    /// established itself (see [`Self::device_key`]).
    pub conversation_key: Option<ConversationKey>,
    /// One mailbox per peer device, lowest device id first.
    pub peer_mailboxes: Vec<PeerMailbox>,
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

/// One peer device's half of the conversation: its mailbox and ephemeral.
///
/// Received during DM bootstrap from that device (authenticated: the Signal
/// layer names the device), or moved by its mailbox rotation.
#[derive(Clone)]
pub struct PeerMailbox {
    /// Peer's Ed25519 public key (32 bytes) - their mailbox address.
    pub public_key: [u8; 32],
    /// The peer device that made this mailbox and polls it — the one device
    /// a message deposited here reaches.
    pub device_id: u32,
    /// That device's conversation ephemeral (X25519).
    pub their_ephemeral_public: [u8; 32],
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
            conversation_key: None,
            peer_mailboxes: Vec::new(),
        }
    }

    /// The conversation key with `device`: our ephemeral and its, and the
    /// secret they derive. `None` without our half.
    pub fn device_key(&self, device: &PeerMailbox) -> Option<ConversationKey> {
        let mut key = self.conversation_key.clone()?;
        key.derive_shared_secret(device.their_ephemeral_public).ok()?;
        Some(key)
    }

    /// Every peer device a message can go to now, with its send key.
    pub fn ready_devices(&self) -> Vec<(&PeerMailbox, [u8; 32])> {
        self.peer_mailboxes
            .iter()
            .filter(|m| !m.is_possibly_expired())
            .filter_map(|m| Some((m, self.device_key(m)?.send_key()?)))
            .collect()
    }

    /// The mailbox-layer receive key of every peer device, by device id.
    pub fn recv_keys(&self) -> Vec<(u32, [u8; 32])> {
        self.peer_mailboxes
            .iter()
            .filter_map(|m| Some((m.device_id, self.device_key(m)?.recv_key()?)))
            .collect()
    }

    /// Check if we can send messages to this peer: our mailbox is healthy
    /// (so replies reach us) and at least one peer device is ready.
    pub fn is_ready_to_send(&self) -> bool {
        self.is_ready_to_receive() && !self.ready_devices().is_empty()
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
    /// True if we don't have our own mailbox yet or if it has expired.
    pub fn needs_bootstrap(&self) -> bool {
        self.our_mailbox.as_ref().map_or(true, |m| m.is_expired())
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
    pub fn new(
        public_key: [u8; 32],
        device_id: u32,
        their_ephemeral_public: [u8; 32],
        expires_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            public_key,
            device_id,
            their_ephemeral_public,
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
        {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut secret_bytes).expect("OS CSPRNG must not fail");}
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
    /// Uses X25519 ECDH followed by HKDF-SHA256 to derive the final
    /// encryption key. Signal spec requires HKDF on all DH outputs —
    /// raw ECDH output should never be used directly as a cipher key.
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

        // Compute raw shared secret via ECDH
        let raw_shared = our_secret.diffie_hellman(&their_public_key);

        // RFC 7748 §6.1: reject low-order peer keys that collapse ECDH to a
        // constant. Without this gate a peer (or the server relaying their
        // ephemeral key) forces an all-zero DH and a predictable conversation
        // key — HKDF over a known input derives a known output. Every other
        // X25519 site in the tree has this check; this one did not (CRYPTO-10).
        if !raw_shared.was_contributory() {
            return Err(MailboxError::KeyDerivationFailed(
                "non-contributory ECDH (low-order public key)".into(),
            ));
        }

        // Derive encryption key via HKDF-SHA256 (never use raw DH output directly)
        let derived_key: [u8; 32] = hkdf_sha256(raw_shared.as_bytes(), None, b"chatter-dm-conversation-v1")
            .map_err(|e| MailboxError::KeyDerivationFailed(e.to_string()))?;

        self.shared_secret = Some(derived_key);
        self.established_at = Some(Utc::now());

        Ok(())
    }

    /// Check if this key has an established shared secret.
    pub fn is_established(&self) -> bool {
        self.shared_secret.is_some()
    }

    /// Per-direction send (encryption) key.
    ///
    /// Returns the 32-byte XChaCha20-Poly1305 key the local peer should use
    /// to encrypt outbound DM frames. Derived from the established
    /// `shared_secret` via HKDF-SHA256 with direction-tagged info; both
    /// peers agree on direction by lex-comparing the two ephemeral
    /// public keys.
    ///
    /// Without per-direction separation, both peers encrypted with the same
    /// 32-byte key under random 24-byte XChaCha nonces. A nonce collision
    /// between the two peers (likelier with a memory-disclosed userspace
    /// PRG; see round-1 / round-5 RNG audits) produces keystream reuse
    /// across the two directions, and XOR of any two such messages reveals
    /// plaintext XOR.
    pub fn send_key(&self) -> Option<[u8; 32]> {
        let root = self.shared_secret?;
        let their = self.their_ephemeral_public?;
        Self::derive_directional_key(&root, &self.our_ephemeral_public, &their, true)
    }

    /// Per-direction receive (decryption) key.
    pub fn recv_key(&self) -> Option<[u8; 32]> {
        let root = self.shared_secret?;
        let their = self.their_ephemeral_public?;
        Self::derive_directional_key(&root, &self.our_ephemeral_public, &their, false)
    }

    fn derive_directional_key(
        root: &[u8; 32],
        our_public: &[u8; 32],
        their_public: &[u8; 32],
        is_send: bool,
    ) -> Option<[u8; 32]> {
        // Choose info string so the smaller-pubkey peer's send key
        // equals the bigger-pubkey peer's recv key, and vice versa.
        let we_are_a = our_public < their_public;
        let info: &[u8] = if (we_are_a && is_send) || (!we_are_a && !is_send) {
            b"chatter-dm-conversation-v1/send-from-A"
        } else {
            b"chatter-dm-conversation-v1/send-from-B"
        };
        let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, root);
        let mut out = [0u8; 32];
        hk.expand(info, &mut out).ok()?;
        Some(out)
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
                "SELECT id, mailbox_public_key, mailbox_private_key, signature, randomizer,
                        token_id, created_at, expires_at, last_polled_at
                 FROM dm_mailboxes WHERE conversation_id = ?1
                 ORDER BY created_at DESC LIMIT 1",
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
                    .and_then(|s| {
                        DateTime::parse_from_rfc3339(&s)
                            .map(|dt| dt.with_timezone(&Utc))
                            .ok()
                    });

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
            let (public_key, device_id, their_ephemeral, received_at, expires_at) = row.map_err(storage)?;
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
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, String>(2)?)),
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
        let mailbox = PeerMailbox::new(mailbox_public_key, peer_device_id, their_ephemeral_public, None);
        let mut key = conversation.conversation_key.ok_or(MailboxError::ConversationNotEstablished)?;
        key.derive_shared_secret(their_ephemeral_public)?;
        self.save_peer_mailbox(peer_id, &mailbox)?;
        self.clear_device_asked(peer_id, peer_device_id)?;
        Ok(mailbox)
    }

    /// The device answered (or is gone): the next ask is a new one.
    fn clear_device_asked(&self, peer_id: &UserId, peer_device_id: u32) -> Result<(), MailboxError> {
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
    pub fn forget_peer_device(&self, peer_id: &UserId, peer_device_id: u32) -> Result<(), MailboxError> {
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

    /// Owe `content` to `peer_device_id`: a message some other device of the
    /// peer already got, held until this one answers the bootstrap.
    pub fn queue_for_device(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        content: &[u8],
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        // ponytail: owed copies older than a mailbox lifetime are dropped, so a
        // device that never answers (logged out, keys still published) costs
        // bounded storage; per-device delivery receipts if that matters.
        let cutoff = (Utc::now()
            - Duration::seconds(super::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS as i64))
        .to_rfc3339();
        conn.execute(
            "DELETE FROM dm_device_pending WHERE queued_at < ?1",
            rusqlite::params![cutoff],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;
        conn.execute(
            "INSERT INTO dm_device_pending (conversation_id, peer_device_id, content, queued_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![peer_id.to_string(), peer_device_id, content, Utc::now().to_rfc3339()],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Record that a bootstrap MESSAGE or REQUEST reached `peer_device_id`.
    pub fn mark_device_asked(&self, peer_id: &UserId, peer_device_id: u32) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        conn.execute(
            "INSERT OR REPLACE INTO dm_device_asked (conversation_id, peer_device_id, asked_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![peer_id.to_string(), peer_device_id, Utc::now().to_rfc3339()],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Whether `peer_device_id` was asked to answer the bootstrap within the
    /// last mailbox lifetime and has not answered since. An ask older than
    /// that is repeated: the mailbox it announced may be gone.
    pub fn was_device_asked(&self, peer_id: &UserId, peer_device_id: u32) -> Result<bool, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let cutoff = (Utc::now()
            - Duration::seconds(super::bootstrap::DEFAULT_MAILBOX_TTL_SECONDS as i64))
        .to_rfc3339();
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM dm_device_asked
                            WHERE conversation_id = ?1 AND peer_device_id = ?2 AND asked_at >= ?3)",
            rusqlite::params![peer_id.to_string(), peer_device_id, cutoff],
            |row| row.get(0),
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))
    }

    /// Take everything owed to `peer_device_id`, oldest first.
    pub fn drain_device_queue(&self, peer_id: &UserId, peer_device_id: u32) -> Result<Vec<Vec<u8>>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;
        let storage = |e: rusqlite::Error| MailboxError::Storage(e.to_string());
        let params = rusqlite::params![peer_id.to_string(), peer_device_id];
        let owed = conn
            .prepare(
                "SELECT id, content FROM dm_device_pending
                 WHERE conversation_id = ?1 AND peer_device_id = ?2 ORDER BY id",
            )
            .map_err(storage)?
            .query_map(params, |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)))
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        // Only what was read: a copy queued meanwhile stays for the next drain.
        if let Some((last, _)) = owed.last() {
            conn.execute(
                "DELETE FROM dm_device_pending WHERE conversation_id = ?1 AND peer_device_id = ?2 AND id <= ?3",
                rusqlite::params![peer_id.to_string(), peer_device_id, last],
            )
            .map_err(storage)?;
        }
        Ok(owed.into_iter().map(|(_, content)| content).collect())
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

// ============================================================================
// Security: Zeroization on Drop
// ============================================================================

impl Drop for DmMailbox {
    fn drop(&mut self) {
        // Zeroize the Ed25519 private key
        self.private_key.zeroize();
    }
}

impl Drop for ConversationKey {
    fn drop(&mut self) {
        // Zeroize the X25519 private key
        self.our_ephemeral_private.zeroize();
        // Zeroize the shared secret if present
        if let Some(ref mut secret) = self.shared_secret {
            secret.zeroize();
        }
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
        assert!(conv.peer_mailboxes.is_empty());
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

        let mailbox = PeerMailbox::new(public_key, 1, [1u8; 32], expires_at);

        assert_eq!(mailbox.public_key, public_key);
        assert!(mailbox.expires_at.is_some());
        assert!(!mailbox.is_possibly_expired());
    }

    #[test]
    fn test_peer_mailbox_expired() {
        let public_key = [0u8; 32];
        let expires_at = Some(Utc::now() - Duration::hours(1));

        let mailbox = PeerMailbox::new(public_key, 1, [1u8; 32], expires_at);

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

        // A peer device's half, but not ours yet: no secret to send under.
        let theirs = ConversationKey::new();
        conv.peer_mailboxes.push(PeerMailbox::new([0u8; 32], 1, theirs.our_ephemeral_public, None));
        assert!(!conv.is_ready_to_send());

        // Our half: that device's secret derives, and it is ready.
        conv.conversation_key = Some(ConversationKey::new());
        assert!(conv.is_ready_to_send());
        let (device, send_key) = conv.ready_devices()[0];
        assert_eq!(device.device_id, 1);
        let mut peer = theirs.clone();
        peer.derive_shared_secret(conv.conversation_key.as_ref().unwrap().our_ephemeral_public).unwrap();
        assert_eq!(Some(send_key), peer.recv_key(), "the device's recv key opens what we send it");
    }

    // ========================================================================
    // L4 rotation: two-mailbox overlap semantics (DB-backed)
    // ========================================================================

    /// The token every test mailbox references (dm_mailboxes.token_id FK →
    /// mailbox_tokens.id → mailbox_server_keys.key_id). Seeded by `setup_db`.
    const TEST_TOKEN_ID: &str = "test-token";

    fn setup_db() -> Arc<Database> {
        // tempfile + SQLCipher (matches call_repo tests): Database::open needs a
        // real path, so a temp file is the closest to in-memory available.
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("dm_conversation_test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();

        // Seed the FK parents so saving a mailbox (which references a token)
        // satisfies dm_mailboxes.token_id → mailbox_tokens.id → server key.
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

    fn make_mailbox(id: &str, created_at: DateTime<Utc>, expires_at: DateTime<Utc>) -> DmMailbox {
        DmMailbox {
            id: id.to_string(),
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
    fn test_two_mailboxes_both_polled() {
        // (a) Two coexisting mailbox rows for one conversation → get_active_mailbox_keys
        // returns BOTH (overlap polling is automatic once two rows coexist).
        let db = setup_db();
        let mgr = DmConversationManager::new(db);
        let peer = UserId::new();

        let now = Utc::now();
        let old = make_mailbox("a", now - Duration::hours(2), now + Duration::hours(22));
        let new = make_mailbox("b", now, now + Duration::hours(24));
        mgr.save_our_mailbox(&peer, &old).unwrap();
        mgr.save_our_mailbox(&peer, &new).unwrap();

        let active = mgr.get_active_mailbox_keys().unwrap();
        let ids: Vec<String> = active.iter().map(|(_, _, _, id)| id.clone()).collect();
        assert_eq!(active.len(), 2, "both non-expired rows must be polled");
        assert!(ids.contains(&"a".to_string()));
        assert!(ids.contains(&"b".to_string()));
    }

    #[test]
    fn test_send_targets_newest_mailbox() {
        // (b) load_conversation().our_mailbox (via load_our_mailbox_internal) returns
        // the NEWEST row (larger created_at) so sends target the rotated mailbox.
        let db = setup_db();
        let mgr = DmConversationManager::new(db);
        let peer = UserId::new();

        let now = Utc::now();
        let old = make_mailbox("a", now - Duration::hours(2), now + Duration::hours(22));
        let new = make_mailbox("b", now, now + Duration::hours(24));
        // Insert old last to prove ordering is by created_at, not insertion order.
        mgr.save_our_mailbox(&peer, &new).unwrap();
        mgr.save_our_mailbox(&peer, &old).unwrap();

        let conv = mgr.load_conversation(&peer).unwrap().unwrap();
        let our = conv.our_mailbox.expect("our mailbox present");
        assert_eq!(our.id, "b", "send mailbox must be the newest by created_at");
    }

    #[test]
    fn test_expired_row_excluded_from_active_polling() {
        // (c) A row past its expires_at is excluded from active polling — the old
        // mailbox retires naturally once its TTL elapses.
        let db = setup_db();
        let mgr = DmConversationManager::new(db);
        let peer = UserId::new();

        let now = Utc::now();
        let expired = make_mailbox("a", now - Duration::hours(48), now - Duration::hours(1));
        let live = make_mailbox("b", now, now + Duration::hours(24));
        mgr.save_our_mailbox(&peer, &expired).unwrap();
        mgr.save_our_mailbox(&peer, &live).unwrap();

        let active = mgr.get_active_mailbox_keys().unwrap();
        let ids: Vec<String> = active.iter().map(|(_, _, _, id)| id.clone()).collect();
        assert_eq!(active.len(), 1, "expired row must be excluded from polling");
        assert_eq!(ids, vec!["b".to_string()]);
    }

    #[test]
    fn test_dm_rotation_candidate_not_reselected_when_fresh() {
        // Re-rotation guard (over-rotation rider): while only an expiring row
        // exists the conversation is a candidate; once a fresh (post-rotation)
        // row coexists the conversation is NO LONGER selected — a lingering old
        // row can't re-trigger rotation every cycle and drain the token budget.
        let db = setup_db();
        let mgr = DmConversationManager::new(db);
        let peer = UserId::new();

        let now = Utc::now();
        // Threshold is 24h; this row expires in 12h → within the window.
        let expiring = make_mailbox("a", now - Duration::hours(36), now + Duration::hours(12));
        mgr.save_our_mailbox(&peer, &expiring).unwrap();

        let candidates = mgr.get_expiring_mailboxes(24).unwrap();
        assert_eq!(candidates, vec![peer], "expiring conversation must be a rotation candidate");

        // Rotation mints a fresh row alongside the old (loss-free overlap).
        let fresh = make_mailbox("b", now, now + Duration::hours(48));
        mgr.save_our_mailbox(&peer, &fresh).unwrap();

        // Old row survives …
        assert_eq!(mgr.get_active_mailbox_keys().unwrap().len(), 2);
        // … but the conversation is no longer re-selected for rotation.
        let candidates = mgr.get_expiring_mailboxes(24).unwrap();
        assert!(
            candidates.is_empty(),
            "conversation with a fresh row must not be re-selected for rotation"
        );
    }

    /// Our half with `peer`, saved, and a peer device's key to answer it with.
    fn seed_our_half(mgr: &DmConversationManager, peer: &UserId) -> ConversationKey {
        let now = Utc::now();
        mgr.save_our_mailbox(peer, &make_mailbox("o", now, now + Duration::hours(40))).unwrap();
        let ours = ConversationKey::new();
        mgr.save_conversation_key(peer, &ours).unwrap();
        ours
    }

    /// F54: each peer device that answers keeps its own half. Device 8
    /// answering hours after device 6 adds a second mailbox and secret; it
    /// does not take device 6's over, and re-answering (F53) replaces only
    /// its own.
    #[test]
    fn a_late_device_adds_its_half_without_taking_anothers() {
        let mgr = DmConversationManager::new(setup_db());
        let peer = UserId::new();
        let ours = seed_our_half(&mgr, &peer);
        let (six, eight) = (ConversationKey::new(), ConversationKey::new());

        mgr.record_peer_device(&peer, 6, [6u8; 32], six.our_ephemeral_public).unwrap();
        mgr.record_peer_device(&peer, 8, [8u8; 32], eight.our_ephemeral_public).unwrap();
        let conv = mgr.load_conversation(&peer).unwrap().unwrap();
        let ready: Vec<_> = conv.ready_devices().iter().map(|(m, k)| (m.device_id, m.public_key, *k)).collect();
        assert_eq!(ready.len(), 2);
        assert_eq!((ready[0].0, ready[0].1), (6, [6u8; 32]), "device 6 keeps its mailbox");
        assert_eq!((ready[1].0, ready[1].1), (8, [8u8; 32]));
        assert_ne!(ready[0].2, ready[1].2, "one secret per device");
        let mut six_side = six.clone();
        six_side.derive_shared_secret(ours.our_ephemeral_public).unwrap();
        assert_eq!(Some(ready[0].2), six_side.recv_key());

        // Device 8 re-bootstraps: only its own row moves.
        let eight_again = ConversationKey::new();
        mgr.record_peer_device(&peer, 8, [0x88u8; 32], eight_again.our_ephemeral_public).unwrap();
        let conv = mgr.load_conversation(&peer).unwrap().unwrap();
        let rows: Vec<_> = conv.peer_mailboxes.iter().map(|m| (m.device_id, m.public_key)).collect();
        assert_eq!(rows, vec![(6, [6u8; 32]), (8, [0x88u8; 32])]);

        // A low-order ephemeral is refused and leaves the row alone.
        assert!(mgr.record_peer_device(&peer, 6, [9u8; 32], [0u8; 32]).is_err());
        assert_eq!(mgr.load_conversation(&peer).unwrap().unwrap().peer_mailboxes[0].public_key, [6u8; 32]);

        // A rotation announce moves only the rotating device's mailbox.
        assert!(mgr.rotate_peer_mailbox(&peer, 6, [0x66u8; 32]).unwrap());
        assert!(!mgr.rotate_peer_mailbox(&peer, 7, [0x77u8; 32]).unwrap());
        let rows: Vec<_> = mgr.load_conversation(&peer).unwrap().unwrap()
            .peer_mailboxes.iter().map(|m| (m.device_id, m.public_key)).collect();
        assert_eq!(rows, vec![(6, [0x66u8; 32]), (8, [0x88u8; 32])]);
    }

    /// A new ephemeral of ours drops every device's half (each was paired
    /// with the old one); re-saving the same one keeps them.
    #[test]
    fn a_new_ephemeral_of_ours_drops_every_device_half() {
        let mgr = DmConversationManager::new(setup_db());
        let peer = UserId::new();
        let ours = seed_our_half(&mgr, &peer);
        mgr.record_peer_device(&peer, 6, [6u8; 32], ConversationKey::new().our_ephemeral_public).unwrap();

        mgr.save_conversation_key(&peer, &ours).unwrap();
        assert_eq!(mgr.load_conversation(&peer).unwrap().unwrap().peer_mailboxes.len(), 1);

        mgr.save_conversation_key(&peer, &ConversationKey::new()).unwrap();
        let conv = mgr.load_conversation(&peer).unwrap().unwrap();
        assert!(conv.peer_mailboxes.is_empty());
        assert!(!conv.is_ready_to_send());
    }

    /// Copies owed to a device drain for that device only, in order; a
    /// forgotten (retired) device loses its half and what it was owed.
    #[test]
    fn owed_copies_are_per_device() {
        let mgr = DmConversationManager::new(setup_db());
        let peer = UserId::new();
        seed_our_half(&mgr, &peer);
        mgr.record_peer_device(&peer, 2, [2u8; 32], ConversationKey::new().our_ephemeral_public).unwrap();

        mgr.queue_for_device(&peer, 8, b"one").unwrap();
        mgr.queue_for_device(&peer, 8, b"two").unwrap();
        mgr.queue_for_device(&peer, 2, b"other").unwrap();

        assert_eq!(mgr.drain_device_queue(&peer, 8).unwrap(), vec![b"one".to_vec(), b"two".to_vec()]);
        assert!(mgr.drain_device_queue(&peer, 8).unwrap().is_empty());

        // An ask is remembered until the device answers.
        assert!(!mgr.was_device_asked(&peer, 8).unwrap());
        mgr.mark_device_asked(&peer, 8).unwrap();
        assert!(mgr.was_device_asked(&peer, 8).unwrap());
        mgr.record_peer_device(&peer, 8, [8u8; 32], ConversationKey::new().our_ephemeral_public).unwrap();
        assert!(!mgr.was_device_asked(&peer, 8).unwrap(), "answering clears the ask");

        mgr.forget_peer_device(&peer, 2).unwrap();
        assert!(mgr.drain_device_queue(&peer, 2).unwrap().is_empty());
        let left: Vec<u32> = mgr.load_conversation(&peer).unwrap().unwrap()
            .peer_mailboxes.iter().map(|m| m.device_id).collect();
        assert_eq!(left, vec![8], "only the retired device is gone");
    }

    // CRYPTO-10: every other X25519 site in the tree rejects a non-contributory
    // ECDH; this one did not. A peer offering a low-order public key forced an
    // all-zero DH output, and HKDF over a known input is a known conversation
    // key — the attacker decrypts and forges the whole conversation.
    #[test]
    fn derive_shared_secret_rejects_low_order_peer_keys() {
        // RFC 7748 §6.1 / the standard small-order point set: each of these
        // drives the DH output to a constant.
        const LOW_ORDER: [[u8; 32]; 7] = [
            // 0 (order 1)
            [0u8; 32],
            // 1 (order 4)
            [
                1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0,
            ],
            [
                0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f,
                0xc4, 0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16,
                0x5f, 0x49, 0xb8, 0x00,
            ],
            [
                0x5f, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c, 0x83,
                0xef, 0x5b, 0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22, 0x4e, 0xdd,
                0xd0, 0x9f, 0x11, 0x57,
            ],
            // p-1, p, p+1
            [
                0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0x7f,
            ],
            [
                0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0x7f,
            ],
            [
                0xee, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0x7f,
            ],
        ];

        for (i, peer) in LOW_ORDER.iter().enumerate() {
            let mut key = ConversationKey::new();
            assert!(
                key.derive_shared_secret(*peer).is_err(),
                "low-order peer key #{i} was accepted"
            );
            assert!(!key.is_established(), "a rejected DH still established a key");
        }

        // An honest peer key still works.
        let peer = ConversationKey::new();
        let mut ours = ConversationKey::new();
        ours.derive_shared_secret(peer.our_ephemeral_public)
            .expect("contributory ECDH must succeed");
        assert!(ours.is_established());
    }
}
