use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};
use zeroize::Zeroize;

use crate::crypto::kdf::hkdf_sha256;
use crate::domain::UserId;
use crate::error::MailboxError;

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
    /// Ed25519 private key (64 bytes) - for signing challenges. Signing
    /// only: envelopes are sealed to `encryption_private_key`'s public half.
    pub private_key: [u8; 64],
    /// X25519 encryption private key (32 bytes), generated alongside the
    /// signing key. Opens the HPKE envelopes sealed to this mailbox
    /// (`crypto::envelope_crypto`); never sent to the server.
    pub encryption_private_key: [u8; 32],
    /// Unblinded RSA signature over the public key.
    pub signature: Vec<u8>,
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
        key.derive_shared_secret(device.their_ephemeral_public)
            .ok()?;
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
        self.our_mailbox.as_ref().is_some_and(|m| !m.is_expired())
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

    /// The X25519 encryption public key peers seal envelopes to.
    pub fn encryption_public_key(&self) -> Result<[u8; 32], MailboxError> {
        crate::crypto::envelope_crypto::mailbox_encryption_public_key(&self.encryption_private_key)
            .map_err(|e| MailboxError::InvalidServerKey(format!("Invalid encryption key: {e}")))
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
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(&mut secret_bytes)
                .expect("OS CSPRNG must not fail");
        }
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
    pub fn derive_shared_secret(&mut self, their_public: [u8; 32]) -> Result<(), MailboxError> {
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
        let derived_key: [u8; 32] =
            hkdf_sha256(raw_shared.as_bytes(), None, b"chatter-dm-conversation-v1")
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

// ============================================================================
// Security: Zeroization on Drop
// ============================================================================

impl Drop for DmMailbox {
    fn drop(&mut self) {
        // Zeroize the Ed25519 signing and X25519 encryption private keys
        self.private_key.zeroize();
        self.encryption_private_key.zeroize();
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
