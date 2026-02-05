//! Application orchestration.
//!
//! Top-level application initialization and management.

pub mod bootstrap;
pub mod config;
pub mod context;
pub mod shutdown;

pub use context::{AppContext, ContextError};
