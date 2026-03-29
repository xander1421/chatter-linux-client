//! Data access repositories.
//!
//! Repository pattern implementations for data access.
//!
//! # Available Repositories
//!
//! - [`UserRepository`] - User profiles and friendship management
//! - [`ChannelRepository`] - Channel and membership CRUD
//! - [`MessageRepository`] - Message storage with cursor pagination
//! - [`AttachmentRepository`] - File attachment metadata
//! - [`ReactionRepository`] - Message reactions (toggle pattern)
//! - [`DraftRepository`] - Message draft persistence
//! - [`SettingsRepository`] - Key-value application settings
//! - [`SearchIndex`] - FTS5 full-text message search
//!
//! # Usage
//!
//! All repositories take `Arc<Database>` for dependency injection:
//!
//! ```ignore
//! let db = Arc::new(Database::open("data.db", "password")?);
//! let users = UserRepository::new(db.clone());
//! let settings = SettingsRepository::new(db.clone());
//! ```

pub mod attachment_repo;
pub mod call_repo;
pub mod channel_repo;
pub mod draft_repo;
pub mod ephemeral_repo;
pub mod message_repo;
pub mod reaction_repo;
pub mod search_index;
pub mod settings_repo;
pub mod user_repo;
pub mod anon_group_repo;
pub mod anon_dm_repo;
pub(crate) mod repo_utils;
pub mod jarvis_memory_repo;
pub mod jarvis_graph_repo;
pub mod unified_message_repo;

pub use attachment_repo::AttachmentRepository;
pub use channel_repo::ChannelRepository;
pub use draft_repo::DraftRepository;
pub use ephemeral_repo::EphemeralRepository;
pub use message_repo::MessageRepository;
pub use reaction_repo::ReactionRepository;
pub use search_index::{SearchIndex, SearchResult};
pub use settings_repo::{keys as settings_keys, SettingsRepository};
pub use user_repo::UserRepository;
pub use anon_group_repo::AnonGroupRepository;
pub use anon_dm_repo::AnonDmRepository;
pub use jarvis_memory_repo::JarvisMemoryRepository;
pub use jarvis_graph_repo::JarvisGraphRepository;
pub use unified_message_repo::UnifiedMessageRepository;
