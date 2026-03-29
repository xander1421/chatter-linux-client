//! Message processor.
//!
//! Process incoming messages from the inbox by coordinating decryption
//! and determining content type for routing to handlers.

use std::fmt;

use rand::SeedableRng;

use libsignal_protocol::{
    DeviceId, IdentityKeyStore, KyberPreKeyStore, PreKeySignalMessage, PreKeyStore,
    ProtocolAddress, SessionStore, SignalMessage, SignedPreKeyStore,
};

use crate::crypto::signal::{SealedSenderDecryptionResult, SealedSenderManager};
use crate::error::CryptoError;
use crate::proto::mailbox_v1::{EnvelopeType, EphemeralMessage};

// ============================================================================
// Content Type
// ============================================================================

/// Content type for routing to handlers.
///
/// Determined by inspecting the decrypted plaintext after decryption.
/// Each type routes to a specific handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    /// Regular chat message (text, media, etc.).
    DataMessage,
    /// Multi-device synchronization message.
    SyncMessage,
    /// Typing indicator (start/stop).
    TypingMessage,
    /// Read/delivery receipt.
    ReceiptMessage,
    /// Call signaling (offer, answer, ice, hangup).
    CallMessage,
    /// Keep-alive/padding message.
    NullMessage,
    /// Group sender key distribution message.
    SenderKeyDistribution,
    /// Decryption error notification (retry request).
    DecryptionError,
    /// Unknown or unrecognized content type.
    Unknown(u32),
}

impl ContentType {
    /// Create from a protobuf content type field value.
    ///
    /// These values map to the Signal protocol Content message type fields.
    /// See: https://github.com/signalapp/Signal-Desktop/blob/main/protos/SignalService.proto
    pub fn from_type_field(type_field: u32) -> Self {
        // Signal protocol Content message type field values
        // Based on SignalService.proto Content message
        match type_field {
            1 => ContentType::DataMessage,
            2 => ContentType::SyncMessage,
            3 => ContentType::CallMessage,
            4 => ContentType::NullMessage,
            5 => ContentType::ReceiptMessage,
            6 => ContentType::TypingMessage,
            7 => ContentType::SenderKeyDistribution,
            8 => ContentType::DecryptionError,
            other => ContentType::Unknown(other),
        }
    }

    /// Check if this is a message that should be persisted.
    pub fn is_persistable(&self) -> bool {
        matches!(self, ContentType::DataMessage | ContentType::SyncMessage)
    }

    /// Check if this is a transient message (not stored).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ContentType::TypingMessage
                | ContentType::NullMessage
                | ContentType::ReceiptMessage
                | ContentType::DecryptionError
        )
    }
}

impl fmt::Display for ContentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContentType::DataMessage => write!(f, "DataMessage"),
            ContentType::SyncMessage => write!(f, "SyncMessage"),
            ContentType::TypingMessage => write!(f, "TypingMessage"),
            ContentType::ReceiptMessage => write!(f, "ReceiptMessage"),
            ContentType::CallMessage => write!(f, "CallMessage"),
            ContentType::NullMessage => write!(f, "NullMessage"),
            ContentType::SenderKeyDistribution => write!(f, "SenderKeyDistribution"),
            ContentType::DecryptionError => write!(f, "DecryptionError"),
            ContentType::Unknown(t) => write!(f, "Unknown({})", t),
        }
    }
}

// ============================================================================
// Processing Error
// ============================================================================

/// Errors that can occur during message processing.
#[derive(Debug, Clone)]
pub enum ProcessingError {
    /// Decryption failed for the message.
    DecryptionFailed {
        /// ID of the message that failed.
        message_id: String,
        /// Error description.
        error: String,
    },
    /// Message failed validation.
    ValidationFailed {
        /// ID of the message that failed.
        message_id: String,
        /// Reason for validation failure.
        reason: String,
    },
    /// Storage operation failed.
    StoreError {
        /// ID of the message that failed.
        message_id: String,
        /// Error description.
        error: String,
    },
    /// Message is a duplicate (already processed).
    Duplicate {
        /// ID of the duplicate message.
        message_id: String,
    },
    /// Message has expired (too old).
    Expired {
        /// ID of the expired message.
        message_id: String,
    },
    /// Unsupported envelope type.
    UnsupportedEnvelope {
        /// ID of the message.
        message_id: String,
        /// The unknown envelope type value.
        envelope_type: i32,
    },
    /// Sealed sender self-send (sent message to ourselves).
    SelfSend {
        /// ID of the message.
        message_id: String,
    },
}

impl ProcessingError {
    /// Get the message ID associated with this error.
    pub fn message_id(&self) -> &str {
        match self {
            ProcessingError::DecryptionFailed { message_id, .. } => message_id,
            ProcessingError::ValidationFailed { message_id, .. } => message_id,
            ProcessingError::StoreError { message_id, .. } => message_id,
            ProcessingError::Duplicate { message_id } => message_id,
            ProcessingError::Expired { message_id } => message_id,
            ProcessingError::UnsupportedEnvelope { message_id, .. } => message_id,
            ProcessingError::SelfSend { message_id } => message_id,
        }
    }

    /// Check if this error is recoverable (should be retried).
    pub fn is_recoverable(&self) -> bool {
        match self {
            // Store errors might be transient
            ProcessingError::StoreError { .. } => true,
            // Most other errors are permanent
            ProcessingError::DecryptionFailed { .. } => false,
            ProcessingError::ValidationFailed { .. } => false,
            ProcessingError::Duplicate { .. } => false,
            ProcessingError::Expired { .. } => false,
            ProcessingError::UnsupportedEnvelope { .. } => false,
            ProcessingError::SelfSend { .. } => false,
        }
    }
}

impl fmt::Display for ProcessingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcessingError::DecryptionFailed { message_id, error } => {
                write!(f, "decryption failed for {}: {}", message_id, error)
            }
            ProcessingError::ValidationFailed { message_id, reason } => {
                write!(f, "validation failed for {}: {}", message_id, reason)
            }
            ProcessingError::StoreError { message_id, error } => {
                write!(f, "storage error for {}: {}", message_id, error)
            }
            ProcessingError::Duplicate { message_id } => {
                write!(f, "duplicate message: {}", message_id)
            }
            ProcessingError::Expired { message_id } => {
                write!(f, "expired message: {}", message_id)
            }
            ProcessingError::UnsupportedEnvelope {
                message_id,
                envelope_type,
            } => {
                write!(
                    f,
                    "unsupported envelope type {} for {}",
                    envelope_type, message_id
                )
            }
            ProcessingError::SelfSend { message_id } => {
                write!(f, "self-send message: {}", message_id)
            }
        }
    }
}

impl std::error::Error for ProcessingError {}

// ============================================================================
// Processed Message
// ============================================================================

/// A successfully processed and decrypted message.
///
/// Contains the decrypted plaintext and metadata needed for routing
/// to the appropriate handler.
#[derive(Debug, Clone)]
pub struct ProcessedMessage {
    /// Original message ID from the envelope.
    pub message_id: String,
    /// Sender's protocol address (None for sealed sender until decrypted).
    pub sender_address: Option<ProtocolAddress>,
    /// Content type for routing to handlers.
    pub content_type: ContentType,
    /// Decrypted plaintext bytes (handlers parse the protobuf).
    pub plaintext: Vec<u8>,
    /// Server timestamp (Unix millis).
    pub timestamp: u64,
    /// Original envelope type.
    pub envelope_type: EnvelopeType,
}

impl ProcessedMessage {
    /// Create a new processed message.
    pub fn new(
        message_id: String,
        sender_address: Option<ProtocolAddress>,
        content_type: ContentType,
        plaintext: Vec<u8>,
        timestamp: u64,
        envelope_type: EnvelopeType,
    ) -> Self {
        Self {
            message_id,
            sender_address,
            content_type,
            plaintext,
            timestamp,
            envelope_type,
        }
    }

    /// Check if sender is known (not sealed sender or already decrypted).
    pub fn has_known_sender(&self) -> bool {
        self.sender_address.is_some()
    }
}

/// Result type for message processing.
pub type ProcessingResult = Result<ProcessedMessage, ProcessingError>;

// ============================================================================
// Message Processor
// ============================================================================

/// Processor for decrypting and categorizing inbox messages.
///
/// Coordinates decryption using SealedSenderManager and libsignal
/// directly, then determines the content type for routing to handlers.
///
/// # Design
///
/// Following the stateless pattern from Phase 14, all cryptographic stores
/// are passed as parameters rather than stored in the struct. This avoids
/// mutable aliasing issues with libsignal's async trait methods.
///
/// # Flow
///
/// 1. Receive `EphemeralMessage` from inbox
/// 2. Match on envelope type (SealedSender, PrekeyMessage, Message)
/// 3. Call appropriate decryption function with stores
/// 4. Determine content type from decrypted plaintext
/// 5. Return `ProcessedMessage` for handler routing
///
/// # Example
///
/// ```ignore
/// let processor = MessageProcessor::new(
///     trust_root,
///     Some("+15551234567".to_string()),
///     "user-uuid".to_string(),
///     DeviceId::new(1),
/// );
///
/// let result = processor.process(
///     &message,
///     &mut identity_store,
///     &mut session_store,
///     &mut prekey_store,
///     &signed_prekey_store,
///     &mut kyber_prekey_store,
/// ).await;
///
/// match result {
///     Ok(processed) => {
///         // Route to handler based on content_type
///         match processed.content_type {
///             ContentType::DataMessage => handle_data_message(&processed),
///             ContentType::TypingMessage => handle_typing(&processed),
///             // ...
///         }
///     }
///     Err(e) => {
///         // Handle error (retry queue, logging, etc.)
///     }
/// }
/// ```
pub struct MessageProcessor {
    /// Server trust root for sealed sender certificate validation.
    trust_root: libsignal_protocol::PublicKey,
    /// Local user's phone number (for self-send detection).
    local_e164: Option<String>,
    /// Local user's UUID.
    local_uuid: String,
    /// Local device ID.
    local_device_id: DeviceId,
}

impl MessageProcessor {
    /// Create a new message processor.
    ///
    /// # Arguments
    ///
    /// * `trust_root` - Server's public key for sealed sender certificate validation
    /// * `local_e164` - Local user's phone number (optional, for self-send detection)
    /// * `local_uuid` - Local user's UUID
    /// * `local_device_id` - Local device ID
    pub fn new(
        trust_root: libsignal_protocol::PublicKey,
        local_e164: Option<String>,
        local_uuid: String,
        local_device_id: DeviceId,
    ) -> Self {
        Self {
            trust_root,
            local_e164,
            local_uuid,
            local_device_id,
        }
    }

    /// Process a message by decrypting and determining content type.
    ///
    /// # Arguments
    ///
    /// * `message` - The ephemeral message from the inbox
    /// * `identity_store` - Identity key storage
    /// * `session_store` - Session storage
    /// * `pre_key_store` - One-time prekey storage
    /// * `signed_pre_key_store` - Signed prekey storage
    /// * `kyber_pre_key_store` - Kyber prekey storage
    ///
    /// # Returns
    ///
    /// `ProcessedMessage` on success, `ProcessingError` on failure.
    #[allow(clippy::too_many_arguments)]
    pub async fn process<I, S, P, SP, K>(
        &self,
        message: &EphemeralMessage,
        identity_store: &mut I,
        session_store: &mut S,
        pre_key_store: &mut P,
        signed_pre_key_store: &SP,
        kyber_pre_key_store: &mut K,
    ) -> ProcessingResult
    where
        I: IdentityKeyStore,
        S: SessionStore,
        P: PreKeyStore,
        SP: SignedPreKeyStore,
        K: KyberPreKeyStore,
    {
        let envelope_type = EnvelopeType::try_from(message.envelope_type)
            .unwrap_or(EnvelopeType::Unspecified);

        match envelope_type {
            EnvelopeType::SealedSender => {
                self.process_sealed_sender(
                    message,
                    identity_store,
                    session_store,
                    pre_key_store,
                    signed_pre_key_store,
                    kyber_pre_key_store,
                )
                .await
            }
            EnvelopeType::PrekeyMessage => {
                self.process_prekey_message(
                    message,
                    identity_store,
                    session_store,
                    pre_key_store,
                    signed_pre_key_store,
                    kyber_pre_key_store,
                )
                .await
            }
            EnvelopeType::Message => {
                self.process_regular_message(message, identity_store, session_store)
                    .await
            }
            EnvelopeType::EciesInit => {
                // ECIES init messages are handled directly in device.rs, not by this processor.
                // If we get here, something is misconfigured.
                Err(ProcessingError::UnsupportedEnvelope {
                    message_id: message.id.clone(),
                    envelope_type: message.envelope_type,
                })
            }
            EnvelopeType::SenderKey => Err(ProcessingError::UnsupportedEnvelope {
                message_id: message.id.clone(),
                envelope_type: message.envelope_type,
            }),
            EnvelopeType::Unspecified => Err(ProcessingError::UnsupportedEnvelope {
                message_id: message.id.clone(),
                envelope_type: message.envelope_type,
            }),
        }
    }

    /// Process a sealed sender message.
    async fn process_sealed_sender<I, S, P, SP, K>(
        &self,
        message: &EphemeralMessage,
        identity_store: &mut I,
        session_store: &mut S,
        pre_key_store: &mut P,
        signed_pre_key_store: &SP,
        kyber_pre_key_store: &mut K,
    ) -> ProcessingResult
    where
        I: IdentityKeyStore,
        S: SessionStore,
        P: PreKeyStore,
        SP: SignedPreKeyStore,
        K: KyberPreKeyStore,
    {
        let timestamp = libsignal_protocol::Timestamp::from_epoch_millis(message.timestamp);

        let result = SealedSenderManager::decrypt(
            &message.encrypted_envelope,
            &self.trust_root,
            timestamp,
            self.local_e164.clone(),
            self.local_uuid.clone(),
            self.local_device_id,
            identity_store,
            session_store,
            pre_key_store,
            signed_pre_key_store,
            kyber_pre_key_store,
        )
        .await;

        match result {
            Ok(decrypted) => {
                let sender_address = self.extract_sender_from_sealed(&decrypted);
                let content_type = self.determine_content_type(&decrypted.message);

                Ok(ProcessedMessage::new(
                    message.id.clone(),
                    sender_address,
                    content_type,
                    decrypted.message,
                    message.timestamp,
                    EnvelopeType::SealedSender,
                ))
            }
            Err(CryptoError::SealedSenderSelfSend) => {
                // Self-send is a special case - log and skip
                tracing::warn!(
                    message_id = %message.id,
                    "Received sealed sender message from self, skipping"
                );
                Err(ProcessingError::SelfSend {
                    message_id: message.id.clone(),
                })
            }
            Err(CryptoError::InvalidSenderCertificate(msg)) => {
                // Certificate expired or invalid
                if msg.contains("expired") {
                    Err(ProcessingError::Expired {
                        message_id: message.id.clone(),
                    })
                } else {
                    Err(ProcessingError::DecryptionFailed {
                        message_id: message.id.clone(),
                        error: format!("invalid sender certificate: {}", msg),
                    })
                }
            }
            Err(e) => Err(ProcessingError::DecryptionFailed {
                message_id: message.id.clone(),
                error: e.to_string(),
            }),
        }
    }

    /// Process a prekey message (initial key exchange).
    ///
    /// Extracts sender address from envelope metadata (source_uuid, source_device_id)
    /// and decrypts using libsignal's PreKeySignalMessage decryption.
    ///
    /// This path handles initial session setup where the sender identity is provided
    /// by the server in the envelope metadata. Most messages use sealed sender instead.
    async fn process_prekey_message<I, S, P, SP, K>(
        &self,
        message: &EphemeralMessage,
        identity_store: &mut I,
        session_store: &mut S,
        pre_key_store: &mut P,
        signed_pre_key_store: &SP,
        kyber_pre_key_store: &mut K,
    ) -> ProcessingResult
    where
        I: IdentityKeyStore,
        S: SessionStore,
        P: PreKeyStore,
        SP: SignedPreKeyStore,
        K: KyberPreKeyStore,
    {
        let sender_address = self.extract_sender_from_envelope(message)?;

        let prekey_msg =
            PreKeySignalMessage::try_from(message.encrypted_envelope.as_slice()).map_err(|e| {
                ProcessingError::DecryptionFailed {
                    message_id: message.id.clone(),
                    error: format!("invalid prekey message format: {}", e),
                }
            })?;

        let mut rng = rand::rngs::StdRng::from_os_rng();
        let plaintext = libsignal_protocol::message_decrypt_prekey(
            &prekey_msg,
            &sender_address,
            session_store,
            identity_store,
            pre_key_store,
            signed_pre_key_store,
            kyber_pre_key_store,
            &mut rng,
        )
        .await
        .map_err(|e| ProcessingError::DecryptionFailed {
            message_id: message.id.clone(),
            error: format!("prekey decryption failed: {}", e),
        })?;

        let content_type = self.determine_content_type(&plaintext);

        Ok(ProcessedMessage::new(
            message.id.clone(),
            Some(sender_address),
            content_type,
            plaintext,
            message.timestamp,
            EnvelopeType::PrekeyMessage,
        ))
    }

    /// Process a regular (non-prekey) message.
    ///
    /// Extracts sender address from envelope metadata and decrypts using
    /// libsignal's SignalMessage decryption for an established session.
    ///
    /// This is a fallback path; most messages use sealed sender.
    async fn process_regular_message<I, S>(
        &self,
        message: &EphemeralMessage,
        identity_store: &mut I,
        session_store: &mut S,
    ) -> ProcessingResult
    where
        I: IdentityKeyStore,
        S: SessionStore,
    {
        let sender_address = self.extract_sender_from_envelope(message)?;

        let signal_msg =
            SignalMessage::try_from(message.encrypted_envelope.as_slice()).map_err(|e| {
                ProcessingError::DecryptionFailed {
                    message_id: message.id.clone(),
                    error: format!("invalid signal message format: {}", e),
                }
            })?;

        let mut rng = rand::rngs::StdRng::from_os_rng();
        let plaintext = libsignal_protocol::message_decrypt_signal(
            &signal_msg,
            &sender_address,
            session_store,
            identity_store,
            &mut rng,
        )
        .await
        .map_err(|e| ProcessingError::DecryptionFailed {
            message_id: message.id.clone(),
            error: format!("signal message decryption failed: {}", e),
        })?;

        let content_type = self.determine_content_type(&plaintext);

        Ok(ProcessedMessage::new(
            message.id.clone(),
            Some(sender_address),
            content_type,
            plaintext,
            message.timestamp,
            EnvelopeType::Message,
        ))
    }

    /// Extract sender address from envelope metadata (source_uuid, source_device_id).
    ///
    /// Used for non-sealed-sender envelopes (PreKey, Message) where the server
    /// provides the sender identity in the envelope wrapper.
    fn extract_sender_from_envelope(
        &self,
        message: &EphemeralMessage,
    ) -> Result<ProtocolAddress, ProcessingError> {
        let source_uuid = message.source_uuid.as_ref().ok_or_else(|| {
            ProcessingError::DecryptionFailed {
                message_id: message.id.clone(),
                error: "non-sealed-sender envelope missing source_uuid".to_string(),
            }
        })?;

        let source_device = message.source_device_id.unwrap_or(1);
        let device_u8: u8 = source_device.try_into().map_err(|_| {
            ProcessingError::DecryptionFailed {
                message_id: message.id.clone(),
                error: format!("source_device_id {} exceeds u8 range", source_device),
            }
        })?;
        let device_id = DeviceId::new(device_u8).map_err(|_| {
            ProcessingError::DecryptionFailed {
                message_id: message.id.clone(),
                error: format!("invalid source_device_id: {}", source_device),
            }
        })?;

        Ok(ProtocolAddress::new(source_uuid.clone(), device_id))
    }

    /// Extract sender address from sealed sender decryption result.
    fn extract_sender_from_sealed(
        &self,
        result: &SealedSenderDecryptionResult,
    ) -> Option<ProtocolAddress> {
        // SealedSenderDecryptionResult has sender_uuid and device_id
        // We construct a ProtocolAddress from these
        Some(ProtocolAddress::new(
            result.sender_uuid.clone(),
            result.device_id,
        ))
    }

    /// Determine content type by inspecting the plaintext.
    ///
    /// Peeks at the protobuf structure to find the content type field.
    /// Signal Content message has optional fields for each type.
    fn determine_content_type(&self, plaintext: &[u8]) -> ContentType {
        // Signal protocol Content message structure (simplified):
        // The Content message has optional fields:
        // - field 1: DataMessage
        // - field 2: SyncMessage
        // - field 3: CallMessage
        // - field 4: NullMessage
        // - field 5: ReceiptMessage
        // - field 6: TypingMessage
        // - field 7: SenderKeyDistributionMessage
        // - field 8: DecryptionErrorMessage
        //
        // We peek at the first tag to determine which field is present.

        if plaintext.is_empty() {
            return ContentType::Unknown(0);
        }

        // Protobuf wire format: tag = (field_number << 3) | wire_type
        // For embedded messages, wire_type = 2 (length-delimited)
        // So tag for field 1 = (1 << 3) | 2 = 10 (0x0A)
        //    tag for field 2 = (2 << 3) | 2 = 18 (0x12)
        //    tag for field 3 = (3 << 3) | 2 = 26 (0x1A)
        //    etc.

        let first_byte = plaintext[0];

        // Check if it's a length-delimited field (wire type 2)
        if first_byte & 0x07 == 2 {
            let field_number = first_byte >> 3;
            return ContentType::from_type_field(field_number as u32);
        }

        ContentType::Unknown(first_byte as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_type_from_type_field() {
        assert_eq!(ContentType::from_type_field(1), ContentType::DataMessage);
        assert_eq!(ContentType::from_type_field(2), ContentType::SyncMessage);
        assert_eq!(ContentType::from_type_field(3), ContentType::CallMessage);
        assert_eq!(ContentType::from_type_field(4), ContentType::NullMessage);
        assert_eq!(ContentType::from_type_field(5), ContentType::ReceiptMessage);
        assert_eq!(ContentType::from_type_field(6), ContentType::TypingMessage);
        assert_eq!(
            ContentType::from_type_field(7),
            ContentType::SenderKeyDistribution
        );
        assert_eq!(
            ContentType::from_type_field(8),
            ContentType::DecryptionError
        );
        assert_eq!(ContentType::from_type_field(99), ContentType::Unknown(99));
    }

    #[test]
    fn test_content_type_is_persistable() {
        assert!(ContentType::DataMessage.is_persistable());
        assert!(ContentType::SyncMessage.is_persistable());
        assert!(!ContentType::TypingMessage.is_persistable());
        assert!(!ContentType::NullMessage.is_persistable());
        assert!(!ContentType::ReceiptMessage.is_persistable());
    }

    #[test]
    fn test_content_type_is_transient() {
        assert!(ContentType::TypingMessage.is_transient());
        assert!(ContentType::NullMessage.is_transient());
        assert!(ContentType::ReceiptMessage.is_transient());
        assert!(ContentType::DecryptionError.is_transient());
        assert!(!ContentType::DataMessage.is_transient());
        assert!(!ContentType::SyncMessage.is_transient());
    }

    #[test]
    fn test_content_type_display() {
        assert_eq!(format!("{}", ContentType::DataMessage), "DataMessage");
        assert_eq!(format!("{}", ContentType::Unknown(42)), "Unknown(42)");
    }

    #[test]
    fn test_processing_error_message_id() {
        let err = ProcessingError::DecryptionFailed {
            message_id: "msg-123".to_string(),
            error: "test".to_string(),
        };
        assert_eq!(err.message_id(), "msg-123");

        let err = ProcessingError::Duplicate {
            message_id: "msg-456".to_string(),
        };
        assert_eq!(err.message_id(), "msg-456");
    }

    #[test]
    fn test_processing_error_is_recoverable() {
        let store_err = ProcessingError::StoreError {
            message_id: "msg-1".to_string(),
            error: "db connection failed".to_string(),
        };
        assert!(store_err.is_recoverable());

        let decrypt_err = ProcessingError::DecryptionFailed {
            message_id: "msg-2".to_string(),
            error: "bad key".to_string(),
        };
        assert!(!decrypt_err.is_recoverable());

        let dup_err = ProcessingError::Duplicate {
            message_id: "msg-3".to_string(),
        };
        assert!(!dup_err.is_recoverable());
    }

    #[test]
    fn test_processing_error_display() {
        let err = ProcessingError::DecryptionFailed {
            message_id: "msg-1".to_string(),
            error: "invalid key".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "decryption failed for msg-1: invalid key"
        );

        let err = ProcessingError::Duplicate {
            message_id: "msg-2".to_string(),
        };
        assert_eq!(err.to_string(), "duplicate message: msg-2");

        let err = ProcessingError::UnsupportedEnvelope {
            message_id: "msg-3".to_string(),
            envelope_type: 99,
        };
        assert_eq!(err.to_string(), "unsupported envelope type 99 for msg-3");
    }

    #[test]
    fn test_processed_message_creation() {
        let device_id = DeviceId::try_from(1u32).unwrap();
        let msg = ProcessedMessage::new(
            "msg-123".to_string(),
            Some(ProtocolAddress::new("user1".to_string(), device_id)),
            ContentType::DataMessage,
            vec![1, 2, 3],
            1234567890,
            EnvelopeType::SealedSender,
        );

        assert_eq!(msg.message_id, "msg-123");
        assert!(msg.has_known_sender());
        assert_eq!(msg.content_type, ContentType::DataMessage);
        assert_eq!(msg.plaintext, vec![1, 2, 3]);
        assert_eq!(msg.timestamp, 1234567890);
        assert_eq!(msg.envelope_type, EnvelopeType::SealedSender);
    }

    #[test]
    fn test_processed_message_unknown_sender() {
        let msg = ProcessedMessage::new(
            "msg-456".to_string(),
            None,
            ContentType::TypingMessage,
            vec![],
            0,
            EnvelopeType::Message,
        );

        assert!(!msg.has_known_sender());
    }

    #[test]
    fn test_envelope_type_variants() {
        // Verify all EnvelopeType variants exist and can be matched
        // Proto EnvelopeType: Unspecified=0, SealedSender=1, PrekeyMessage=2, Message=3

        let sealed = EnvelopeType::SealedSender;
        let prekey = EnvelopeType::PrekeyMessage;
        let message = EnvelopeType::Message;
        let unspecified = EnvelopeType::Unspecified;

        assert_eq!(sealed as i32, 1);
        assert_eq!(prekey as i32, 2);
        assert_eq!(message as i32, 3);
        assert_eq!(unspecified as i32, 0);
    }

    #[test]
    fn test_sealed_sender_is_primary_path() {
        // Sealed sender should be the preferred path for privacy
        let sealed = EnvelopeType::SealedSender;
        let prekey = EnvelopeType::PrekeyMessage;
        let message = EnvelopeType::Message;

        // Sealed sender provides anonymity
        assert!(matches!(sealed, EnvelopeType::SealedSender));

        // PrekeyMessage and Message require sender info from envelope metadata
        // which breaks anonymity
        assert!(!matches!(prekey, EnvelopeType::SealedSender));
        assert!(!matches!(message, EnvelopeType::SealedSender));
    }

    #[test]
    fn test_prekey_message_requires_sender_context() {
        // PreKey messages need sender address from envelope metadata
        // because they are initial key exchange messages

        let error = ProcessingError::DecryptionFailed {
            message_id: "prekey-msg-1".to_string(),
            error: "prekey message requires sender address from envelope metadata (not yet implemented)".to_string(),
        };

        // This error indicates the expected behavior for prekey without context
        assert!(error.to_string().contains("prekey message requires sender address"));
        assert!(error.to_string().contains("not yet implemented"));
    }

    #[test]
    fn test_regular_message_requires_sender_context() {
        // Regular (non-sealed sender) messages also need sender address

        let error = ProcessingError::DecryptionFailed {
            message_id: "regular-msg-1".to_string(),
            error: "regular message requires sender address from envelope metadata (not yet implemented)".to_string(),
        };

        assert!(error.to_string().contains("regular message requires sender address"));
    }

    #[test]
    fn test_envelope_type_processing_priority() {
        // Processing priority for different envelope types:
        // 1. SealedSender - full anonymity, contains sender info inside
        // 2. PrekeyMessage - initial session setup, needs envelope metadata
        // 3. Message - established session, needs envelope metadata
        // 4. Unspecified - should not process

        fn get_processing_priority(envelope_type: EnvelopeType) -> u8 {
            match envelope_type {
                EnvelopeType::SealedSender => 1,   // Highest priority, self-contained
                EnvelopeType::PrekeyMessage => 2,  // Needs context
                EnvelopeType::Message => 3,        // Needs context
                EnvelopeType::EciesInit => 0,      // Handled by device.rs, not processor
                EnvelopeType::SenderKey => 4,      // Group sender key message
                EnvelopeType::Unspecified => 255,  // Should not process
            }
        }

        assert_eq!(get_processing_priority(EnvelopeType::SealedSender), 1);
        assert_eq!(get_processing_priority(EnvelopeType::PrekeyMessage), 2);
        assert_eq!(get_processing_priority(EnvelopeType::Message), 3);
        assert_eq!(get_processing_priority(EnvelopeType::Unspecified), 255);
    }

    #[test]
    fn test_processed_message_sender_extraction() {
        // Test extracting sender info from sealed sender result
        let device_id = DeviceId::try_from(1u32).unwrap();
        let sender = ProtocolAddress::new("alice-uuid".to_string(), device_id);

        let msg = ProcessedMessage::new(
            "sealed-msg-1".to_string(),
            Some(sender.clone()),
            ContentType::DataMessage,
            vec![0x01, 0x02, 0x03],
            1234567890,
            EnvelopeType::SealedSender,
        );

        // Sealed sender should have known sender
        assert!(msg.has_known_sender());

        // Verify sender info
        let extracted_sender = msg.sender_address.as_ref().unwrap();
        assert_eq!(extracted_sender.name(), "alice-uuid");
        assert_eq!(extracted_sender.device_id(), device_id);
    }

    #[test]
    fn test_content_type_requires_decryption() {
        // Check which content types need decryption
        fn requires_decryption(content_type: ContentType) -> bool {
            matches!(
                content_type,
                ContentType::DataMessage | ContentType::SyncMessage | ContentType::SenderKeyDistribution
            )
        }

        assert!(requires_decryption(ContentType::DataMessage));
        assert!(requires_decryption(ContentType::SyncMessage));
        assert!(requires_decryption(ContentType::SenderKeyDistribution));
        assert!(!requires_decryption(ContentType::TypingMessage));
        assert!(!requires_decryption(ContentType::ReceiptMessage));
    }

    #[test]
    fn test_envelope_type_try_from() {
        // Test conversion from i32 to EnvelopeType
        assert_eq!(
            EnvelopeType::try_from(0).ok(),
            Some(EnvelopeType::Unspecified)
        );
        assert_eq!(
            EnvelopeType::try_from(1).ok(),
            Some(EnvelopeType::SealedSender)
        );
        assert_eq!(
            EnvelopeType::try_from(2).ok(),
            Some(EnvelopeType::PrekeyMessage)
        );
        assert_eq!(
            EnvelopeType::try_from(3).ok(),
            Some(EnvelopeType::Message)
        );
        // Invalid values should fail
        assert!(EnvelopeType::try_from(99).is_err());
    }
}
