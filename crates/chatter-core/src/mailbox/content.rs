//! Mailbox content.
//!
//! Mailbox message content encoding (NDSS padding).
//!
//! Uses log-bucketing to pad messages to discrete size buckets, preventing
//! message size inference attacks. All messages within a bucket appear
//! identical in size to the server.

use rand::RngCore;

use crate::error::ContentError;

/// Minimum padded content size for NDSS privacy.
/// Small enough to not waste bandwidth, large enough to hide typical messages.
pub const MIN_PADDED_SIZE: usize = 256;

/// Maximum unpadded content size we support.
pub const MAX_CONTENT_SIZE: usize = 64 * 1024; // 64KB

/// Log-bucketing base (1.05 = 5% size increase per bucket).
const BUCKET_BASE: f64 = 1.05;

/// Block size for padding alignment (AES block size).
const BLOCK_SIZE: usize = 16;

/// Length prefix size (4 bytes for u32 little-endian).
const LENGTH_PREFIX_SIZE: usize = 4;

/// Mailbox content encoder with NDSS-style padding.
///
/// Prevents message size inference by padding all messages to discrete
/// size buckets using the log-bucketing algorithm.
///
/// # Format
///
/// Padded content: `[length: u32 LE (4 bytes)] [original content] [random padding]`
///
/// - Length prefix stores original content size
/// - Random padding fills to bucket size (not zeros, to prevent oracle attacks)
/// - Total size is always a multiple of 16 (AES block size)
///
/// # Example
///
/// ```ignore
/// use chatter::mailbox::ContentEncoder;
///
/// let content = b"Hello, anonymous mailbox!";
/// let padded = ContentEncoder::pad(content)?;
/// let unpadded = ContentEncoder::unpad(&padded)?;
/// assert_eq!(unpadded, content);
/// ```
pub struct ContentEncoder;

impl ContentEncoder {
    /// Pad content using log-bucketing algorithm.
    ///
    /// # Arguments
    ///
    /// * `content` - The content to pad (max 64KB)
    ///
    /// # Returns
    ///
    /// Padded content with length prefix and random padding.
    ///
    /// # Errors
    ///
    /// Returns `ContentError::ContentTooLarge` if content exceeds 64KB.
    pub fn pad(content: &[u8]) -> Result<Vec<u8>, ContentError> {
        if content.len() > MAX_CONTENT_SIZE {
            return Err(ContentError::ContentTooLarge(content.len(), MAX_CONTENT_SIZE));
        }

        let padded_size = Self::padded_size(content.len());
        let mut padded = vec![0u8; padded_size];

        // Write 4-byte little-endian length prefix
        let length_bytes = (content.len() as u32).to_le_bytes();
        padded[..LENGTH_PREFIX_SIZE].copy_from_slice(&length_bytes);

        // Copy original content
        padded[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + content.len()].copy_from_slice(content);

        // Fill remaining with random bytes (not zeros)
        let padding_start = LENGTH_PREFIX_SIZE + content.len();
        if padding_start < padded_size {
            {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut padded[padding_start..]).expect("OS CSPRNG must not fail");}
        }

        Ok(padded)
    }

    /// Remove padding and recover original content.
    ///
    /// # Arguments
    ///
    /// * `padded` - The padded content to unpad
    ///
    /// # Returns
    ///
    /// The original content.
    ///
    /// # Errors
    ///
    /// Returns `ContentError::InvalidPadding` if:
    /// - Padded content is too short (< 4 bytes)
    /// - Length prefix claims more data than available
    pub fn unpad(padded: &[u8]) -> Result<Vec<u8>, ContentError> {
        if padded.len() < LENGTH_PREFIX_SIZE {
            return Err(ContentError::InvalidPadding(format!(
                "too short: {} bytes (need at least {})",
                padded.len(),
                LENGTH_PREFIX_SIZE
            )));
        }

        // Extract 4-byte little-endian length prefix
        let length_bytes: [u8; 4] = padded[..LENGTH_PREFIX_SIZE]
            .try_into()
            .expect("slice length checked");
        let original_length = u32::from_le_bytes(length_bytes) as usize;

        // Validate length
        let available = padded.len() - LENGTH_PREFIX_SIZE;
        if original_length > available {
            return Err(ContentError::InvalidPadding(format!(
                "length prefix {} exceeds available data {}",
                original_length, available
            )));
        }

        // Extract original content
        let content = padded[LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + original_length].to_vec();
        Ok(content)
    }

    /// Compute padded size using log-bucketing algorithm.
    ///
    /// Formula: `max(MIN_PADDED_SIZE, ceil(1.05^ceil(log(size+4)/log(1.05))))`
    /// rounded up to next multiple of 16 (AES block size).
    ///
    /// The +4 accounts for the length prefix.
    #[must_use]
    pub fn padded_size(original_size: usize) -> usize {
        // Total size needed: length prefix + content
        let total_size = LENGTH_PREFIX_SIZE + original_size;

        if total_size == LENGTH_PREFIX_SIZE {
            // Empty content: pad to minimum
            return Self::round_to_block_size(MIN_PADDED_SIZE);
        }

        // Log-bucketing: ceil(1.05^ceil(log(size)/log(1.05)))
        let bucket = (total_size as f64).ln() / BUCKET_BASE.ln();
        let rounded = bucket.ceil();
        let padded = BUCKET_BASE.powf(rounded) as usize;
        let result = padded.max(MIN_PADDED_SIZE);

        // Ensure result is a multiple of block size
        Self::round_to_block_size(result)
    }

    /// Round up to the next multiple of block size (16 bytes).
    #[inline]
    fn round_to_block_size(size: usize) -> usize {
        (size + BLOCK_SIZE - 1) & !(BLOCK_SIZE - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ContentError;

    #[test]
    fn test_pad_unpad_roundtrip() {
        let content = b"Hello, anonymous mailbox!";
        let padded = ContentEncoder::pad(content).unwrap();
        let unpadded = ContentEncoder::unpad(&padded).unwrap();
        assert_eq!(unpadded, content);
    }

    #[test]
    fn test_pad_unpad_empty() {
        let content = b"";
        let padded = ContentEncoder::pad(content).unwrap();
        assert!(padded.len() >= MIN_PADDED_SIZE);
        let unpadded = ContentEncoder::unpad(&padded).unwrap();
        assert_eq!(unpadded, content);
    }

    #[test]
    fn test_padded_size_minimum() {
        // Small content should pad to MIN_PADDED_SIZE
        assert!(ContentEncoder::padded_size(10) >= MIN_PADDED_SIZE);
        assert_eq!(ContentEncoder::padded_size(10) % BLOCK_SIZE, 0);
    }

    #[test]
    fn test_padded_size_log_bucketing() {
        // Different small sizes should bucket to same padded size (or discrete buckets)
        let size_100 = ContentEncoder::padded_size(100);
        let size_110 = ContentEncoder::padded_size(110);
        // At least verify they're block-aligned
        assert_eq!(size_100 % BLOCK_SIZE, 0);
        assert_eq!(size_110 % BLOCK_SIZE, 0);
    }

    #[test]
    fn test_content_too_large() {
        let content = vec![0u8; MAX_CONTENT_SIZE + 1];
        let result = ContentEncoder::pad(&content);
        assert!(matches!(result, Err(ContentError::ContentTooLarge(_, _))));
    }

    #[test]
    fn test_invalid_padding_too_short() {
        let padded = vec![0u8; 3]; // Less than 4-byte length prefix
        let result = ContentEncoder::unpad(&padded);
        assert!(matches!(result, Err(ContentError::InvalidPadding(_))));
    }

    #[test]
    fn test_invalid_padding_wrong_length() {
        // Create valid-looking padded content but with wrong length prefix
        let mut padded = vec![0u8; 256];
        // Set length prefix to 500 (more than available)
        padded[..4].copy_from_slice(&500u32.to_le_bytes());
        let result = ContentEncoder::unpad(&padded);
        assert!(matches!(result, Err(ContentError::InvalidPadding(_))));
    }

    #[test]
    fn test_same_content_different_padding() {
        // Same content should produce different padded bytes (random fill)
        let content = b"test message";
        let padded1 = ContentEncoder::pad(content).unwrap();
        let padded2 = ContentEncoder::pad(content).unwrap();

        // Length prefix and content should be the same
        assert_eq!(&padded1[..4], &padded2[..4]);
        assert_eq!(
            &padded1[4..4 + content.len()],
            &padded2[4..4 + content.len()]
        );

        // Padding bytes should differ (with very high probability)
        let padding_start = 4 + content.len();
        if padded1.len() > padding_start {
            assert_ne!(
                &padded1[padding_start..],
                &padded2[padding_start..],
                "random padding should differ"
            );
        }

        // Both should unpad to same content
        assert_eq!(ContentEncoder::unpad(&padded1).unwrap(), content);
        assert_eq!(ContentEncoder::unpad(&padded2).unwrap(), content);
    }

    #[test]
    fn test_padded_size_always_larger() {
        for size in [0, 1, 100, 1000, 10_000, 50_000] {
            let padded = ContentEncoder::padded_size(size);
            // Padded size must be >= original + 4 (length prefix)
            assert!(
                padded >= size + 4,
                "padded_size({}) = {} should be >= {}",
                size,
                padded,
                size + 4
            );
            assert_eq!(
                padded % BLOCK_SIZE,
                0,
                "padded_size({}) should be multiple of {}",
                size,
                BLOCK_SIZE
            );
        }
    }

    #[test]
    fn test_max_content_boundary() {
        let content = vec![0u8; MAX_CONTENT_SIZE];
        let result = ContentEncoder::pad(&content);
        assert!(result.is_ok());

        let padded = result.unwrap();
        let unpadded = ContentEncoder::unpad(&padded).unwrap();
        assert_eq!(unpadded.len(), MAX_CONTENT_SIZE);
    }

    #[test]
    fn test_padded_output_format() {
        let content = b"hello world";
        let padded = ContentEncoder::pad(content).unwrap();

        // Check length prefix
        let length_bytes: [u8; 4] = padded[..4].try_into().unwrap();
        let length = u32::from_le_bytes(length_bytes) as usize;
        assert_eq!(length, content.len());

        // Check content preserved
        assert_eq!(&padded[4..4 + content.len()], content);

        // Check total size is block-aligned
        assert_eq!(padded.len() % BLOCK_SIZE, 0);
    }

    #[test]
    fn test_round_to_block_size() {
        // Internal function test via padded_size
        assert_eq!(ContentEncoder::padded_size(0) % 16, 0);
        assert_eq!(ContentEncoder::padded_size(1) % 16, 0);
        assert_eq!(ContentEncoder::padded_size(15) % 16, 0);
        assert_eq!(ContentEncoder::padded_size(16) % 16, 0);
        assert_eq!(ContentEncoder::padded_size(17) % 16, 0);
    }

    #[test]
    fn test_various_content_sizes() {
        for size in [1, 10, 50, 100, 200, 500, 1000, 5000, 10_000, 30_000] {
            let content: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
            let padded = ContentEncoder::pad(&content).expect("pad should succeed");
            let unpadded = ContentEncoder::unpad(&padded).expect("unpad should succeed");
            assert_eq!(
                unpadded, content,
                "roundtrip failed for size {}",
                size
            );
        }
    }

    #[test]
    fn test_error_messages() {
        let err = ContentError::ContentTooLarge(100_000, MAX_CONTENT_SIZE);
        assert!(err.to_string().contains("100000"));
        assert!(err.to_string().contains(&MAX_CONTENT_SIZE.to_string()));

        let err = ContentError::InvalidPadding("test error".to_string());
        assert!(err.to_string().contains("test error"));
    }
}
