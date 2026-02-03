//! Content validation.
//!
//! Content validation pipeline for incoming messages.

use thiserror::Error;

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
            max_content_size: 64 * 1024,           // 64KB
            max_timestamp_drift_secs: 5 * 60,      // 5 minutes
            min_timestamp: 1577836800000,          // 2020-01-01 00:00:00 UTC in millis
            max_id_length: 64,                     // ULIDs/UUIDs are ~26-36 chars
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
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let max_future = now_millis + (self.limits.max_timestamp_drift_secs * 1000);
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
            Some(Type::Call(_)) => 0, // Call notifications are small, skip size check
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

/// Message validation pipeline (placeholder for future expansion).
pub struct MessageValidationPipeline;

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
                .as_millis() as u64,
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
        message.timestamp = 1000000000000; // Year 2001, before 2020

        let result = validator.validate(&message);
        assert!(matches!(result, Err(ValidationError::InvalidTimestamp(_))));
    }

    #[test]
    fn test_timestamp_too_future() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        // 1 hour in the future (beyond 5 minute drift limit)
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        message.timestamp = now_millis + (60 * 60 * 1000);

        let result = validator.validate(&message);
        assert!(matches!(result, Err(ValidationError::InvalidTimestamp(_))));
    }

    #[test]
    fn test_timestamp_within_drift_ok() {
        let validator = ContentValidator::with_defaults();
        let mut message = make_valid_message();
        // 2 minutes in the future (within 5 minute drift limit)
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        message.timestamp = now_millis + (2 * 60 * 1000);

        assert!(validator.validate(&message).is_ok());
    }

    #[test]
    fn test_custom_limits() {
        let limits = ValidationLimits {
            max_content_size: 1024, // 1KB
            max_timestamp_drift_secs: 60, // 1 minute
            min_timestamp: 1609459200000, // 2021-01-01
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
}
