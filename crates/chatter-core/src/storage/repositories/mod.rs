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
// message_repo.rs deleted — all callers now use UnifiedMessageRepository via `MessageRepository` alias.
pub mod reaction_repo;
pub mod search_index;
pub mod settings_repo;
pub mod user_repo;
pub mod anon_group_repo;
pub mod anon_dm_repo;
pub mod mesh_conversation_repo;
pub mod mesh_delivery_repo;
pub mod mesh_favorites_repo;
pub(crate) mod repo_utils;
#[cfg(feature = "jarvis")]
pub mod jarvis_memory_repo;
#[cfg(feature = "jarvis")]
pub mod jarvis_graph_repo;
pub mod section_repo;
pub mod shared_content_repo;
pub mod unified_conversation_repo;
pub mod unified_message_repo;
pub mod vector_repo;

pub use attachment_repo::AttachmentRepository;
pub use call_repo::CallHistoryRepository;
pub use channel_repo::ChannelRepository;
pub use draft_repo::DraftRepository;
pub use ephemeral_repo::EphemeralRepository;
pub use unified_conversation_repo::UnifiedConversationRepository;
/// Re-export `UnifiedMessageRepository` as `MessageRepository` — all code now uses the
/// unified implementation. The old `message_repo` module is kept for step 5 deletion.
pub use unified_message_repo::UnifiedMessageRepository as MessageRepository;
pub use unified_message_repo::UnifiedMessageRepository;
pub use reaction_repo::ReactionRepository;
pub use search_index::{SearchIndex, SearchResult};
pub use shared_content_repo::{SharedContentRepository, SharedContentRow};
pub use settings_repo::{keys as settings_keys, SettingsRepository};
pub use user_repo::UserRepository;
pub use anon_group_repo::AnonGroupRepository;
pub use anon_dm_repo::AnonDmRepository;
pub use mesh_conversation_repo::{MeshConversationRepository, StoredMeshConversation};
pub use mesh_delivery_repo::MeshDeliveryRepository;
pub use mesh_favorites_repo::MeshFavoritesRepository;
#[cfg(feature = "jarvis")]
pub use jarvis_memory_repo::JarvisMemoryRepository;
#[cfg(feature = "jarvis")]
pub use jarvis_graph_repo::JarvisGraphRepository;
pub use section_repo::SectionRepository;
pub use vector_repo::{VectorRepository, StoredEmbedding, EmbeddingSourceType};
