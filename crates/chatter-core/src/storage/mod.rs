//! Persistent storage layer.
//!
//! SQLCipher-backed persistence with proper migrations.

pub mod backup_data;
pub mod database;
pub mod key_store;
pub(crate) mod local_wipe;
#[cfg(feature = "mesh")]
pub mod mesh_binding_store;
#[cfg(feature = "mesh")]
pub mod mesh_identity_store;
#[cfg(feature = "mesh")]
pub mod mesh_store;
pub mod migrations;
pub mod receive_artifact;
pub mod repositories;
pub mod serialization;
pub mod unlink;
pub(crate) mod peer_verification;

pub use database::Database;
pub use key_store::KeyStore;
#[cfg(feature = "mesh")]
pub use mesh_identity_store::MeshIdentityStore;
pub use repositories::AttachmentRepository;
pub use repositories::ChannelRepository;
pub use repositories::MessageRepository;
pub use repositories::SearchIndex;
pub use repositories::SettingsRepository;
pub use repositories::UserRepository;
pub use repositories::settings_keys;
