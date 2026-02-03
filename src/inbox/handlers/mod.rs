//! Type-specific message handlers.
//!
//! Handlers for different inbox message types. Each handler processes a specific
//! content type from decrypted inbox messages and emits domain events.
//!
//! # Architecture
//!
//! - `InboxHandler` trait: Common interface for all handlers
//! - `HandlerContext`: Shared context passed to handlers (user ID, event channel)
//! - `DomainEvent`: Events emitted by handlers for UI/storage consumption
//! - `HandlerOutcome`: Result of handler processing (processed, ignored, deferred)
//!
//! # Flow
//!
//! 1. `MessageProcessor` decrypts and determines `ContentType`
//! 2. Router selects appropriate handler based on content type
//! 3. Handler parses plaintext, validates, emits `DomainEvent`
//! 4. Events flow to UI/storage via broadcast channel

pub mod attachment;
pub mod chat;
pub mod key_exchange;
pub mod pin;
pub mod reaction;
pub mod receipt;
pub mod sender_key;
pub mod sync;
pub mod typing;

use std::fmt;

use tokio::sync::broadcast;

use crate::domain::{
    Attachment, ChannelId, Message, MessageContent, MessageId, UserId,
};
use crate::inbox::processor::{ContentType, ProcessedMessage};

use std::sync::Arc;

// Re-export handlers
pub use attachment::AttachmentHandler;
pub use chat::ChatHandler;
pub use key_exchange::KeyExchangeHandler;
pub use pin::PinHandler;
pub use reaction::ReactionHandler;
pub use receipt::ReceiptHandler;
pub use sender_key::SenderKeyHandler;
pub use typing::TypingHandler;

// ============================================================================
// InboxHandler Trait
// ============================================================================

/// Handler for processing specific inbox message types.
///
/// Handlers are stateless - all context is passed via `HandlerContext`.
/// Each handler processes one or more `ContentType` variants.
#[async_trait::async_trait]
pub trait InboxHandler: Send + Sync {
    /// Handle a processed message, returning domain events.
    ///
    /// # Arguments
    ///
    /// * `message` - The decrypted message with content type and plaintext
    /// * `ctx` - Handler context with user ID and event channel
    ///
    /// # Returns
    ///
    /// `HandlerOutcome` indicating success, ignore, or deferral
    async fn handle(&self, message: &ProcessedMessage, ctx: &HandlerContext) -> HandlerResult;

    /// Content types this handler processes.
    ///
    /// Used by router to select appropriate handler for a message.
    fn content_types(&self) -> &[ContentType];
}

// ============================================================================
// Handler Context
// ============================================================================

/// Shared context for handler execution.
///
/// Passed to each `handle()` call, providing access to local user info
/// and event emission. Stateless design avoids mutable aliasing.
#[derive(Clone)]
pub struct HandlerContext {
    /// Current user's ID (for sender detection, ownership checks).
    pub local_user_id: UserId,
    /// Broadcast channel for domain events (UI/storage subscribers).
    pub event_tx: broadcast::Sender<DomainEvent>,
}

impl HandlerContext {
    /// Create a new handler context.
    pub fn new(local_user_id: UserId, event_tx: broadcast::Sender<DomainEvent>) -> Self {
        Self {
            local_user_id,
            event_tx,
        }
    }

    /// Emit a domain event to all subscribers.
    ///
    /// Returns number of active receivers, or 0 if no subscribers.
    pub fn emit(&self, event: DomainEvent) -> usize {
        self.event_tx.send(event).unwrap_or(0)
    }
}

// ============================================================================
// Handler Result Types
// ============================================================================

/// Result type for handler operations.
pub type HandlerResult = Result<HandlerOutcome, HandlerError>;

/// Outcome of handler processing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOutcome {
    /// Successfully processed, message should be acknowledged to server.
    Processed,
    /// Not relevant to this handler (wrong content type), skip.
    Ignored,
    /// Needs retry later (missing dependency, rate limited).
    Deferred {
        /// Reason for deferral.
        reason: String,
    },
}

impl HandlerOutcome {
    /// Create a deferred outcome with reason.
    pub fn deferred(reason: impl Into<String>) -> Self {
        Self::Deferred {
            reason: reason.into(),
        }
    }

    /// Check if the outcome indicates success (processed).
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Processed)
    }

    /// Check if the outcome indicates the message was ignored.
    pub fn is_ignored(&self) -> bool {
        matches!(self, Self::Ignored)
    }

    /// Check if the outcome indicates deferral is needed.
    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred { .. })
    }
}

/// Errors that can occur during handler processing.
#[derive(Debug, Clone)]
pub enum HandlerError {
    /// Protobuf decode failed.
    ParseFailed {
        /// Error message from prost.
        error: String,
    },
    /// Content failed validation (e.g., missing required field).
    ValidationFailed {
        /// Reason for validation failure.
        reason: String,
    },
    /// Required context data missing (e.g., no sender for non-sealed-sender).
    MissingContext {
        /// Field that was expected but not present.
        field: String,
    },
}

impl HandlerError {
    /// Create a parse failed error.
    pub fn parse_failed(error: impl Into<String>) -> Self {
        Self::ParseFailed {
            error: error.into(),
        }
    }

    /// Create a validation failed error.
    pub fn validation_failed(reason: impl Into<String>) -> Self {
        Self::ValidationFailed {
            reason: reason.into(),
        }
    }

    /// Create a missing context error.
    pub fn missing_context(field: impl Into<String>) -> Self {
        Self::MissingContext {
            field: field.into(),
        }
    }

    /// Check if this error is recoverable (should be retried).
    pub fn is_recoverable(&self) -> bool {
        // Parse and validation errors are permanent, missing context might be transient
        matches!(self, Self::MissingContext { .. })
    }
}

impl fmt::Display for HandlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HandlerError::ParseFailed { error } => write!(f, "parse failed: {}", error),
            HandlerError::ValidationFailed { reason } => write!(f, "validation failed: {}", reason),
            HandlerError::MissingContext { field } => {
                write!(f, "missing required context: {}", field)
            }
        }
    }
}

impl std::error::Error for HandlerError {}

// ============================================================================
// Domain Events
// ============================================================================

/// Domain events emitted by handlers.
///
/// These events flow to UI and storage layers via broadcast channels.
/// Handlers emit events; consumers subscribe and react.
#[derive(Debug, Clone)]
pub enum DomainEvent {
    /// New chat message received.
    MessageReceived {
        /// The complete message with content, sender, channel.
        message: Message,
    },
    /// Existing message was edited.
    MessageEdited {
        /// ID of the edited message.
        message_id: MessageId,
        /// New content after edit.
        new_content: MessageContent,
        /// Channel containing the message.
        channel_id: ChannelId,
    },
    /// Message was deleted (delete for everyone).
    MessageDeleted {
        /// ID of the deleted message (derived from timestamp).
        message_id: MessageId,
        /// Author who deleted the message.
        author_id: UserId,
        /// Channel containing the message.
        channel_id: ChannelId,
    },
    /// Attachment metadata ready (needs download).
    AttachmentReady {
        /// Attachment metadata from pointer.
        attachment: Attachment,
        /// Message this attachment belongs to.
        message_id: MessageId,
        /// Channel context.
        channel_id: ChannelId,
    },
    /// Typing indicator received.
    TypingStarted {
        /// User who started typing.
        user_id: UserId,
        /// Channel where typing.
        channel_id: ChannelId,
    },
    /// Typing indicator stopped.
    TypingStopped {
        /// User who stopped typing.
        user_id: UserId,
        /// Channel where typing stopped.
        channel_id: ChannelId,
    },
    /// Read receipt received.
    MessagesRead {
        /// User who read the messages.
        reader_id: UserId,
        /// Message timestamps that were read (Signal uses timestamps as message IDs).
        message_timestamps: Vec<u64>,
        /// When the receipt was sent (ms since epoch).
        receipt_timestamp: u64,
    },
    /// Delivery receipt received.
    MessagesDelivered {
        /// User whose device received the messages.
        recipient_id: UserId,
        /// Message timestamps that were delivered (Signal uses timestamps as message IDs).
        message_timestamps: Vec<u64>,
        /// When the receipt was sent (ms since epoch).
        receipt_timestamp: u64,
    },
    /// Reaction added to message.
    ReactionAdded {
        /// Target message (identified by timestamp in Signal protocol).
        message_id: MessageId,
        /// Message author's ID (for lookup with target_author_aci).
        author_id: UserId,
        /// User who added the reaction.
        reactor_id: UserId,
        /// Emoji used.
        emoji: String,
        /// When the reaction was sent (ms since epoch).
        timestamp: u64,
    },
    /// Reaction removed from message.
    ReactionRemoved {
        /// Target message (identified by timestamp in Signal protocol).
        message_id: MessageId,
        /// Message author's ID (for lookup with target_author_aci).
        author_id: UserId,
        /// User who removed the reaction.
        reactor_id: UserId,
        /// Emoji that was removed.
        emoji: String,
        /// When the reaction removal was sent (ms since epoch).
        timestamp: u64,
    },
    /// View-once message was viewed (for ephemeral/disappearing messages).
    ViewedReceipt {
        /// The viewed message (identified by timestamp).
        message_timestamp: u64,
        /// User who viewed the message.
        from_user_id: UserId,
        /// When the view occurred (ms since epoch).
        receipt_timestamp: u64,
    },
    /// Message was pinned.
    MessagePinned {
        /// Message that was pinned.
        message_id: MessageId,
        /// User who pinned the message.
        pinner_id: UserId,
        /// Channel context.
        channel_id: ChannelId,
    },
    /// Message was unpinned.
    MessageUnpinned {
        /// Message that was unpinned.
        message_id: MessageId,
        /// User who unpinned the message.
        unpinner_id: UserId,
        /// Channel context.
        channel_id: ChannelId,
    },
    /// Key exchange received (mailbox DM encryption setup).
    KeyExchangeReceived {
        /// User who sent the key exchange.
        sender_id: UserId,
        /// Conversation/channel this key applies to.
        conversation_id: ChannelId,
        /// Sender's ephemeral public key (32 bytes Ed25519).
        ephemeral_public_key: [u8; 32],
        /// When the key exchange was initiated (ms since epoch).
        timestamp: u64,
    },
    /// Sender key distribution received (group encryption setup).
    SenderKeyReceived {
        /// User who distributed the sender key.
        sender_id: UserId,
        /// Channel this key is for.
        channel_id: ChannelId,
        /// Serialized sender key distribution message (libsignal format).
        distribution_message: Vec<u8>,
        /// When the distribution was sent (ms since epoch).
        timestamp: u64,
    },
}

impl DomainEvent {
    /// Get the channel ID associated with this event, if any.
    pub fn channel_id(&self) -> Option<ChannelId> {
        match self {
            DomainEvent::MessageReceived { message } => Some(message.channel_id),
            DomainEvent::MessageEdited { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessageDeleted { channel_id, .. } => Some(*channel_id),
            DomainEvent::AttachmentReady { channel_id, .. } => Some(*channel_id),
            DomainEvent::TypingStarted { channel_id, .. } => Some(*channel_id),
            DomainEvent::TypingStopped { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessagesRead { .. } => None, // Cross-channel
            DomainEvent::MessagesDelivered { .. } => None, // Cross-channel
            DomainEvent::ViewedReceipt { .. } => None, // Cross-channel
            DomainEvent::ReactionAdded { .. } => None, // Reactions now use message lookup, not channel
            DomainEvent::ReactionRemoved { .. } => None, // Reactions now use message lookup, not channel
            DomainEvent::MessagePinned { channel_id, .. } => Some(*channel_id),
            DomainEvent::MessageUnpinned { channel_id, .. } => Some(*channel_id),
            DomainEvent::KeyExchangeReceived { conversation_id, .. } => Some(*conversation_id),
            DomainEvent::SenderKeyReceived { channel_id, .. } => Some(*channel_id),
        }
    }

    /// Check if this is a message-related event.
    pub fn is_message_event(&self) -> bool {
        matches!(
            self,
            DomainEvent::MessageReceived { .. }
                | DomainEvent::MessageEdited { .. }
                | DomainEvent::MessageDeleted { .. }
        )
    }

    /// Check if this is a transient event (not persisted).
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            DomainEvent::TypingStarted { .. } | DomainEvent::TypingStopped { .. }
        )
    }
}

// ============================================================================
// Handler Registry
// ============================================================================

/// Registry for routing messages to appropriate handlers.
///
/// The registry maintains a collection of handlers and routes incoming
/// `ProcessedMessage` objects to the handler that supports their content type.
///
/// # Example
///
/// ```ignore
/// let (event_tx, event_rx) = broadcast::channel(64);
/// let registry = HandlerRegistry::new(local_user_id, event_tx);
///
/// // Route an incoming message
/// let result = registry.route(&processed_message).await;
///
/// // Subscribe to domain events
/// let mut rx = registry.subscribe();
/// while let Ok(event) = rx.recv().await {
///     match event {
///         DomainEvent::MessageReceived { message } => { ... }
///         DomainEvent::TypingStarted { user_id, channel_id } => { ... }
///         _ => {}
///     }
/// }
/// ```
///
/// # Adding Custom Handlers
///
/// Use `register()` to add custom handlers:
///
/// ```ignore
/// registry.register(Arc::new(MyCustomHandler::new()));
/// ```
pub struct HandlerRegistry {
    handlers: Vec<Arc<dyn InboxHandler>>,
    context: HandlerContext,
}

impl HandlerRegistry {
    /// Create a new handler registry with default handlers.
    ///
    /// # Arguments
    ///
    /// * `local_user_id` - The current user's ID (for sender detection, ownership checks)
    /// * `event_tx` - Broadcast channel for domain events
    ///
    /// # Default Handlers
    ///
    /// - `ChatHandler` - DataMessage content (text, media, edits, deletes)
    /// - `AttachmentHandler` - Attachment metadata from pointers
    /// - `ReactionHandler` - Emoji reactions on messages
    /// - `ReceiptHandler` - Delivery/read/viewed receipts
    /// - `TypingHandler` - Typing indicators
    pub fn new(local_user_id: UserId, event_tx: broadcast::Sender<DomainEvent>) -> Self {
        let context = HandlerContext::new(local_user_id, event_tx);

        let handlers: Vec<Arc<dyn InboxHandler>> = vec![
            Arc::new(ChatHandler::new()),
            Arc::new(AttachmentHandler::new()),
            Arc::new(ReactionHandler::new()),
            Arc::new(ReceiptHandler::new()),
            Arc::new(TypingHandler::new()),
        ];

        Self { handlers, context }
    }

    /// Route a processed message to the appropriate handler.
    ///
    /// Finds the first handler that supports the message's content type
    /// and delegates processing to it.
    ///
    /// # Returns
    ///
    /// - `Ok(HandlerOutcome::Processed)` - Message was successfully handled
    /// - `Ok(HandlerOutcome::Ignored)` - No handler for this content type
    /// - `Ok(HandlerOutcome::Deferred { reason })` - Handler needs retry
    /// - `Err(HandlerError)` - Handler encountered an error
    pub async fn route(&self, message: &ProcessedMessage) -> HandlerResult {
        // Find handler that supports this content type
        for handler in &self.handlers {
            if handler.content_types().contains(&message.content_type) {
                return handler.handle(message, &self.context).await;
            }
        }

        // No handler found for content type
        tracing::debug!(
            content_type = %message.content_type,
            "No handler registered for content type"
        );
        Ok(HandlerOutcome::Ignored)
    }

    /// Register an additional handler.
    ///
    /// Custom handlers are appended to the handler list. The first handler
    /// that supports a content type wins, so built-in handlers take precedence.
    ///
    /// # Arguments
    ///
    /// * `handler` - The handler to register
    pub fn register(&mut self, handler: Arc<dyn InboxHandler>) {
        self.handlers.push(handler);
    }

    /// Subscribe to domain events.
    ///
    /// Returns a receiver that will receive all domain events emitted by handlers.
    /// Multiple subscribers can exist simultaneously.
    pub fn subscribe(&self) -> broadcast::Receiver<DomainEvent> {
        self.context.event_tx.subscribe()
    }

    /// Get the number of registered handlers.
    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handler_outcome_constructors() {
        assert!(HandlerOutcome::Processed.is_success());
        assert!(HandlerOutcome::Ignored.is_ignored());

        let deferred = HandlerOutcome::deferred("rate limited");
        assert!(deferred.is_deferred());
        if let HandlerOutcome::Deferred { reason } = deferred {
            assert_eq!(reason, "rate limited");
        }
    }

    #[test]
    fn test_handler_error_display() {
        let parse_err = HandlerError::parse_failed("invalid protobuf");
        assert_eq!(parse_err.to_string(), "parse failed: invalid protobuf");

        let validation_err = HandlerError::validation_failed("missing body");
        assert_eq!(
            validation_err.to_string(),
            "validation failed: missing body"
        );

        let context_err = HandlerError::missing_context("sender_address");
        assert_eq!(
            context_err.to_string(),
            "missing required context: sender_address"
        );
    }

    #[test]
    fn test_handler_error_is_recoverable() {
        assert!(!HandlerError::parse_failed("test").is_recoverable());
        assert!(!HandlerError::validation_failed("test").is_recoverable());
        assert!(HandlerError::missing_context("test").is_recoverable());
    }

    #[test]
    fn test_domain_event_is_transient() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        let typing = DomainEvent::TypingStarted {
            user_id,
            channel_id,
        };
        assert!(typing.is_transient());

        let msg = DomainEvent::MessageReceived {
            message: Message::new_text(channel_id, Some(user_id), "test".to_string()),
        };
        assert!(!msg.is_transient());
    }

    #[test]
    fn test_domain_event_channel_id() {
        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        let msg = DomainEvent::MessageReceived {
            message: Message::new_text(channel_id, Some(user_id), "test".to_string()),
        };
        assert_eq!(msg.channel_id(), Some(channel_id));

        let read = DomainEvent::MessagesRead {
            reader_id: user_id,
            message_timestamps: vec![123],
            receipt_timestamp: 456,
        };
        assert_eq!(read.channel_id(), None); // Cross-channel
    }

    #[test]
    fn test_handler_context_emit() {
        let (tx, mut rx) = broadcast::channel(16);
        let ctx = HandlerContext::new(UserId::new(), tx);

        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        ctx.emit(DomainEvent::TypingStarted {
            user_id,
            channel_id,
        });

        let event = rx.try_recv().unwrap();
        if let DomainEvent::TypingStarted {
            user_id: u,
            channel_id: c,
        } = event
        {
            assert_eq!(u, user_id);
            assert_eq!(c, channel_id);
        } else {
            panic!("Wrong event type");
        }
    }

    // ========================================================================
    // HandlerRegistry Tests
    // ========================================================================

    use crate::proto::content_v1::{content, typing_message, Content, TypingMessage};
    use crate::proto::mailbox_v1::EnvelopeType;
    use libsignal_protocol::{DeviceId, ProtocolAddress};
    use prost::Message as ProstMessage;
    use uuid::Uuid;

    fn make_registry() -> (HandlerRegistry, broadcast::Receiver<DomainEvent>) {
        let (tx, rx) = broadcast::channel(64);
        let registry = HandlerRegistry::new(UserId::new(), tx);
        (registry, rx)
    }

    fn make_processed(content_type: ContentType, plaintext: Vec<u8>) -> ProcessedMessage {
        ProcessedMessage::new(
            "msg-test".to_string(),
            Some(ProtocolAddress::new(
                Uuid::new_v4().to_string(),
                DeviceId::try_from(1u32).unwrap(),
            )),
            content_type,
            plaintext,
            1234567890000,
            EnvelopeType::SealedSender,
        )
    }

    #[test]
    fn test_registry_has_default_handlers() {
        let (registry, _rx) = make_registry();
        // 5 default handlers: Chat, Attachment, Reaction, Receipt, Typing
        assert_eq!(registry.handler_count(), 5);
    }

    #[test]
    fn test_registry_register_custom_handler() {
        let (mut registry, _rx) = make_registry();
        assert_eq!(registry.handler_count(), 5);

        // Add custom handler
        registry.register(Arc::new(TypingHandler::new()));
        assert_eq!(registry.handler_count(), 6);
    }

    #[test]
    fn test_registry_subscribe() {
        let (registry, _rx) = make_registry();
        // Can create multiple subscribers
        let _rx1 = registry.subscribe();
        let _rx2 = registry.subscribe();
    }

    #[tokio::test]
    async fn test_registry_routes_typing_message() {
        let (registry, mut rx) = make_registry();

        // Create typing message content
        let typing = TypingMessage {
            action: typing_message::Action::Started as i32,
            timestamp: 1704067200000,
            channel_id: Uuid::new_v4().to_string(),
        };

        let content = Content {
            content: Some(content::Content::Typing(typing)),
            ephemeral_reply_to: None,
            timestamp: 1704067200000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed(ContentType::TypingMessage, plaintext);

        let result = registry.route(&processed).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, DomainEvent::TypingStarted { .. }));
    }

    #[tokio::test]
    async fn test_registry_routes_receipt_message() {
        use crate::proto::content_v1::{receipt_message, ReceiptMessage};

        let (registry, mut rx) = make_registry();

        let receipt = ReceiptMessage {
            r#type: receipt_message::Type::Read as i32,
            timestamps: vec![1704067200000],
            timestamp: 1704067201000,
        };

        let content = Content {
            content: Some(content::Content::Receipt(receipt)),
            ephemeral_reply_to: None,
            timestamp: 1704067201000,
            padding: vec![],
        };

        let mut plaintext = Vec::new();
        content.encode(&mut plaintext).unwrap();
        let processed = make_processed(ContentType::ReceiptMessage, plaintext);

        let result = registry.route(&processed).await.unwrap();
        assert!(result.is_success());

        // Verify event was emitted
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, DomainEvent::MessagesRead { .. }));
    }

    #[tokio::test]
    async fn test_registry_returns_ignored_for_unknown() {
        let (registry, _rx) = make_registry();

        // Unknown content type
        let processed = make_processed(ContentType::Unknown(99), vec![]);

        let result = registry.route(&processed).await.unwrap();
        assert!(result.is_ignored());
    }

    #[tokio::test]
    async fn test_registry_routes_null_message() {
        let (registry, _rx) = make_registry();

        // NullMessage has no handler currently
        let processed = make_processed(ContentType::NullMessage, vec![]);

        let result = registry.route(&processed).await.unwrap();
        assert!(result.is_ignored());
    }
}
