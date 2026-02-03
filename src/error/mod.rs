//! Error handling.
//!
//! Application error types and recovery.

pub mod app_error;
pub mod crypto_error;
pub mod network_error;
pub mod recovery;
pub mod storage_error;

pub use storage_error::StorageError;
