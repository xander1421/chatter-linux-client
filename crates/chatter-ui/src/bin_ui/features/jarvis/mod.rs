//! Jarvis AI assistant UI features.
//!
//! The AI channel (a local virtual channel) is the single Jarvis surface;
//! @Jarvis mentions and slash commands in real channels are intercepted
//! pre-send and answered locally.

pub mod channel_view;
mod local_messages;
pub mod mention_handler;

pub use chatter_core::services::jarvis::{JarvisCommand, parse_slash_command};
pub use channel_view::{handle_jarvis_channel_send, is_jarvis_channel_str, load_jarvis_channel_into_ui};
pub use local_messages::{add_jarvis_thinking_placeholder, replace_thinking_with_response};
pub use mention_handler::{contains_jarvis_mention, extract_jarvis_query, handle_jarvis_mention};
