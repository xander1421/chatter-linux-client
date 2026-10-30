//! Prelude module - import everything you need for testing.
//!
//! # Usage
//!
//! ```rust
//! use crate::framework::prelude::*;
//! ```

// Re-export builders
pub use crate::framework::builders::{
    AttachmentBuilder, ChannelBuilder, CryptoBuilder, MessageBuilder, SessionBuilder, UserBuilder,
};

// Re-export harness
pub use crate::framework::harness::TestHarness;

// Re-export fixtures
pub use crate::framework::fixtures::{
    fake_cdn_url, generate_prekey_bundle, random_bytes, random_identity_key, random_password,
    random_public_key, random_uak, unique_channel_name, unique_username,
};

// Re-export common external crates used in tests
pub use tokio;
pub use tokio_stream::StreamExt;
