use thiserror::Error;
use crate::error::CryptoError;
use crate::error::NetworkError;
use crate::error::StorageError;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("Not logged in")]
    NotLoggedIn,
    #[error("Validation error: {0}")]
    Validation(String),
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("Network error: {0}")]
    Network(#[from] NetworkError),
    #[error("Crypto error: {0}")]
    Crypto(#[from] CryptoError),
}
