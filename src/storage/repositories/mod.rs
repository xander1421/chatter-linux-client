//! Data access repositories.
//!
//! Repository pattern implementations for data access.

pub mod attachment_repo;
pub mod call_repo;
pub mod channel_repo;
pub mod draft_repo;
pub mod message_repo;
pub mod reaction_repo;
pub mod search_index;
pub mod settings_repo;
pub mod user_repo;

pub use user_repo::UserRepository;
