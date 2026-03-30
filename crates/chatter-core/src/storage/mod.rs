//! Persistent storage layer.
//!
//! SQLCipher-backed persistence with proper migrations.

pub mod blob_store;
pub mod blob_stream;
pub mod cache;
pub mod database;
pub mod key_store;
pub mod migrations;
pub mod repositories;
pub mod serialization;

pub use database::Database;
pub use key_store::KeyStore;
pub use repositories::AttachmentRepository;
pub use repositories::ChannelRepository;
pub use repositories::DraftRepository;
pub use repositories::MessageRepository;
pub use repositories::ReactionRepository;
pub use repositories::SearchIndex;
pub use repositories::SearchResult;
pub use repositories::SettingsRepository;
pub use repositories::UserRepository;
pub use repositories::SectionRepository;
pub use repositories::settings_keys;
