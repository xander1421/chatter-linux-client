//! Jarvis AI assistant state.
//!
//! UI state for Jarvis interactions - panel visibility, thinking state, conversation history.

use std::sync::Arc;
use parking_lot::RwLock;
use uuid::Uuid;

/// Jarvis conversation entry.
#[derive(Debug, Clone)]
pub struct JarvisMessage {
    /// Unique identifier for this message.
    pub id: String,
    /// Role of the message author (user or assistant).
    pub role: JarvisRole,
    /// Message content.
    pub content: String,
    /// When the message was created.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Channel context where this message was triggered (for @Jarvis mentions).
    pub context_channel_id: Option<String>,
}

/// Role of a Jarvis message author.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JarvisRole {
    /// User asking a question or making a request.
    User,
    /// Jarvis AI assistant responding.
    Assistant,
}

/// Internal state data for Jarvis.
#[derive(Debug, Clone, Default)]
struct JarvisStateData {
    /// Whether Jarvis is currently thinking (inference in progress).
    is_thinking: bool,
    /// Conversation history for Jarvis panel.
    messages: Vec<JarvisMessage>,
}

/// State store for Jarvis AI assistant.
///
/// Tracks thinking state and conversation history.
/// Thread-safe via `Arc<RwLock<_>>`.
#[derive(Clone)]
pub struct JarvisState {
    inner: Arc<RwLock<JarvisStateData>>,
}

impl JarvisState {
    /// Create a new Jarvis state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(JarvisStateData::default())),
        }
    }

    /// Set the thinking state.
    pub fn set_thinking(&self, thinking: bool) {
        self.inner.write().is_thinking = thinking;
    }

    /// Add a user message to the conversation.
    ///
    /// Returns the generated message ID.
    pub fn add_user_message(&self, content: &str, channel_id: Option<&str>) -> String {
        let id = Uuid::new_v4().to_string();
        let mut state = self.inner.write();
        state.messages.push(JarvisMessage {
            id: id.clone(),
            role: JarvisRole::User,
            content: content.to_string(),
            timestamp: chrono::Utc::now(),
            context_channel_id: channel_id.map(|s| s.to_string()),
        });
        id
    }

    /// Add an assistant message to the conversation.
    ///
    /// Returns the generated message ID.
    pub fn add_assistant_message(&self, content: &str) -> String {
        let id = Uuid::new_v4().to_string();
        let mut state = self.inner.write();
        state.messages.push(JarvisMessage {
            id: id.clone(),
            role: JarvisRole::Assistant,
            content: content.to_string(),
            timestamp: chrono::Utc::now(),
            context_channel_id: None,
        });
        id
    }
}

impl Default for JarvisState {
    fn default() -> Self {
        Self::new()
    }
}

