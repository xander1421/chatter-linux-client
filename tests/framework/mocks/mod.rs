//! Auto-generated mocks using mockall.
//!
//! This module provides mock implementations of key services and traits.
//! Mocks are auto-generated using the `mockall` crate.
//!
//! # Usage
//!
//! ```rust
//! use crate::framework::mocks::*;
//!
//! let mut mock = MockMessageService::new();
//! mock.expect_send_message()
//!     .returning(|_, _| Ok(Message::default()));
//! ```

pub mod services;
pub mod signal;
pub mod storage;
pub mod network;

// Re-export all mocks
pub use services::*;
pub use signal::*;
pub use storage::*;
pub use network::*;

use std::sync::Arc;
use tokio::sync::RwLock;

/// Container for all mock services.
///
/// Provides a convenient way to create and manage multiple mocks.
#[derive(Default)]
pub struct MockServices {
    /// Mock message service.
    pub message: Option<Arc<RwLock<MockMessageServiceTrait>>>,
    /// Mock channel service.
    pub channel: Option<Arc<RwLock<MockChannelServiceTrait>>>,
    /// Mock user service.
    pub user: Option<Arc<RwLock<MockUserServiceTrait>>>,
    /// Mock signal service.
    pub signal: Option<Arc<RwLock<MockSignalServiceTrait>>>,
    /// Mock storage.
    pub storage: Option<Arc<RwLock<MockStorageTrait>>>,
}

impl MockServices {
    /// Create a new empty mock services container.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with all mocks initialized to default behavior.
    pub fn with_defaults() -> Self {
        Self {
            message: Some(Arc::new(RwLock::new(MockMessageServiceTrait::new()))),
            channel: Some(Arc::new(RwLock::new(MockChannelServiceTrait::new()))),
            user: Some(Arc::new(RwLock::new(MockUserServiceTrait::new()))),
            signal: Some(Arc::new(RwLock::new(MockSignalServiceTrait::new()))),
            storage: Some(Arc::new(RwLock::new(MockStorageTrait::new()))),
        }
    }
}
