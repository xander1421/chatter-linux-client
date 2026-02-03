//! Storage-specific errors.
//!
//! Errors related to database, migration, and storage operations.

use thiserror::Error;

/// Storage layer error types.
#[derive(Debug, Error)]
pub enum StorageError {
    /// Database operation failure.
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// Connection pool exhaustion or creation failure.
    #[error("Connection pool error: {0}")]
    ConnectionPool(String),

    /// Migration execution failure.
    #[error("Migration error: {0}")]
    Migration(String),

    /// Entity not found.
    #[error("Not found: {0}")]
    NotFound(String),

    /// Data corruption or serialization issues.
    #[error("Invalid data: {0}")]
    InvalidData(String),

    /// SQLCipher encryption/decryption failure.
    #[error("Encryption error: {0}")]
    Encryption(String),
}
