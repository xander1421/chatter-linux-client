//! Slint UI layer.
//!
//! Rust to Slint UI bridge and adapters.

pub mod adapters;
pub mod bridge;
pub mod commands;
pub mod handlers;
pub mod widgets;

// Re-export bridge types for convenience
pub use bridge::{MainWindow, UiBridge};
