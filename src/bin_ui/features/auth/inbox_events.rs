//! Inbox event handling - re-export from modular implementation.
//!
//! This file provides backwards compatibility by re-exporting from the `inbox` module.
//! The actual implementation is split across:
//! - `inbox/mod.rs` - Main dispatcher
//! - `inbox/typing.rs` - Typing timer management
//! - `inbox/utils.rs` - Shared utilities
//! - `inbox/dm_handlers.rs` - DM event handlers
//! - `inbox/channel_handlers.rs` - Channel event handlers

