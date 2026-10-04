//! Content validation.
//!
//! Content validation pipeline for incoming messages.

use thiserror::Error;

use super::deduplicator::{DeduplicatorStats, MessageDeduplicator};
use super::orderer::{MessageOrderer, OrdererStats, OrderingResult};
use crate::proto::chat_v1::Message;

/// Validation errors for message content.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ValidationError {
    /// A required field is missing or empty.
    #[error("missing required field: {0}")]
    MissingField(String),

    /// Message ID is invalid (empty or too long).
    #[error("invalid message ID format")]
    InvalidMessageId,

    /// Channel ID is invalid (empty or too long).
    #[error("invalid channel ID format")]
    InvalidChannelId,

    /// Timestamp is outside acceptable range.
    #[error("timestamp out of range: {0}")]
    InvalidTimestamp(u64),

    /// Message content exceeds size limit.
    #[error("content too large: {size} bytes (max {max})")]
    ContentTooLarge {
        /// Actual content size in bytes.
        size: usize,
        /// Maximum allowed size in bytes.
        max: usize,
    },

    /// Message has no content.
    #[error("empty message content")]
    EmptyContent,
}

/// Content limits for validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationLimits {
    /// Maximum message content size in bytes.
    pub max_content_size: usize,
    /// Maximum timestamp drift from current time in seconds.
    pub max_timestamp_drift_secs: u64,
    /// Minimum valid timestamp (Unix epoch in milliseconds).
    pub min_timestamp: u64,
    /// Maximum length for ID fields (message_id, channel_id).
    pub max_id_length: usize,
}

impl Default for ValidationLimits {
    fn default() -> Self {
        Self {
            max_content_size: 64 * 1024,                      // 64KB
            max_timestamp_drift_secs: 5 * 60,                 // 5 minutes
            min_timestamp: 1_577_836_800_000_000_000,         // 2020-01-01 00:00:00 UTC in nanos
            max_id_length: 64,                                // ULIDs/UUIDs are ~26-36 chars
        }
    }
}

/// Content validator for incoming messages.
///
/// Validates message structure and content before processing:
/// - ID format and length
/// - Timestamp bounds
/// - Content size limits
///
/// # Note
///
/// Does NOT validate `author_id` as empty since sealed sender messages
/// have no author (per Signal protocol).
pub struct ContentValidator {
    limits: ValidationLimits,
}

impl ContentValidator {
    /// Create a validator with custom limits.
    pub fn new(limits: ValidationLimits) -> Self {
        Self { limits }
    }

    /// Create a validator with default limits.
    pub fn with_defaults() -> Self {
        Self::new(ValidationLimits::default())
    }

    /// Validate a message.
    ///
    /// Returns `Ok(())` if the message is valid, or an appropriate
    /// `ValidationError` describing what's wrong.
    pub fn validate(&self, message: &Message) -> Result<(), ValidationError> {
        // Validate message ID
        self.validate_id(&message.id, "message_id")?;

        // Validate channel ID
        self.validate_id(&message.channel_id, "channel_id")?;

        // Note: author_id can be empty for sealed sender messages
        // Do NOT validate it as required

        // Validate timestamp
        self.validate_timestamp(message.timestamp)?;

        // Validate content if present
        if let Some(ref content) = message.content {
            self.validate_content_size(content)?;
        }
        // Note: Empty content is allowed - some messages may be system/meta

        Ok(())
    }

    /// Validate an ID field (message_id, channel_id).
    fn validate_id(&self, id: &str, field_name: &str) -> Result<(), ValidationError> {
        if id.is_empty() {
            return Err(ValidationError::MissingField(field_name.to_string()));
        }

        if id.len() > self.limits.max_id_length {
            return match field_name {
                "message_id" => Err(ValidationError::InvalidMessageId),
                "channel_id" => Err(ValidationError::InvalidChannelId),
                _ => Err(ValidationError::MissingField(format!(
                    "{} too long ({} > {})",
                    field_name,
                    id.len(),
                    self.limits.max_id_length
                ))),
            };
        }

        // Basic sanity check - IDs should be ASCII alphanumeric with hyphens
        // Allow some flexibility for different ID formats (ULID, UUID, etc.)
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return match field_name {
                "message_id" => Err(ValidationError::InvalidMessageId),
                "channel_id" => Err(ValidationError::InvalidChannelId),
                _ => Err(ValidationError::MissingField(format!(
                    "{} contains invalid characters",
                    field_name
                ))),
            };
        }

        Ok(())
    }

    /// Validate timestamp is within reasonable bounds.
    fn validate_timestamp(&self, timestamp: u64) -> Result<(), ValidationError> {
        // Check minimum (not before 2020)
        if timestamp < self.limits.min_timestamp {
            return Err(ValidationError::InvalidTimestamp(timestamp));
        }

        // Check maximum (not too far in the future)
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);

        let max_future = now_nanos + (self.limits.max_timestamp_drift_secs * 1_000_000_000);
        if timestamp > max_future {
            return Err(ValidationError::InvalidTimestamp(timestamp));
        }

        Ok(())
    }

    /// Validate content size.
    fn validate_content_size(
        &self,
        content: &crate::proto::chat_v1::message::Content,
    ) -> Result<(), ValidationError> {
        // Estimate content size based on the type
        let size = Self::estimate_content_size(content);

        if size > self.limits.max_content_size {
            return Err(ValidationError::ContentTooLarge {
                size,
                max: self.limits.max_content_size,
            });
        }

        Ok(())
    }

    /// Estimate the size of message content.
    fn estimate_content_size(content: &crate::proto::chat_v1::message::Content) -> usize {
        use crate::proto::chat_v1::message::content::Type;

        match &content.r#type {
            Some(Type::Text(text)) => text.len(),
            Some(Type::Encrypted(enc)) => enc.ciphertext.len(),
            Some(Type::File(file)) => file.name.len() + file.mime_type.len(),
            Some(Type::Reaction(_)) => 0, // Reactions are small, skip size check
            Some(Type::Mixed(mixed)) => {
                mixed.text.len()
                    + mixed
                        .files
                        .iter()
                        .map(|f| f.name.len() + f.mime_type.len())
                        .sum::<usize>()
            }
            Some(Type::AiBot(ai)) => ai.text.len(),
            Some(Type::SignalEncrypted(sig)) => sig.body.len(),
            Some(Type::SealedSender(sealed)) => sealed.sealed_body.len(),
            None => 0,
        }
    }

    /// Get the current validation limits.
    pub fn limits(&self) -> &ValidationLimits {
        &self.limits
    }
}

impl Default for ContentValidator {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Result of pipeline processing.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Message is the common case, boxing would add unnecessary indirection
pub enum PipelineResult {
    /// Message passed validation and is ready for processing.
    Ready(Message),
    /// Message is valid but buffered (out of order).
    Buffered,
    /// Message was a duplicate, already processed.
    Duplicate,
    /// Message failed validation.
    Invalid(ValidationError),
    /// Message was dropped (buffer full).
    Dropped,
}

/// Combined statistics from all pipeline stages.
#[derive(Debug, Clone, Copy)]
pub struct PipelineStats {
    /// Deduplicator statistics.
    pub deduplicator: DeduplicatorStats,
    /// Orderer statistics.
    pub orderer: OrdererStats,
}

/// Unified validation pipeline for incoming messages.
///
/// Processing order:
/// 1. Deduplication check (by message ID)
/// 2. Content validation (format, size, timestamp)
/// 3. Ordering (per-channel sequence)
///
/// # Thread Safety
///
/// All internal components use appropriate locking for thread-safe access.
pub struct MessageValidationPipeline {
    deduplicator: MessageDeduplicator,
    validator: ContentValidator,
    orderer: MessageOrderer,
}

impl MessageValidationPipeline {
    /// Create a new pipeline with default configuration.
    pub fn new() -> Self {
        Self {
            deduplicator: MessageDeduplicator::with_default_capacity(),
            validator: ContentValidator::with_defaults(),
            orderer: MessageOrderer::new(),
        }
    }

    /// Create a pipeline with custom configuration.
    ///
    /// # Arguments
    ///
    /// * `dedup_capacity` - Maximum number of message IDs to track for deduplication
    /// * `validation_limits` - Content validation limits
    /// * `orderer_buffer_size` - Maximum out-of-order messages to buffer per channel
    pub fn with_config(
        dedup_capacity: usize,
        validation_limits: ValidationLimits,
        orderer_buffer_size: usize,
    ) -> Self {
        Self {
            deduplicator: MessageDeduplicator::new(dedup_capacity),
            validator: ContentValidator::new(validation_limits),
            orderer: MessageOrderer::with_buffer_size(orderer_buffer_size),
        }
    }

    /// Process a message through the full validation pipeline.
    ///
    /// Processing order:
    /// 1. Check for duplicate by message ID
    /// 2. Validate content (format, size, timestamp)
    /// 3. Order the message per channel
    ///
    /// Returns the appropriate `PipelineResult` based on processing outcome.
    pub fn process(&self, message: Message) -> PipelineResult {
        // 1. Check for duplicate by message ID
        if self.deduplicator.is_duplicate(&message.id) {
            return PipelineResult::Duplicate;
        }

        // 2. Validate content
        if let Err(e) = self.validator.validate(&message) {
            return PipelineResult::Invalid(e);
        }

        // 3. Order the message
        match self.orderer.order(message) {
            OrderingResult::Ready(msg) => {
                // Mark as processed only when ready
                self.deduplicator.mark_processed(&msg.id);
                PipelineResult::Ready(msg)
            }
            OrderingResult::Buffered => PipelineResult::Buffered,
            OrderingResult::Duplicate => PipelineResult::Duplicate,
            OrderingResult::Dropped => PipelineResult::Dropped,
        }
    }

    /// Get messages that are now ready after processing.
    ///
    /// Call this after processing a `Ready` message to get buffered messages
    /// that are now in sequence.
    pub fn get_ready(&self, channel_id: &str) -> Vec<Message> {
        let ready = self.orderer.get_ready(channel_id);
        // Mark newly ready messages as processed
        for msg in &ready {
            self.deduplicator.mark_processed(&msg.id);
        }
        ready
    }

    /// Flush stale buffered messages.
    ///
    /// Returns messages that were flushed so they can be logged or processed anyway.
    /// Call this periodically to prevent indefinite buffering.
    pub fn flush_stale(&self) -> Vec<Message> {
        let stale = self.orderer.flush_stale();
        for msg in &stale {
            self.deduplicator.mark_processed(&msg.id);
        }
        stale
    }

    /// Get combined statistics from all pipeline stages.
    pub fn stats(&self) -> PipelineStats {
        PipelineStats {
            deduplicator: self.deduplicator.stats(),
            orderer: self.orderer.stats(),
        }
    }
}

impl Default for MessageValidationPipeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_valid_message() -> Message {
        Message {
            id: "01HQWX5K8MZGK1234567890AB".to_string(), // ULID format
            channel_id: "channel-123".to_string(),
            author_id: "user-456".to_string(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
            content: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_valid_message_passes() {
        let validator = ContentValidator::with_defaults();
        let message = make_valid_message();
        assert!(validator.validate(&message).is_ok());
    }

    #[test]
    fn test_empty_message_id_fails() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        message.id = String::new();

        let result = validator.validate(&message);
        assert!(matches!(result, Err(ValidationError::MissingField(f)) if f == "message_id"));
    }

    #[test]
    fn test_empty_channel_id_fails() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        message.channel_id = String::new();

        let result = validator.validate(&message);
        assert!(matches!(result, Err(ValidationError::MissingField(f)) if f == "channel_id"));
    }

    #[test]
    fn test_empty_author_id_allowed() {
        // Sealed sender messages have no author
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        message.author_id = String::new();

        assert!(validator.validate(&message).is_ok());
    }

    #[test]
    fn test_message_id_too_long() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        message.id = "a".repeat(100); // Over 64 char limit

        let result = validator.validate(&message);
        assert!(matches!(result, Err(ValidationError::InvalidMessageId)));
    }

    #[test]
    fn test_channel_id_too_long() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        message.channel_id = "a".repeat(100);

        let result = validator.validate(&message);
        assert!(matches!(result, Err(ValidationError::InvalidChannelId)));
    }

    #[test]
    fn test_invalid_id_characters() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        message.id = "msg!@#$%".to_string(); // Invalid characters

        let result = validator.validate(&message);
        assert!(matches!(result, Err(ValidationError::InvalidMessageId)));
    }

    #[test]
    fn test_timestamp_too_old() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        message.timestamp = 1_000_000_000_000_000_000; // Year 2001 in nanos, before 2020

        let result = validator.validate(&message);
        assert!(matches!(result, Err(ValidationError::InvalidTimestamp(_))));
    }

    #[test]
    fn test_timestamp_too_future() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        // 1 hour in the future (beyond 5 minute drift limit)
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        message.timestamp = now_nanos + (60 * 60 * 1_000_000_000);

        let result = validator.validate(&message);
        assert!(matches!(result, Err(ValidationError::InvalidTimestamp(_))));
    }

    #[test]
    fn test_timestamp_within_drift_ok() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        // 2 minutes in the future (within 5 minute drift limit)
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        message.timestamp = now_nanos + (2 * 60 * 1_000_000_000);

        assert!(validator.validate(&message).is_ok());
    }

    #[test]
    fn test_custom_limits() {
        let limits = ValidationLimits {
            max_content_size: 1024, // 1KB
            max_timestamp_drift_secs: 60, // 1 minute
            min_timestamp: 1_609_459_200_000_000_000, // 2021-01-01 in nanos
            max_id_length: 32,
        };
        let validator = ContentValidator::new(limits);

        assert_eq!(validator.limits().max_content_size, 1024);
        assert_eq!(validator.limits().max_id_length, 32);
    }

    #[test]
    fn test_uuid_format_valid() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        message.id = "550e8400-e29b-41d4-a716-446655440000".to_string();
        message.channel_id = "f47ac10b-58cc-4372-a567-0e02b2c3d479".to_string();

        assert!(validator.validate(&message).is_ok());
    }

    #[test]
    fn test_ulid_format_valid() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        message.id = "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string();

        assert!(validator.validate(&message).is_ok());
    }

    #[test]
    fn test_error_display() {
        let err = ValidationError::MissingField("message_id".to_string());
        assert_eq!(err.to_string(), "missing required field: message_id");

        let err = ValidationError::ContentTooLarge {
            size: 100000,
            max: 65536,
        };
        assert_eq!(
            err.to_string(),
            "content too large: 100000 bytes (max 65536)"
        );

        let err = ValidationError::InvalidTimestamp(12345);
        assert_eq!(err.to_string(), "timestamp out of range: 12345");
    }

    // ==========================================================================
    // Pipeline tests
    // ==========================================================================

    fn make_test_message(id: &str, channel_id: &str, timestamp: u64) -> Message {
        Message {
            id: id.to_string(),
            channel_id: channel_id.to_string(),
            author_id: "user-123".to_string(),
            timestamp,
            content: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_pipeline_dedup_then_validate() {
        let pipeline = MessageValidationPipeline::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        // First message should be ready
        let msg1 = make_test_message("msg-1", "channel-1", now);
        assert!(matches!(pipeline.process(msg1), PipelineResult::Ready(_)));

        // Same message ID should be duplicate
        let msg1_dup = make_test_message("msg-1", "channel-1", now + 100);
        assert!(matches!(pipeline.process(msg1_dup), PipelineResult::Duplicate));
    }

    #[test]
    fn test_pipeline_invalid_message() {
        let pipeline = MessageValidationPipeline::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        // Message with empty ID should fail validation
        let mut msg = make_test_message("", "channel-1", now);
        msg.id = String::new();
        match pipeline.process(msg) {
            PipelineResult::Invalid(ValidationError::MissingField(f)) => {
                assert_eq!(f, "message_id");
            }
            other => panic!("Expected Invalid result, got {:?}", other),
        }
    }

    #[test]
    fn test_pipeline_ready_message() {
        let pipeline = MessageValidationPipeline::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        // Valid message should be ready
        let msg = make_test_message("msg-1", "channel-1", now);
        match pipeline.process(msg) {
            PipelineResult::Ready(m) => {
                assert_eq!(m.id, "msg-1");
                assert_eq!(m.channel_id, "channel-1");
            }
            other => panic!("Expected Ready result, got {:?}", other),
        }
    }

    #[test]
    fn test_pipeline_buffered_then_ready() {
        let pipeline = MessageValidationPipeline::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        // First message at now + 1000
        let msg1 = make_test_message("msg-1", "channel-1", now + 1000);
        assert!(matches!(pipeline.process(msg1), PipelineResult::Ready(_)));

        // Message at now (older) should be buffered
        let msg2 = make_test_message("msg-2", "channel-1", now);
        assert!(matches!(pipeline.process(msg2), PipelineResult::Buffered));

        // Stats should show one buffered
        let stats = pipeline.stats();
        assert_eq!(stats.orderer.total_buffered, 1);
    }

    #[test]
    fn test_pipeline_with_config() {
        let limits = ValidationLimits {
            max_content_size: 1024,
            max_timestamp_drift_secs: 60,
            min_timestamp: 1_609_459_200_000_000_000, // 2021-01-01 in nanos
            max_id_length: 32,
        };
        let pipeline = MessageValidationPipeline::with_config(100, limits, 10);

        let stats = pipeline.stats();
        assert_eq!(stats.deduplicator.capacity, 100);
    }

    #[test]
    fn test_pipeline_stats() {
        let pipeline = MessageValidationPipeline::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        // Initial stats
        let stats = pipeline.stats();
        assert_eq!(stats.deduplicator.tracked_count, 0);
        assert_eq!(stats.orderer.channels_tracked, 0);

        // Process a message
        let msg = make_test_message("msg-1", "channel-1", now);
        pipeline.process(msg);

        let stats = pipeline.stats();
        assert_eq!(stats.deduplicator.tracked_count, 1);
        assert_eq!(stats.orderer.channels_tracked, 1);
    }

    #[test]
    fn test_pipeline_get_ready() {
        let pipeline = MessageValidationPipeline::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        // First message
        let msg1 = make_test_message("msg-1", "channel-1", now);
        pipeline.process(msg1);

        // Old message gets buffered
        let msg2 = make_test_message("msg-2", "channel-1", now - 100);
        pipeline.process(msg2);

        // get_ready should clean up old buffered messages
        let ready = pipeline.get_ready("channel-1");
        // Old message is discarded, so ready should be empty
        assert!(ready.is_empty());
    }

    #[test]
    fn test_pipeline_default() {
        let pipeline = MessageValidationPipeline::default();
        let stats = pipeline.stats();
        assert_eq!(stats.deduplicator.capacity, 10_000); // Default capacity
    }
}
