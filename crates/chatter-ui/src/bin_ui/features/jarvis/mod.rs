//! Jarvis AI assistant UI features.
//!
//! Provides @Jarvis mention detection, slash command parsing, panel UI, and integration.

mod local_messages;
pub mod mention_handler;
pub mod panel;

pub use chatter_core::services::jarvis::{JarvisCommand, parse_slash_command};
pub use local_messages::{add_jarvis_thinking_placeholder, replace_thinking_with_response};
pub use mention_handler::{contains_jarvis_mention, extract_jarvis_query, handle_jarvis_mention};
pub use panel::wire_jarvis_panel;
