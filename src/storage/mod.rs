//! Persistent storage layer.
//!
//! SQLCipher-backed persistence with proper migrations.

pub mod blob_store;
pub mod cache;
pub mod database;
pub mod key_store;
pub mod migrations;
pub mod repositories;

pub use database::Database;
pub use repositories::AttachmentRepository;
pub use repositories::ChannelRepository;
pub use repositories::DraftRepository;
pub use repositories::MessageRepository;
pub use repositories::ReactionRepository;
pub use repositories::UserRepository;
