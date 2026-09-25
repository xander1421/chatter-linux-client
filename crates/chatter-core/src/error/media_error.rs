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

    /// Audio/video playback error.
    #[error("playback error: {0}")]
    PlaybackError(String),

    /// Audio recording error.
    #[error("recording error: {0}")]
    RecordingError(String),

    /// Preview error (PDF, code, etc.).
    #[error("preview error: {0}")]
    PreviewError(String),

    /// GIF API error.
    #[error("gif error: {0}")]
    GifError(String),
}
