//! Reaction domain entity.
//!
//! Represents emoji reactions on messages.

use crate::domain::ids::{MessageId, ReactionId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ============================================================================
// Reaction
// ============================================================================

/// Represents a reaction on a message.
///
/// Reactions allow users to respond to messages with emoji. Each reaction
/// is associated with a specific message and user.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reaction {
    pub id: ReactionId,
    pub message_id: MessageId,
    pub user_id: UserId,
    pub emoji: Emoji,
    pub created_at: DateTime<Utc>,
}

impl Reaction {
    /// Create a new reaction with a generated ID and current timestamp.
    #[must_use]
    pub fn new(message_id: MessageId, user_id: UserId, emoji: Emoji) -> Self {
        Self {
            id: ReactionId::new(),
            message_id,
            user_id,
            emoji,
            created_at: Utc::now(),
        }
    }

    /// Check if this reaction was created by the given user.
    #[must_use]
    pub fn is_from_user(&self, user_id: &UserId) -> bool {
        self.user_id == *user_id
    }
}

// ============================================================================
// Emoji
// ============================================================================

/// Emoji data for reactions.
///
/// Supports both standard Unicode emoji and custom server emoji.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Emoji {
    /// The emoji value (unicode character or custom code).
    pub value: String,
    /// Whether this is a custom emoji.
    pub is_custom: bool,
    /// Name for custom emoji (e.g., "party_parrot").
    pub custom_name: Option<String>,
    /// URL for custom emoji image.
    pub custom_url: Option<String>,
}

impl Emoji {
    /// Create a standard Unicode emoji.
    #[must_use]
    pub fn unicode(s: &str) -> Self {
        Self {
            value: s.to_string(),
            is_custom: false,
            custom_name: None,
            custom_url: None,
        }
    }

    /// Create a custom emoji with name and URL.
    #[must_use]
    pub fn custom(name: String, url: String) -> Self {
        Self {
            value: format!(":{name}:"),
            is_custom: true,
            custom_name: Some(name),
            custom_url: Some(url),
        }
    }

    /// Check if this is a standard Unicode emoji (not custom).
    #[must_use]
    pub fn is_unicode(&self) -> bool {
        !self.is_custom
    }
}

// ============================================================================
// Emoji Category
// ============================================================================

/// Emoji categories for picker organization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EmojiCategory {
    /// Smileys and emotion.
    Smileys,
    /// People and body.
    People,
    /// Animals and nature.
    Animals,
    /// Food and drink.
    Food,
    /// Travel and places.
    Travel,
    /// Activities.
    Activities,
    /// Objects.
    Objects,
    /// Symbols.
    Symbols,
    /// Flags.
    Flags,
    /// Custom server emoji.
    Custom,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reaction_new() {
        let message_id = MessageId::new();
        let user_id = UserId::new();
        let emoji = Emoji::unicode("👍");

        let reaction = Reaction::new(message_id, user_id, emoji.clone());

        assert_eq!(reaction.message_id, message_id);
        assert_eq!(reaction.user_id, user_id);
        assert_eq!(reaction.emoji, emoji);
    }

    #[test]
    fn test_reaction_is_from_user() {
        let message_id = MessageId::new();
        let user_id = UserId::new();
        let other_user_id = UserId::new();
        let emoji = Emoji::unicode("❤️");

        let reaction = Reaction::new(message_id, user_id, emoji);

        assert!(reaction.is_from_user(&user_id));
        assert!(!reaction.is_from_user(&other_user_id));
    }

    #[test]
    fn test_emoji_unicode() {
        let emoji = Emoji::unicode("🎉");

        assert_eq!(emoji.value, "🎉");
        assert!(!emoji.is_custom);
        assert!(emoji.is_unicode());
        assert!(emoji.custom_name.is_none());
        assert!(emoji.custom_url.is_none());
    }

    #[test]
    fn test_emoji_custom() {
        let emoji = Emoji::custom(
            "party_parrot".to_string(),
            "https://example.com/party_parrot.gif".to_string(),
        );

        assert_eq!(emoji.value, ":party_parrot:");
        assert!(emoji.is_custom);
        assert!(!emoji.is_unicode());
        assert_eq!(emoji.custom_name, Some("party_parrot".to_string()));
        assert_eq!(
            emoji.custom_url,
            Some("https://example.com/party_parrot.gif".to_string())
        );
    }

    #[test]
    fn test_emoji_category_variants() {
        // Ensure all variants exist and are distinct
        let categories = [
            EmojiCategory::Smileys,
            EmojiCategory::People,
            EmojiCategory::Animals,
            EmojiCategory::Food,
            EmojiCategory::Travel,
            EmojiCategory::Activities,
            EmojiCategory::Objects,
            EmojiCategory::Symbols,
            EmojiCategory::Flags,
            EmojiCategory::Custom,
        ];

        assert_eq!(categories.len(), 10);
        assert_ne!(EmojiCategory::Smileys, EmojiCategory::Custom);
    }
}
