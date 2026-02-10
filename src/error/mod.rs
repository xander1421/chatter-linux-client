//! Error handling.
//!
//! Application error types and recovery.

pub mod app_error;
pub mod crypto_error;
pub mod mailbox_error;
pub mod media_error;
pub mod network_error;
pub mod recovery;
pub mod storage_error;
pub mod system_error;

pub use crypto_error::CryptoError;
pub use mailbox_error::{ContentError, MailboxError};
pub use media_error::MediaError;
pub use network_error::NetworkError;
pub use storage_error::StorageError;
pub use system_error::SystemError;
pub use service_error::ServiceError;

pub mod service_error;
