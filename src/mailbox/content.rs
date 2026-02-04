//! Mailbox content.
//!
//! Mailbox message content encoding (NDSS padding).
//!
//! Uses log-bucketing to pad messages to discrete size buckets, preventing
//! message size inference attacks. All messages within a bucket appear
//! identical in size to the server.

use rand::RngCore;
use thiserror::Error;

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

/// Content encoding errors.
#[derive(Debug, Clone, Error)]
pub enum ContentError {
    /// Content exceeds maximum size.
    #[error("content too large: {0} bytes (max {1})")]
    ContentTooLarge(usize, usize),

    /// Invalid padded content (corrupted or tampered).
    #[error("invalid padded content: {0}")]
    InvalidPadding(String),
}

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
            rand::rng().fill_bytes(&mut padded[padding_start..]);
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
