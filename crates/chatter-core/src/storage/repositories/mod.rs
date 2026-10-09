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
pub mod ephemeral_repo;
pub(crate) mod delete_batch_retry_repo;
// message_repo.rs deleted — all callers now use UnifiedMessageRepository via `MessageRepository` alias.
pub mod anon_dm_repo;
pub mod anon_group_repo;
#[cfg(feature = "jarvis")]
pub mod jarvis_graph_repo;
#[cfg(feature = "jarvis")]
pub mod jarvis_memory_repo;
pub mod mesh_conversation_repo;
pub mod mesh_delivery_repo;
pub mod message_modifier_repo;
pub mod message_receipt_repo;
pub mod pending_dm_repo;
pub mod saved_sync_repo;
pub(crate) mod repo_utils;
pub mod search_index;
pub mod section_repo;
pub mod send_log_repo;
pub mod settings_repo;
pub mod shared_content_repo;
pub mod unified_conversation_repo;
pub mod unified_message_repo;
pub mod user_repo;
pub mod vector_repo;
pub mod verified_membership_repo;

pub use anon_dm_repo::AnonDmRepository;
pub use anon_group_repo::AnonGroupRepository;
pub use attachment_repo::AttachmentRepository;
pub use call_repo::CallHistoryRepository;
pub use channel_repo::ChannelRepository;
pub use ephemeral_repo::EphemeralRepository;
#[cfg(feature = "jarvis")]
pub use jarvis_graph_repo::JarvisGraphRepository;
#[cfg(feature = "jarvis")]
pub use jarvis_memory_repo::JarvisMemoryRepository;
pub use mesh_conversation_repo::{MeshConversationRepository, StoredMeshConversation};
pub use mesh_delivery_repo::MeshDeliveryRepository;
pub use pending_dm_repo::{PendingDm, PendingDmRepository};
pub use saved_sync_repo::{PinWrite, SavedSyncRepository};
pub use search_index::{SearchIndex, SearchResult};
pub use section_repo::SectionRepository;
pub use settings_repo::{SettingsRepository, keys as settings_keys};
pub use shared_content_repo::{SharedContentRepository, SharedContentRow};
pub use unified_conversation_repo::UnifiedConversationRepository;
/// Re-export `UnifiedMessageRepository` as `MessageRepository` — all code now uses the
/// unified implementation. The old `message_repo` module is kept for step 5 deletion.
pub use unified_message_repo::UnifiedMessageRepository as MessageRepository;
pub use unified_message_repo::UnifiedMessageRepository;
pub use user_repo::UserRepository;
pub use vector_repo::{EmbeddingSourceType, StoredEmbedding, VectorRepository};
