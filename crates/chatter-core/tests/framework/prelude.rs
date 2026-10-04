//! Prelude module - import everything you need for testing.
//!
//! # Usage
//!
//! ```rust
//! use crate::framework::prelude::*;
//! ```

// Re-export builders
pub use crate::framework::builders::{
    UserBuilder, MessageBuilder, ChannelBuilder, AttachmentBuilder,
    CryptoBuilder, SessionBuilder,
};

// Re-export harness
pub use crate::framework::harness::TestHarness;

// Re-export fixtures
pub use crate::framework::fixtures::{
    unique_username, random_password,
    random_public_key, random_identity_key, random_uak, generate_prekey_bundle,
    unique_channel_name, random_bytes, fake_cdn_url,
};

// Re-export common external crates used in tests
pub use tokio;
pub use tokio_stream::StreamExt;
