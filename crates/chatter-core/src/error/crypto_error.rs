//! Crypto-specific errors.
//!
//! Errors related to cryptographic operations including Signal Protocol.

use thiserror::Error;

use super::StorageError;

/// Who said this peer is the peer, for an identity change we are about to show
/// a human.
///
/// The three sites that can surface a changed identity key do not attribute the
/// peer equally well, and the difference is the whole security value of the
/// prompt: a user asked to accept a new key for "Alice" is entitled to know
/// whether "Alice" was proven or merely asserted by the server. Collapsing them
/// would let a malicious server present its own key change as though it had been
/// cryptographically attributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityChangeSource {
    /// Sealed sender. The peer id comes from a sender certificate this client
    /// validated against the pinned trust root, so it is authenticated.
    SenderCertificate,
    /// Identified sender. The peer id is envelope metadata the SERVER supplied
    /// and nothing authenticates it.
    ServerClaimed,
    /// A prekey bundle fetched to open an outgoing session. Server-supplied, and
    /// only as trustworthy as the key-transparency gate that admitted it.
    PreKeyBundle,
}

impl IdentityChangeSource {
    /// True only when the peer id was proven cryptographically. The UI must not
    /// name the peer as a fact when this is false.
    pub fn is_authenticated(&self) -> bool {
        matches!(self, Self::SenderCertificate)
    }

    /// Stable token for the `pending_identity_changes.source` column.
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Self::SenderCertificate => "sender_certificate",
            Self::ServerClaimed => "server_claimed",
            Self::PreKeyBundle => "prekey_bundle",
        }
    }

    /// Inverse of [`Self::as_db_str`]. An unrecognised token degrades to the
    /// LEAST trusted variant — a row we cannot interpret must never be shown to
    /// the user as an authenticated attribution.
    pub fn from_db_str(s: &str) -> Self {
        match s {
            "sender_certificate" => Self::SenderCertificate,
            "prekey_bundle" => Self::PreKeyBundle,
            _ => Self::ServerClaimed,
        }
    }
}

impl std::fmt::Display for IdentityChangeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SenderCertificate => "sender certificate (authenticated)",
            Self::ServerClaimed => "server-claimed envelope (unauthenticated)",
            Self::PreKeyBundle => "prekey bundle (server-supplied)",
        })
    }
}

/// Crypto error types.
#[derive(Debug, Error)]
pub enum CryptoError {
    /// Storage layer error.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// No local identity key has been generated.
    #[error("no local identity")]
    NoIdentity,

    /// Signal protocol error.
    #[error("signal protocol error: {0}")]
    Protocol(String),

    /// Invalid key format or corrupted key data.
    #[error("invalid key format")]
    InvalidKeyFormat,

    /// Invalid key length provided.
    #[error("invalid key length: expected {expected}, got {got}")]
    InvalidKeyLength {
        /// Expected key length in bytes.
        expected: usize,
        /// Actual key length in bytes.
        got: usize,
    },

    /// Key derivation operation failed.
    #[error("key derivation failed: {0}")]
    KeyDerivationFailed(String),

    /// The peer presented a DIFFERENT identity key than the one pinned for their
    /// account — a reinstall or device wipe, or an active MITM. The two are
    /// indistinguishable from here, which is why it is never resolved
    /// automatically: libsignal refuses the operation and the conversation stays
    /// wedged until a human accepts the new key via
    /// [`crate::services::SignalService::accept_peer_identity_change`].
    ///
    /// **Must not be collapsed into [`Self::SessionDesync`].** A desync is answered
    /// with a `DecryptionErrorMessage` asking the peer to reset its session and
    /// resend — which resends the same new identity key and fails identically,
    /// forever. No session-layer action can change which key is pinned.
    #[error(
        "untrusted identity for {peer_id}:{device_id} — the peer's identity key changed \
         (reinstall or MITM), attributed by {attribution}; it must be explicitly accepted"
    )]
    UntrustedIdentity {
        /// Peer account id (identity keys are pinned per account).
        peer_id: String,
        /// Device whose message or bundle surfaced the change.
        device_id: u32,
        /// The new key as presented, when the failing operation carried one.
        new_identity_key: Option<Vec<u8>>,
        /// How `peer_id` was attributed. Load-bearing for what the UI may claim.
        /// (Not named `source`: thiserror would read that as an error cause.)
        attribution: IdentityChangeSource,
    },

    /// The server answered the peer's Key Transparency lookup with "no entry".
    ///
    /// Every identity key is logged in the same transaction that stores it
    /// (server `keytransparency.AppendLeafTx`, on registration, device linking
    /// and republish), so a peer with a prekey bundle always has an entry. This
    /// answer is a server choosing to skip KT for that peer — the TOFU downgrade
    /// a MITM needs — and the session is refused. Retrying cannot help until the
    /// server proves the key.
    #[error("{peer_id} has no Key Transparency entry; refusing to trust its identity key")]
    NotInTransparencyLog {
        /// Peer account id that was looked up.
        peer_id: String,
    },

    /// Session not found for the given address.
    #[error("session not found for {0}")]
    SessionNotFound(String),

    /// Session exists but is not usable (expired, corrupted, or invalid state).
    #[error("session unusable for {address}: {reason}")]
    SessionUnusable {
        /// The remote address.
        address: String,
        /// Why the session is unusable.
        reason: String,
    },

    /// Session ratchet has reached a state where it cannot be used for sending.
    #[error("session expired for {0}")]
    SessionExpired(String),

    /// The inner message had already been decrypted once — libsignal's
    /// `DuplicatedMessage`, raised when the ratchet chain has advanced past the
    /// counter the message carries and its message keys are gone.
    ///
    /// **Not** a [`Self::SessionDesync`], although it arrives down the same path.
    /// A duplicate is a re-delivery: the session is healthy and the plaintext was
    /// already handled, so the message is dropped. Reporting it as a desync sends
    /// the peer a `DecryptionErrorMessage` asking it to tear down a working
    /// session and resend — which produces more traffic, more re-deliveries, and
    /// more reports. Measured before this was split out: 201 inbound reports and
    /// 12 outbound in four minutes between two idle devices.
    ///
    /// libsignal treats it as routine too, logging `Duplicate message for
    /// counter: N` at info (`session_cipher.rs`).
    #[error("duplicate message from {sender_uuid}:{sender_device_id} (already decrypted)")]
    DuplicateMessage {
        /// Authenticated sender user id.
        sender_uuid: String,
        /// Authenticated sender device id.
        sender_device_id: u32,
    },

    /// A sealed-sender envelope opened cleanly (sender authenticated via the
    /// certificate) but the INNER Whisper/PreKey message failed to decrypt —
    /// the classic desynced-1:1-session wedge. Carries everything needed to
    /// build a Signal `DecryptionErrorMessage` back to the sender so the
    /// session self-heals (see `MessageGateway::process_incoming_message`).
    #[error("session desync with {sender_uuid}:{sender_device_id}: {reason}")]
    SessionDesync {
        /// Authenticated sender user id (from the validated sender certificate).
        sender_uuid: String,
        /// Authenticated sender device id.
        sender_device_id: u32,
        /// Inner `CiphertextMessageType` discriminant (2 = Whisper, 3 = PreKey).
        msg_type: u8,
        /// The failed inner ciphertext (serialized Whisper/PreKey message).
        ciphertext: Vec<u8>,
        /// Underlying libsignal error string.
        reason: String,
        /// The sealed-sender layer's group id (Signal's USMC `groupId`): the
        /// channel a pairwise channel control belongs to, so the failure is
        /// shown there rather than in a DM with the sender.
        group_id: Option<Vec<u8>>,
    },

    /// Encryption operation failed.
    #[error("encryption failed: {0}")]
    EncryptionFailed(String),

    /// Decryption operation failed.
    #[error("decryption failed: {0}")]
    DecryptionFailed(String),

    /// Key not found in store.
    #[error("key not found: {0}")]
    KeyNotFound(String),

    /// Invalid signature.
    #[error("invalid signature")]
    InvalidSignature,

    /// Fingerprint generation failed.
    #[error("fingerprint generation failed: {0}")]
    FingerprintFailed(String),

    /// Invalid or malformed sealed sender message.
    #[error("invalid sealed sender message: {0}")]
    InvalidSealedSenderMessage(String),

    /// Attempted to decrypt own sealed sender message.
    #[error("cannot decrypt sealed sender message from self")]
    SealedSenderSelfSend,

    /// Sender certificate validation failed.
    #[error("invalid sender certificate: {0}")]
    InvalidSenderCertificate(String),

    /// Server certificate ID not recognized.
    #[error("unknown server certificate: {0}")]
    UnknownServerCertificate(u32),

    // =========================================================================
    // Attachment Encryption Errors
    // =========================================================================
    /// Attachment encryption operation failed.
    #[error("attachment encryption failed: {0}")]
    AttachmentEncryptionFailed(String),

    /// Attachment decryption operation failed.
    #[error("attachment decryption failed: {0}")]
    AttachmentDecryptionFailed(String),

    /// Attachment MAC verification failed (tampered or corrupted).
    #[error("attachment MAC mismatch")]
    AttachmentMacMismatch,

    /// Attachment digest verification failed.
    #[error("attachment digest mismatch")]
    AttachmentDigestMismatch,

    /// Attachment exceeds maximum allowed size.
    #[error("attachment too large: {size} bytes exceeds {max} byte limit")]
    AttachmentTooLarge { size: u64, max: u64 },

    // =========================================================================
    // Blind Signature Errors (NDSS '21 Ephemeral Mailboxes)
    // =========================================================================
    /// Blind signature operation failed (blinding/unblinding).
    #[error("blind signature error: {0}")]
    BlindSignatureError(String),

    /// Invalid key provided (RSA public key parsing, Ed25519 format, etc.).
    #[error("invalid key: {0}")]
    InvalidKey(String),

    /// Validation error (e.g., invalid parameters, bounds exceeded).
    #[error("validation error: {0}")]
    Validation(String),
}
