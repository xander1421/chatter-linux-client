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
    /// Whether Jarvis panel is visible.
    panel_visible: bool,
    /// Whether Jarvis is currently thinking (inference in progress).
    is_thinking: bool,
    /// Conversation history for Jarvis panel.
    messages: Vec<JarvisMessage>,
    /// Current channel context for @Jarvis mentions.
    active_context_channel_id: Option<String>,
}

/// State store for Jarvis AI assistant.
///
/// Tracks panel visibility, thinking state, and conversation history.
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

    /// Check if the Jarvis panel is currently visible.
    #[must_use]
    pub fn is_panel_visible(&self) -> bool {
        self.inner.read().panel_visible
    }

    /// Toggle the Jarvis panel visibility.
    pub fn toggle_panel(&self) {
        let mut state = self.inner.write();
        state.panel_visible = !state.panel_visible;
    }

    /// Show the Jarvis panel.
    pub fn show_panel(&self) {
        self.inner.write().panel_visible = true;
    }

    /// Hide the Jarvis panel.
    pub fn hide_panel(&self) {
        self.inner.write().panel_visible = false;
    }

    /// Check if Jarvis is currently thinking (processing a query).
    #[must_use]
    pub fn is_thinking(&self) -> bool {
        self.inner.read().is_thinking
    }

    /// Set the thinking state.
    pub fn set_thinking(&self, thinking: bool) {
        self.inner.write().is_thinking = thinking;
    }

    /// Get all messages in the conversation history.
    #[must_use]
    pub fn get_messages(&self) -> Vec<JarvisMessage> {
        self.inner.read().messages.clone()
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

    /// Set the active channel context for @Jarvis mentions.
    pub fn set_active_context(&self, channel_id: Option<&str>) {
        self.inner.write().active_context_channel_id = channel_id.map(|s| s.to_string());
    }

    /// Get the active channel context.
    #[must_use]
    pub fn get_active_context(&self) -> Option<String> {
        self.inner.read().active_context_channel_id.clone()
    }

    /// Clear all messages from the conversation history.
    pub fn clear_messages(&self) {
        self.inner.write().messages.clear();
    }
}

impl Default for JarvisState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_panel_visibility() {
        let state = JarvisState::new();
        assert!(!state.is_panel_visible());

        state.show_panel();
        assert!(state.is_panel_visible());

        state.hide_panel();
        assert!(!state.is_panel_visible());

        state.toggle_panel();
        assert!(state.is_panel_visible());
    }

    #[test]
    fn test_thinking_state() {
        let state = JarvisState::new();
        assert!(!state.is_thinking());

        state.set_thinking(true);
        assert!(state.is_thinking());

        state.set_thinking(false);
        assert!(!state.is_thinking());
    }

    #[test]
    fn test_messages() {
        let state = JarvisState::new();
        assert!(state.get_messages().is_empty());

        let user_id = state.add_user_message("Hello", Some("channel-123"));
        assert!(!user_id.is_empty());

        let assistant_id = state.add_assistant_message("Hi there!");
        assert!(!assistant_id.is_empty());

        let messages = state.get_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, JarvisRole::User);
        assert_eq!(messages[0].content, "Hello");
        assert_eq!(messages[0].context_channel_id, Some("channel-123".to_string()));
        assert_eq!(messages[1].role, JarvisRole::Assistant);
        assert_eq!(messages[1].content, "Hi there!");

        state.clear_messages();
        assert!(state.get_messages().is_empty());
    }

    #[test]
    fn test_active_context() {
        let state = JarvisState::new();
        assert!(state.get_active_context().is_none());

        state.set_active_context(Some("channel-456"));
        assert_eq!(state.get_active_context(), Some("channel-456".to_string()));

        state.set_active_context(None);
        assert!(state.get_active_context().is_none());
    }
}
