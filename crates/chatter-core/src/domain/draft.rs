//! Draft domain entity.
//!
//! Represents message drafts with per-channel/thread persistence.
//! Drafts enable users to save work-in-progress messages that persist
//! across sessions and navigation.

use crate::domain::ids::{AttachmentId, ChannelId, DraftId, MessageId, ThreadId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

// ============================================================================
// DraftKey
// ============================================================================

/// Key for identifying drafts by channel and optional thread.
///
/// Used as a lookup key to find the draft for a specific context.
/// The key uniquely identifies where a draft belongs (channel root or thread).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftKey {
    /// The channel this draft belongs to.
    pub channel_id: ChannelId,
    /// The thread this draft belongs to (None for main channel).
    pub thread_id: Option<ThreadId>,
}

impl DraftKey {
    /// Create a draft key for a channel (main conversation).
    #[must_use]
    pub fn for_channel(channel_id: ChannelId) -> Self {
        Self {
            channel_id,
            thread_id: None,
        }
    }

    /// Create a draft key for a thread within a channel.
    #[must_use]
    pub fn for_thread(channel_id: ChannelId, thread_id: ThreadId) -> Self {
        Self {
            channel_id,
            thread_id: Some(thread_id),
        }
    }

    /// Check if this key is for a thread.
    #[must_use]
    pub fn is_thread(&self) -> bool {
        self.thread_id.is_some()
    }
}

impl Hash for DraftKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.channel_id.hash(state);
        self.thread_id.hash(state);
    }
}

// ============================================================================
// Draft
// ============================================================================

/// A message draft with auto-save context.
///
/// Drafts persist user input for messages that haven't been sent yet.
/// They support:
/// - Per-channel and per-thread drafts
/// - Reply-to context preservation
/// - Pending attachment tracking
/// - Mention detection
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    /// Unique identifier for this draft.
    pub id: DraftId,
    /// Location key (channel + optional thread).
    pub key: DraftKey,
    /// The draft text content.
    pub content: String,
    /// Message being replied to (if any).
    pub reply_to: Option<MessageId>,
    /// Pending attachments not yet uploaded.
    pub attachments: Vec<AttachmentId>,
    /// Detected @mentions in the content.
    pub mentions: Vec<UserId>,
    /// When the draft was first created.
    pub created_at: DateTime<Utc>,
    /// When the draft was last modified.
    pub updated_at: DateTime<Utc>,
}

impl Draft {
    /// Create a new draft for the given key and content.
    #[must_use]
    pub fn new(key: DraftKey, content: String) -> Self {
        let now = Utc::now();
        Self {
            id: DraftId::new(),
            key,
            content,
            reply_to: None,
            attachments: Vec::new(),
            mentions: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Check if the draft is empty (no content and no attachments).
    ///
    /// Empty drafts can typically be discarded without saving.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty() && self.attachments.is_empty()
    }

    /// Update the draft content and refresh the updated_at timestamp.
    pub fn update_content(&mut self, content: String) {
        self.content = content;
        self.updated_at = Utc::now();
    }

    /// Set or clear the reply-to message reference.
    pub fn set_reply(&mut self, message_id: Option<MessageId>) {
        self.reply_to = message_id;
        self.updated_at = Utc::now();
    }

    /// Add an attachment to the draft.
    pub fn add_attachment(&mut self, attachment_id: AttachmentId) {
        self.attachments.push(attachment_id);
        self.updated_at = Utc::now();
    }

    /// Remove an attachment from the draft.
    ///
    /// Returns true if the attachment was found and removed.
    pub fn remove_attachment(&mut self, attachment_id: &AttachmentId) -> bool {
        let initial_len = self.attachments.len();
        self.attachments.retain(|id| id != attachment_id);
        let removed = self.attachments.len() < initial_len;
        if removed {
            self.updated_at = Utc::now();
        }
        removed
    }

    /// Count the number of words in the draft content.
    ///
    /// Uses Unicode-aware word boundaries.
    #[must_use]
    pub fn word_count(&self) -> usize {
        self.content.split_whitespace().count()
    }

    /// Check if the draft has any attachments.
    #[must_use]
    pub fn has_attachments(&self) -> bool {
        !self.attachments.is_empty()
    }

    /// Check if the draft is a reply to another message.
    #[must_use]
    pub fn is_reply(&self) -> bool {
        self.reply_to.is_some()
    }

    /// Add a mention to the draft.
    pub fn add_mention(&mut self, user_id: UserId) {
        if !self.mentions.contains(&user_id) {
            self.mentions.push(user_id);
            self.updated_at = Utc::now();
        }
    }

    /// Clear all mentions from the draft.
    pub fn clear_mentions(&mut self) {
        if !self.mentions.is_empty() {
            self.mentions.clear();
            self.updated_at = Utc::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_draft_key_for_channel() {
        let channel_id = ChannelId::new();
        let key = DraftKey::for_channel(channel_id);

        assert_eq!(key.channel_id, channel_id);
        assert!(key.thread_id.is_none());
        assert!(!key.is_thread());
    }

    #[test]
    fn test_draft_key_for_thread() {
        let channel_id = ChannelId::new();
        let thread_id = MessageId::new();
        let key = DraftKey::for_thread(channel_id, thread_id);

        assert_eq!(key.channel_id, channel_id);
        assert_eq!(key.thread_id, Some(thread_id));
        assert!(key.is_thread());
    }

    #[test]
    fn test_draft_key_hash() {
        use std::collections::HashMap;

        let channel_id = ChannelId::new();
        let key1 = DraftKey::for_channel(channel_id);
        let key2 = DraftKey::for_channel(channel_id);

        let mut map = HashMap::new();
        map.insert(key1.clone(), "draft1");
        assert_eq!(map.get(&key2), Some(&"draft1"));
    }

    #[test]
    fn test_draft_new() {
        let channel_id = ChannelId::new();
        let key = DraftKey::for_channel(channel_id);
        let draft = Draft::new(key.clone(), "Hello, world!".to_string());

        assert_eq!(draft.key, key);
        assert_eq!(draft.content, "Hello, world!");
        assert!(draft.reply_to.is_none());
        assert!(draft.attachments.is_empty());
        assert!(draft.mentions.is_empty());
    }

    #[test]
    fn test_draft_is_empty() {
        let key = DraftKey::for_channel(ChannelId::new());

        // Empty content, no attachments
        let draft = Draft::new(key.clone(), "".to_string());
        assert!(draft.is_empty());

        // Whitespace only
        let draft2 = Draft::new(key.clone(), "   \n\t  ".to_string());
        assert!(draft2.is_empty());

        // Has content
        let draft3 = Draft::new(key.clone(), "Hello".to_string());
        assert!(!draft3.is_empty());

        // Empty content but has attachment
        let mut draft4 = Draft::new(key, "".to_string());
        draft4.add_attachment(AttachmentId::new());
        assert!(!draft4.is_empty());
    }

    #[test]
    fn test_draft_update_content() {
        let key = DraftKey::for_channel(ChannelId::new());
        let mut draft = Draft::new(key, "Initial".to_string());
        let original_updated = draft.updated_at;

        // Small delay to ensure timestamp changes
        std::thread::sleep(std::time::Duration::from_millis(1));

        draft.update_content("Updated".to_string());
        assert_eq!(draft.content, "Updated");
        assert!(draft.updated_at >= original_updated);
    }

    #[test]
    fn test_draft_set_reply() {
        let key = DraftKey::for_channel(ChannelId::new());
        let mut draft = Draft::new(key, "Reply text".to_string());

        assert!(!draft.is_reply());

        let message_id = MessageId::new();
        draft.set_reply(Some(message_id));
        assert!(draft.is_reply());
        assert_eq!(draft.reply_to, Some(message_id));

        draft.set_reply(None);
        assert!(!draft.is_reply());
    }

    #[test]
    fn test_draft_attachments() {
        let key = DraftKey::for_channel(ChannelId::new());
        let mut draft = Draft::new(key, "".to_string());

        assert!(!draft.has_attachments());

        let att1 = AttachmentId::new();
        let att2 = AttachmentId::new();

        draft.add_attachment(att1);
        draft.add_attachment(att2);
        assert!(draft.has_attachments());
        assert_eq!(draft.attachments.len(), 2);

        // Remove first attachment
        let removed = draft.remove_attachment(&att1);
        assert!(removed);
        assert_eq!(draft.attachments.len(), 1);

        // Try to remove non-existent
        let not_removed = draft.remove_attachment(&att1);
        assert!(!not_removed);
    }

    #[test]
    fn test_draft_word_count() {
        let key = DraftKey::for_channel(ChannelId::new());

        let draft1 = Draft::new(key.clone(), "".to_string());
        assert_eq!(draft1.word_count(), 0);

        let draft2 = Draft::new(key.clone(), "Hello world!".to_string());
        assert_eq!(draft2.word_count(), 2);

        let draft3 = Draft::new(key.clone(), "  One   two   three  ".to_string());
        assert_eq!(draft3.word_count(), 3);

        let draft4 = Draft::new(key, "Multi\nline\ntext".to_string());
        assert_eq!(draft4.word_count(), 3);
    }

    #[test]
    fn test_draft_mentions() {
        let key = DraftKey::for_channel(ChannelId::new());
        let mut draft = Draft::new(key, "Hey @user1 and @user2".to_string());

        let user1 = UserId::new();
        let user2 = UserId::new();

        draft.add_mention(user1);
        draft.add_mention(user2);
        assert_eq!(draft.mentions.len(), 2);

        // Adding same user again should not duplicate
        draft.add_mention(user1);
        assert_eq!(draft.mentions.len(), 2);

        draft.clear_mentions();
        assert!(draft.mentions.is_empty());
    }
}
