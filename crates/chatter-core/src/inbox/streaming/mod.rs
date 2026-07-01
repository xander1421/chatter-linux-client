//! Real-time streaming handlers.
//!
//! This module contains stream-based handlers for real-time event subscriptions.
//! Unlike polling, streams provide immediate notification when events occur.
//!
//! # Streams
//!
//! - `user_events` - Friend requests, profile updates, presence changes
//! - `dm_bootstrap_handler` - DM bootstrap key exchange processing

pub mod dm_bootstrap_handler;
pub mod user_events;

pub use dm_bootstrap_handler::spawn_dm_bootstrap_handler_from_bus;
pub use user_events::spawn_user_events_stream;
