//! Handle @Jarvis mentions in messages.
//!
//! Detects @Jarvis mentions in message text and routes queries to the JarvisService.

use std::sync::Arc;

use chatter_linux::app::UIContext;
use chatter_linux::domain::ids::ChannelId;
use chatter_linux::domain::jarvis::JarvisContext;
use slint::Weak;

use crate::MainWindow;

/// Check if message contains @Jarvis mention.
///
/// Case-insensitive detection of @jarvis anywhere in the message.
pub fn contains_jarvis_mention(text: &str) -> bool {
    text.to_lowercase().contains("@jarvis")
}

/// Extract the query portion after @Jarvis.
///
/// Returns the trimmed text following @jarvis, or None if empty.
pub fn extract_jarvis_query(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    if let Some(pos) = lower.find("@jarvis") {
        // "@jarvis" is 7 characters
        let after = &text[pos + 7..];
        let query = after.trim();
        if !query.is_empty() {
            return Some(query.to_string());
        }
    }
    None
}

/// Handle a @Jarvis mention asynchronously.
///
/// Sets thinking state, adds thinking placeholder to chat, calls JarvisService,
/// and replaces placeholder with actual response.
pub async fn handle_jarvis_mention(
    ui_ctx: &Arc<UIContext>,
    query: &str,
    channel_id: Option<&ChannelId>,
    weak_window: Weak<MainWindow>,
) {
    // Set thinking state
    ui_ctx.jarvis_state.set_thinking(true);

    // Add thinking placeholder to chat and update header indicator
    let thinking_id = {
        let weak = weak_window.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_jarvis_thinking(true);
                let id = super::local_messages::add_jarvis_thinking_placeholder(&w);
                let _ = tx.send(id);
            }
        });
        rx.recv().ok()
    };

    // Build context - use channel context if available, otherwise use a DM-style context
    // with a nil UUID (Jarvis conversations are stored separately anyway)
    let context = match channel_id {
        Some(id) => JarvisContext::channel(id.clone()),
        None => {
            // Create a placeholder channel context for Jarvis panel conversations
            // Uses nil UUID - Jarvis stores conversations in its own tables anyway
            let placeholder_id: ChannelId = uuid::Uuid::nil().into();
            JarvisContext::dm(placeholder_id)
        }
    };

    // Get current user ID
    let author_id = ui_ctx.auth_state.get().current_user_id;

    // Add user message to Jarvis state
    let channel_id_str = channel_id.map(|id| id.to_string());
    ui_ctx
        .jarvis_state
        .add_user_message(query, channel_id_str.as_deref());

    // Call Jarvis
    match ui_ctx
        .jarvis_service
        .ask(&context, query, author_id.as_ref())
        .await
    {
        Ok(response) => {
            // Add assistant response to state
            ui_ctx.jarvis_state.add_assistant_message(&response);

            // Update UI - replace thinking placeholder with actual response
            let response_clone = response.clone();
            let query_clone = query.to_string();
            let thinking_id_clone = thinking_id.clone();
            let jarvis_state = ui_ctx.jarvis_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                jarvis_state.set_thinking(false);
                if let Some(w) = weak_window.upgrade() {
                    w.set_jarvis_thinking(false);
                    if let Some(tid) = thinking_id_clone {
                        super::local_messages::replace_thinking_with_response(
                            &w,
                            &tid,
                            &query_clone,
                            &response_clone,
                        );
                    }
                    tracing::info!("Jarvis response displayed: {} chars", response_clone.len());
                }
            });
        }
        Err(e) => {
            tracing::error!("Jarvis error: {}", e);
            let error_msg = format!("I encountered an error: {}", e);
            ui_ctx.jarvis_state.add_assistant_message(&error_msg);

            let query_clone = query.to_string();
            let thinking_id_clone = thinking_id.clone();
            let jarvis_state = ui_ctx.jarvis_state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                jarvis_state.set_thinking(false);
                if let Some(w) = weak_window.upgrade() {
                    w.set_jarvis_thinking(false);
                    if let Some(tid) = thinking_id_clone {
                        super::local_messages::replace_thinking_with_response(
                            &w,
                            &tid,
                            &query_clone,
                            &error_msg,
                        );
                    }
                    tracing::warn!("Jarvis error displayed: {}", error_msg);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_jarvis_mention() {
        assert!(contains_jarvis_mention("@Jarvis what time is it?"));
        assert!(contains_jarvis_mention("Hey @jarvis help me"));
        assert!(contains_jarvis_mention("@JARVIS"));
        assert!(!contains_jarvis_mention("Hello world"));
        assert!(!contains_jarvis_mention("jarvis"));
        assert!(!contains_jarvis_mention("@jar"));
    }

    #[test]
    fn test_extract_jarvis_query() {
        assert_eq!(
            extract_jarvis_query("@Jarvis what time is it?"),
            Some("what time is it?".to_string())
        );
        assert_eq!(
            extract_jarvis_query("Hey @jarvis help me"),
            Some("help me".to_string())
        );
        assert_eq!(
            extract_jarvis_query("  @JARVIS   summarize this  "),
            Some("summarize this".to_string())
        );
        assert_eq!(extract_jarvis_query("@Jarvis"), None);
        assert_eq!(extract_jarvis_query("@Jarvis   "), None);
        assert_eq!(extract_jarvis_query("Hello world"), None);
    }
}
