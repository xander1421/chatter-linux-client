//! System integration errors.
//!
//! Error types for OS integration operations.

/// Errors from system integration operations.
#[derive(Debug, thiserror::Error)]
pub enum SystemError {
    /// Failed to determine system directories.
    #[error("Failed to determine system directories")]
    DirectoryInit,

    /// Failed to create a directory.
    #[error("Failed to create directory: {0}")]
    DirectoryCreate(#[source] std::io::Error),

    /// Keyring operation failed.
    #[error("Keyring error: {0}")]
    Keyring(String),
}
