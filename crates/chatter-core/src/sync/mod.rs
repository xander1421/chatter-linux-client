//! Synchronization engine.
//!
//! Orchestrates all sync operations across devices:
//! - Channel sync: sync channel list and membership
//! - Message sync: fetch missed messages per channel
//! - User sync: sync friends list and profiles
//! - Key sync: encryption key health (delegated to SignalService)
//! - Conflict resolution: simple server-wins strategy

pub mod sync_engine;
pub mod message_sync;
pub mod channel_sync;
pub mod user_sync;
pub mod conflict;
