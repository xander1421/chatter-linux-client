//! Jarvis AI assistant UI features.
//!
//! Provides @Jarvis mention detection, slash command parsing, panel UI, and integration.

mod local_messages;
mod mention_handler;
mod panel;
mod slash_commands;

pub use local_messages::{
    add_jarvis_response_to_chat, add_jarvis_thinking_placeholder, replace_thinking_with_response,
};
pub use mention_handler::{contains_jarvis_mention, extract_jarvis_query, handle_jarvis_mention};
pub use panel::wire_jarvis_panel;
pub use slash_commands::{parse_slash_command, JarvisCommand};
