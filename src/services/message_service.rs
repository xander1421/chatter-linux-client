//! Message service - re-export from modular implementation.
//!
//! This module is now organized into focused submodules:
//! - `message::dm` - DM operations (Signal Protocol encryption)
//! - `message::group` - Group/sender key operations
//! - `message::core` - Core message operations (send, edit, delete, reply, forward)
//! - `message::thread` - Thread operations
//! - `message::pin` - Pin operations
//! - `message::saved` - Saved messages (Note to Self)
//!
//! All public types and functions are re-exported from here for backward compatibility.

pub use crate::services::message::*;
