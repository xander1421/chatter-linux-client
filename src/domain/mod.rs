//! Domain layer - core business entities.
//!
//! Pure Rust structs with no external dependencies. These are the source of truth
//! for all business logic in the application.

pub mod ids;
pub mod user;
pub mod message;
pub mod channel;
pub mod attachment;
pub mod reaction;
pub mod call;
pub mod notification;
pub mod ai;
pub mod moderation;
pub mod saved_message;
pub mod draft;
pub mod ephemeral;
pub mod mention;
pub mod embed;
