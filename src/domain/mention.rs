//! Mention domain entity.
//!
//! Represents @mentions in messages, including user mentions (@username),
//! channel mentions (#channel), and special mentions (@here, @all).

use crate::domain::ids::{ChannelId, UserId};
use serde::{Deserialize, Serialize};

// ============================================================================
// MentionType
// ============================================================================

/// Type of mention in a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MentionType {
    /// @user mention - mentions a specific user.
    User,
    /// #channel mention - references a channel.
    Channel,
    /// @here mention - notifies all online users in the channel.
    Here,
    /// @all or @everyone mention - notifies all members in the channel.
    All,
}

impl MentionType {
    /// Check if this mention type notifies multiple users.
    #[must_use]
    pub fn is_broadcast(&self) -> bool {
        matches!(self, Self::Here | Self::All)
    }

    /// Get the display prefix for this mention type.
    #[must_use]
    pub fn prefix(&self) -> &'static str {
        match self {
            Self::User | Self::Here | Self::All => "@",
            Self::Channel => "#",
        }
    }
}

// ============================================================================
// Mention
// ============================================================================

/// A mention within a message.
///
/// Tracks the position of a mention within message text, allowing for
/// rich rendering (highlighting, click handling) and notification routing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mention {
    /// The type of mention.
    pub mention_type: MentionType,
    /// Start position in the message text (byte offset).
    pub start_offset: usize,
    /// End position in the message text (byte offset).
    pub end_offset: usize,
    /// The display text (e.g., "@alice", "#general", "@here").
    pub display_text: String,
    /// Target ID for user/channel mentions (None for @here/@all).
    pub target_id: Option<String>,
}

impl Mention {
    /// Create a user mention.
    ///
    /// # Arguments
    /// * `user_id` - The mentioned user's ID
    /// * `display` - The display text (e.g., "@alice")
    /// * `start` - Start byte offset in message
    /// * `end` - End byte offset in message
    #[must_use]
    pub fn user(user_id: UserId, display: String, start: usize, end: usize) -> Self {
        Self {
            mention_type: MentionType::User,
            start_offset: start,
            end_offset: end,
            display_text: display,
            target_id: Some(user_id.to_string()),
        }
    }

    /// Create a channel mention.
    ///
    /// # Arguments
    /// * `channel_id` - The mentioned channel's ID
    /// * `display` - The display text (e.g., "#general")
    /// * `start` - Start byte offset in message
    /// * `end` - End byte offset in message
    #[must_use]
    pub fn channel(channel_id: ChannelId, display: String, start: usize, end: usize) -> Self {
        Self {
            mention_type: MentionType::Channel,
            start_offset: start,
            end_offset: end,
            display_text: display,
            target_id: Some(channel_id.to_string()),
        }
    }

    /// Create a @here mention (online users).
    ///
    /// # Arguments
    /// * `start` - Start byte offset in message
    /// * `end` - End byte offset in message
    #[must_use]
    pub fn here(start: usize, end: usize) -> Self {
        Self {
            mention_type: MentionType::Here,
            start_offset: start,
            end_offset: end,
            display_text: "@here".to_string(),
            target_id: None,
        }
    }

    /// Create an @all mention (all members).
    ///
    /// # Arguments
    /// * `start` - Start byte offset in message
    /// * `end` - End byte offset in message
    #[must_use]
    pub fn all(start: usize, end: usize) -> Self {
        Self {
            mention_type: MentionType::All,
            start_offset: start,
            end_offset: end,
            display_text: "@all".to_string(),
            target_id: None,
        }
    }

    /// Get the length of the mention in bytes.
    #[must_use]
    pub fn length(&self) -> usize {
        self.end_offset.saturating_sub(self.start_offset)
    }

    /// Check if this mention is a broadcast type (@here or @all).
    #[must_use]
    pub fn is_broadcast(&self) -> bool {
        self.mention_type.is_broadcast()
    }

    /// Check if this mention targets a specific entity.
    #[must_use]
    pub fn has_target(&self) -> bool {
        self.target_id.is_some()
    }
}

// ============================================================================
// MentionSuggestion
// ============================================================================

/// Autocomplete suggestion for mentions.
///
/// Used to populate the mention dropdown when users type @ or #.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MentionSuggestion {
    /// Type of suggestion.
    pub suggestion_type: MentionType,
    /// ID of the suggested entity.
    pub id: String,
    /// Display name shown in autocomplete.
    pub display_name: String,
    /// Avatar URL for visual context.
    pub avatar_url: Option<String>,
    /// Online status (for user suggestions).
    pub is_online: Option<bool>,
}

impl MentionSuggestion {
    /// Create a user suggestion.
    #[must_use]
    pub fn user(id: UserId, display_name: String) -> Self {
        Self {
            suggestion_type: MentionType::User,
            id: id.to_string(),
            display_name,
            avatar_url: None,
            is_online: None,
        }
    }

    /// Create a channel suggestion.
    #[must_use]
    pub fn channel(id: ChannelId, display_name: String) -> Self {
        Self {
            suggestion_type: MentionType::Channel,
            id: id.to_string(),
            display_name,
            avatar_url: None,
            is_online: None,
        }
    }

    /// Set the avatar URL.
    #[must_use]
    pub fn with_avatar(mut self, url: String) -> Self {
        self.avatar_url = Some(url);
        self
    }

    /// Set the online status.
    #[must_use]
    pub fn with_online_status(mut self, is_online: bool) -> Self {
        self.is_online = Some(is_online);
        self
    }

    /// Get the formatted mention text (e.g., "@alice", "#general").
    #[must_use]
    pub fn mention_text(&self) -> String {
        format!("{}{}", self.suggestion_type.prefix(), self.display_name)
    }
}

// ============================================================================
// MentionExtractor
// ============================================================================

/// Simple helper for extracting mentions from text.
///
/// Provides basic mention detection for @username, @here, @all, and #channel patterns.
pub struct MentionExtractor;

impl MentionExtractor {
    /// Extract mentions from text.
    ///
    /// Detects patterns:
    /// - `@username` - User mention (alphanumeric + underscore)
    /// - `@here` - Online users mention
    /// - `@all` or `@everyone` - All members mention
    /// - `#channel-name` - Channel mention (alphanumeric + hyphen + underscore)
    ///
    /// Note: This returns mentions with `target_id` as None since we don't
    /// have access to user/channel lookups here. The caller should resolve
    /// the actual IDs.
    #[must_use]
    pub fn extract_mentions(text: &str) -> Vec<Mention> {
        let mut mentions = Vec::new();
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            let byte_offset = text
                .char_indices()
                .nth(i)
                .map(|(idx, _)| idx)
                .unwrap_or(text.len());

            if chars[i] == '@' {
                if let Some((mention, end_idx)) = Self::parse_at_mention(&chars, i, byte_offset, text)
                {
                    mentions.push(mention);
                    i = end_idx;
                    continue;
                }
            } else if chars[i] == '#' {
                if let Some((mention, end_idx)) =
                    Self::parse_channel_mention(&chars, i, byte_offset, text)
                {
                    mentions.push(mention);
                    i = end_idx;
                    continue;
                }
            }
            i += 1;
        }

        mentions
    }

    fn parse_at_mention(
        chars: &[char],
        start_idx: usize,
        start_byte: usize,
        text: &str,
    ) -> Option<(Mention, usize)> {
        // Need at least one char after @
        if start_idx + 1 >= chars.len() {
            return None;
        }

        // Collect the identifier
        let mut end_idx = start_idx + 1;
        while end_idx < chars.len()
            && (chars[end_idx].is_alphanumeric() || chars[end_idx] == '_')
        {
            end_idx += 1;
        }

        if end_idx == start_idx + 1 {
            return None;
        }

        let identifier: String = chars[start_idx + 1..end_idx].iter().collect();
        let display: String = chars[start_idx..end_idx].iter().collect();
        let end_byte = text
            .char_indices()
            .nth(end_idx)
            .map(|(idx, _)| idx)
            .unwrap_or(text.len());

        // Check for special mentions
        let lower = identifier.to_lowercase();
        if lower == "here" {
            Some((Mention::here(start_byte, end_byte), end_idx))
        } else if lower == "all" || lower == "everyone" {
            Some((
                Mention {
                    mention_type: MentionType::All,
                    start_offset: start_byte,
                    end_offset: end_byte,
                    display_text: display,
                    target_id: None,
                },
                end_idx,
            ))
        } else {
            // User mention - target_id is None, caller must resolve
            Some((
                Mention {
                    mention_type: MentionType::User,
                    start_offset: start_byte,
                    end_offset: end_byte,
                    display_text: display,
                    target_id: None,
                },
                end_idx,
            ))
        }
    }

    fn parse_channel_mention(
        chars: &[char],
        start_idx: usize,
        start_byte: usize,
        text: &str,
    ) -> Option<(Mention, usize)> {
        // Need at least one char after #
        if start_idx + 1 >= chars.len() {
            return None;
        }

        // Collect the identifier (alphanumeric, hyphen, underscore)
        let mut end_idx = start_idx + 1;
        while end_idx < chars.len()
            && (chars[end_idx].is_alphanumeric()
                || chars[end_idx] == '-'
                || chars[end_idx] == '_')
        {
            end_idx += 1;
        }

        if end_idx == start_idx + 1 {
            return None;
        }

        let display: String = chars[start_idx..end_idx].iter().collect();
        let end_byte = text
            .char_indices()
            .nth(end_idx)
            .map(|(idx, _)| idx)
            .unwrap_or(text.len());

        // Channel mention - target_id is None, caller must resolve
        Some((
            Mention {
                mention_type: MentionType::Channel,
                start_offset: start_byte,
                end_offset: end_byte,
                display_text: display,
                target_id: None,
            },
            end_idx,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mention_type_is_broadcast() {
        assert!(!MentionType::User.is_broadcast());
        assert!(!MentionType::Channel.is_broadcast());
        assert!(MentionType::Here.is_broadcast());
        assert!(MentionType::All.is_broadcast());
    }

    #[test]
    fn test_mention_type_prefix() {
        assert_eq!(MentionType::User.prefix(), "@");
        assert_eq!(MentionType::Channel.prefix(), "#");
        assert_eq!(MentionType::Here.prefix(), "@");
        assert_eq!(MentionType::All.prefix(), "@");
    }

    #[test]
    fn test_mention_user() {
        let user_id = UserId::new();
        let mention = Mention::user(user_id, "@alice".to_string(), 0, 6);

        assert_eq!(mention.mention_type, MentionType::User);
        assert_eq!(mention.start_offset, 0);
        assert_eq!(mention.end_offset, 6);
        assert_eq!(mention.display_text, "@alice");
        assert_eq!(mention.target_id, Some(user_id.to_string()));
        assert_eq!(mention.length(), 6);
        assert!(mention.has_target());
        assert!(!mention.is_broadcast());
    }

    #[test]
    fn test_mention_channel() {
        let channel_id = ChannelId::new();
        let mention = Mention::channel(channel_id, "#general".to_string(), 10, 18);

        assert_eq!(mention.mention_type, MentionType::Channel);
        assert_eq!(mention.start_offset, 10);
        assert_eq!(mention.end_offset, 18);
        assert_eq!(mention.display_text, "#general");
        assert!(mention.has_target());
    }

    #[test]
    fn test_mention_here() {
        let mention = Mention::here(0, 5);

        assert_eq!(mention.mention_type, MentionType::Here);
        assert_eq!(mention.display_text, "@here");
        assert!(!mention.has_target());
        assert!(mention.is_broadcast());
    }

    #[test]
    fn test_mention_all() {
        let mention = Mention::all(0, 4);

        assert_eq!(mention.mention_type, MentionType::All);
        assert_eq!(mention.display_text, "@all");
        assert!(!mention.has_target());
        assert!(mention.is_broadcast());
    }

    #[test]
    fn test_mention_suggestion_user() {
        let user_id = UserId::new();
        let suggestion = MentionSuggestion::user(user_id, "alice".to_string())
            .with_avatar("https://example.com/avatar.png".to_string())
            .with_online_status(true);

        assert_eq!(suggestion.suggestion_type, MentionType::User);
        assert_eq!(suggestion.display_name, "alice");
        assert!(suggestion.avatar_url.is_some());
        assert_eq!(suggestion.is_online, Some(true));
        assert_eq!(suggestion.mention_text(), "@alice");
    }

    #[test]
    fn test_mention_suggestion_channel() {
        let channel_id = ChannelId::new();
        let suggestion = MentionSuggestion::channel(channel_id, "general".to_string());

        assert_eq!(suggestion.suggestion_type, MentionType::Channel);
        assert_eq!(suggestion.mention_text(), "#general");
    }

    #[test]
    fn test_extract_mentions_user() {
        let text = "Hello @alice!";
        let mentions = MentionExtractor::extract_mentions(text);

        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].mention_type, MentionType::User);
        assert_eq!(mentions[0].display_text, "@alice");
        assert_eq!(mentions[0].start_offset, 6);
        assert_eq!(mentions[0].end_offset, 12);
    }

    #[test]
    fn test_extract_mentions_here_all() {
        let text = "@here please check this @all";
        let mentions = MentionExtractor::extract_mentions(text);

        assert_eq!(mentions.len(), 2);
        assert_eq!(mentions[0].mention_type, MentionType::Here);
        assert_eq!(mentions[1].mention_type, MentionType::All);
    }

    #[test]
    fn test_extract_mentions_everyone() {
        let text = "Hey @everyone!";
        let mentions = MentionExtractor::extract_mentions(text);

        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].mention_type, MentionType::All);
        assert_eq!(mentions[0].display_text, "@everyone");
    }

    #[test]
    fn test_extract_mentions_channel() {
        let text = "Check out #general and #dev-team";
        let mentions = MentionExtractor::extract_mentions(text);

        assert_eq!(mentions.len(), 2);
        assert_eq!(mentions[0].mention_type, MentionType::Channel);
        assert_eq!(mentions[0].display_text, "#general");
        assert_eq!(mentions[1].mention_type, MentionType::Channel);
        assert_eq!(mentions[1].display_text, "#dev-team");
    }

    #[test]
    fn test_extract_mentions_mixed() {
        let text = "@alice check #general @here";
        let mentions = MentionExtractor::extract_mentions(text);

        assert_eq!(mentions.len(), 3);
        assert_eq!(mentions[0].mention_type, MentionType::User);
        assert_eq!(mentions[1].mention_type, MentionType::Channel);
        assert_eq!(mentions[2].mention_type, MentionType::Here);
    }

    #[test]
    fn test_extract_mentions_empty() {
        let text = "No mentions here!";
        let mentions = MentionExtractor::extract_mentions(text);

        assert!(mentions.is_empty());
    }

    #[test]
    fn test_extract_mentions_invalid() {
        // @ or # at end with no identifier
        let text = "trailing @ and #";
        let mentions = MentionExtractor::extract_mentions(text);

        assert!(mentions.is_empty());
    }

    #[test]
    fn test_extract_mentions_unicode() {
        // Unicode text with ASCII mentions
        let text = "Hello 世界 @alice!";
        let mentions = MentionExtractor::extract_mentions(text);

        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].display_text, "@alice");
        // Verify byte offsets are correct for the actual text
        assert_eq!(&text[mentions[0].start_offset..mentions[0].end_offset], "@alice");
    }
}
