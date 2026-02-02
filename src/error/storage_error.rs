//! Storage-specific errors.
//!
//! Errors related to storage operations.

/// Storage error.
pub enum StorageError {
    /// Database error.
    DatabaseError,
    /// Migration failed.
    MigrationFailed,
    /// Not found.
    NotFound,
}
