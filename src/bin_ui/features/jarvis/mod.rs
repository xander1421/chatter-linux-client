//! Jarvis AI assistant UI features.
//!
//! Provides @Jarvis mention detection, slash command parsing, and UI integration.

mod mention_handler;
mod slash_commands;

pub use mention_handler::{contains_jarvis_mention, extract_jarvis_query, handle_jarvis_mention};
pub use slash_commands::{parse_slash_command, JarvisCommand};
