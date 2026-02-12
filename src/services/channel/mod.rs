//! Channel service module.
//!
//! Provides channel creation, management, membership, and moderation operations.

pub mod converters;
pub mod error;
pub mod service;

// Re-export main types for convenience
pub use converters::proto_to_domain_channel;
pub use error::ChannelError;
pub use service::ChannelService;
