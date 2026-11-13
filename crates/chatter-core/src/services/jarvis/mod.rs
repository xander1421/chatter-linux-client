//! Jarvis AI assistant service.
//!
//! Provides a persistent AI assistant with:
//! - **Memory**: the owner's remote conversation history
//! - **Knowledge Graph**: browsing previously stored entities and facts

pub mod commands;
pub mod context_builder;
pub mod engine;
pub mod error;
pub mod memory_manager;
pub mod service;

pub use commands::{JarvisCommand, parse_slash_command};
pub use engine::{JarvisEngine, SharedJarvisEngine};
pub use error::JarvisError;
pub use memory_manager::MemoryStats;
pub use service::JarvisService;
