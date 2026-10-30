//! Database serialization helpers.
//!
//! Provides bincode 2 standard encoding (little-endian variable integers) for database storage.
//! Bincode offers ~10x faster serialization and ~50% smaller storage compared to JSON.

use serde::{Deserialize, Serialize};

use crate::error::StorageError;

/// Serialize a value to bincode bytes for database BLOB storage.
///
/// # Performance
/// - ~10x faster than JSON serialization
/// - ~50% smaller output
///
/// # Example
/// ```ignore
/// let attachments: Vec<AttachmentId> = vec![...];
/// let blob = db_serialize(&attachments)?;
/// ```
#[inline]
pub fn db_serialize<T: Serialize>(value: &T) -> Result<Vec<u8>, StorageError> {
    bincode::serde::encode_to_vec(value, bincode::config::standard())
        .map_err(|e| StorageError::InvalidData(format!("bincode serialize: {}", e)))
}

/// Deserialize a value from bincode bytes.
///
/// # Example
/// ```ignore
/// let blob: Vec<u8> = row.get(0)?;
/// let attachments: Vec<AttachmentId> = db_deserialize(&blob)?;
/// ```
#[inline]
pub fn db_deserialize<T: for<'de> Deserialize<'de>>(data: &[u8]) -> Result<T, StorageError> {
    bincode::serde::decode_from_slice(data, bincode::config::standard())
        .and_then(|(value, consumed)| {
            if consumed == data.len() {
                Ok(value)
            } else {
                Err(bincode::error::DecodeError::Other("trailing bytes"))
            }
        })
        .map_err(|e| StorageError::InvalidData(format!("bincode deserialize: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::{AttachmentId, IntegrationId, UserId};
    use crate::domain::message::ReactionCount;

    // ========================================================================
    // Basic type tests
    // ========================================================================

    #[test]
    fn test_roundtrip_vec_string() {
        let original: Vec<String> = vec!["hello".to_string(), "world".to_string()];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<String> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_roundtrip_empty_vec() {
        let original: Vec<u32> = vec![];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<u32> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_roundtrip_nested_vec() {
        let original: Vec<Vec<u8>> = vec![vec![1, 2, 3], vec![4, 5], vec![]];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<Vec<u8>> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    // ========================================================================
    // Domain type tests (IDs)
    // ========================================================================

    #[test]
    fn test_roundtrip_attachment_ids() {
        let original: Vec<AttachmentId> = vec![
            AttachmentId::new(),
            AttachmentId::new(),
            AttachmentId::new(),
        ];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<AttachmentId> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_roundtrip_empty_attachment_ids() {
        let original: Vec<AttachmentId> = vec![];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<AttachmentId> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
        // Empty vec should be small
        assert!(serialized.len() <= 8);
    }

    #[test]
    fn test_roundtrip_user_ids() {
        let original: Vec<UserId> = vec![UserId::new(), UserId::new()];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<UserId> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_roundtrip_integration_ids() {
        let original: Vec<IntegrationId> = vec![IntegrationId::new(), IntegrationId::new()];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<IntegrationId> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    // ========================================================================
    // Domain type tests (complex structs)
    // ========================================================================

    #[test]
    fn test_roundtrip_reaction_counts() {
        let original: Vec<ReactionCount> = vec![
            ReactionCount {
                emoji: "👍".to_string(),
                count: 5,
            },
            ReactionCount {
                emoji: "❤️".to_string(),
                count: 3,
            },
            ReactionCount {
                emoji: "🎉".to_string(),
                count: 1,
            },
        ];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<ReactionCount> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_roundtrip_empty_reaction_counts() {
        let original: Vec<ReactionCount> = vec![];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<ReactionCount> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_roundtrip_current_user_reactions() {
        let original: Vec<String> = vec!["👍".to_string(), "❤️".to_string()];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<String> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    // ========================================================================
    // Size comparison tests
    // ========================================================================

    #[test]
    fn test_size_comparison_strings() {
        let data: Vec<String> = (0..100).map(|i| format!("item_{}", i)).collect();

        let bincode_size = db_serialize(&data).unwrap().len();
        let json_size = serde_json::to_vec(&data).unwrap().len();

        // Both formats should produce valid output
        assert!(bincode_size > 0);
        assert!(json_size > 0);
    }

    #[test]
    fn test_size_comparison_uuids() {
        let data: Vec<AttachmentId> = (0..100).map(|_| AttachmentId::new()).collect();

        let bincode_size = db_serialize(&data).unwrap().len();
        let json_size = serde_json::to_vec(&data).unwrap().len();

        // Both formats should produce valid output
        assert!(bincode_size > 0);
        assert!(json_size > 0);
    }

    #[test]
    fn test_size_comparison_reaction_counts() {
        let data: Vec<ReactionCount> = vec![
            ReactionCount {
                emoji: "👍".to_string(),
                count: 100,
            },
            ReactionCount {
                emoji: "❤️".to_string(),
                count: 50,
            },
            ReactionCount {
                emoji: "😂".to_string(),
                count: 25,
            },
            ReactionCount {
                emoji: "🎉".to_string(),
                count: 10,
            },
            ReactionCount {
                emoji: "🔥".to_string(),
                count: 5,
            },
        ];

        let bincode_size = db_serialize(&data).unwrap().len();
        let json_size = serde_json::to_vec(&data).unwrap().len();

        // Bincode should be smaller
        assert!(bincode_size < json_size);
    }

    // ========================================================================
    // Error handling tests
    // ========================================================================

    #[test]
    fn test_invalid_data_returns_error() {
        let invalid_bytes = vec![0xFF, 0xFF, 0xFF];
        let result: Result<Vec<String>, _> = db_deserialize(&invalid_bytes);
        assert!(result.is_err());

        let err = result.unwrap_err();
        match err {
            StorageError::InvalidData(msg) => {
                assert!(msg.contains("bincode"));
            }
            _ => panic!("Expected InvalidData error"),
        }
    }

    #[test]
    fn test_truncated_data_returns_error() {
        let original: Vec<AttachmentId> = vec![AttachmentId::new(), AttachmentId::new()];
        let mut serialized = db_serialize(&original).unwrap();

        // Truncate the data
        serialized.truncate(serialized.len() / 2);

        let result: Result<Vec<AttachmentId>, _> = db_deserialize(&serialized);
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_data_for_vec() {
        // Empty bytes should fail for Vec (needs at least length prefix)
        let result: Result<Vec<String>, _> = db_deserialize(&[]);
        assert!(result.is_err());
    }

    // ========================================================================
    // Edge case tests
    // ========================================================================

    #[test]
    fn test_large_vec() {
        let original: Vec<u64> = (0..10000).collect();
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<u64> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_unicode_strings() {
        let original: Vec<String> = vec![
            "Hello, 世界!".to_string(),
            "Emoji: 👍🎉❤️🔥".to_string(),
            "Mixed: café résumé naïve".to_string(),
            "🇺🇸🇬🇧🇯🇵".to_string(),
        ];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<String> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    #[test]
    fn test_special_characters() {
        let original: Vec<String> = vec![
            "null\0byte".to_string(),
            "tab\there".to_string(),
            "newline\nhere".to_string(),
            "quote\"here".to_string(),
            "backslash\\here".to_string(),
        ];
        let serialized = db_serialize(&original).unwrap();
        let deserialized: Vec<String> = db_deserialize(&serialized).unwrap();
        assert_eq!(original, deserialized);
    }

    // ========================================================================
    // Determinism tests
    // ========================================================================

    #[test]
    fn test_serialization_is_deterministic() {
        let original: Vec<String> = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        let serialized1 = db_serialize(&original).unwrap();
        let serialized2 = db_serialize(&original).unwrap();

        assert_eq!(serialized1, serialized2);
    }

    #[test]
    fn test_uuid_serialization_is_deterministic() {
        let id = AttachmentId::new();
        let original: Vec<AttachmentId> = vec![id, id, id];

        let serialized1 = db_serialize(&original).unwrap();
        let serialized2 = db_serialize(&original).unwrap();

        assert_eq!(serialized1, serialized2);
    }
}
