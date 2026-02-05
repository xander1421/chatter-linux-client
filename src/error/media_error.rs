//! Media error types.
//!
//! Error types for media processing operations.

use std::io;
use thiserror::Error;

/// Errors that can occur during media operations.
#[derive(Error, Debug)]
pub enum MediaError {
    /// Error reading a file.
    #[error("failed to read file: {0}")]
    FileRead(#[from] io::Error),

    /// Invalid file format.
    #[error("invalid format: {0}")]
    InvalidFormat(String),

    /// Thumbnail generation failed.
    #[error("thumbnail generation failed: {0}")]
    ThumbnailGeneration(String),

    /// Cache operation failed.
    #[error("cache error: {0}")]
    CacheError(String),
}
