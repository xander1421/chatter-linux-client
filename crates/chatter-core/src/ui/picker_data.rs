//! Picker data structures for Slint UI bindings.
//!
//! Provides Rust structs that map to Slint struct definitions for
//! GIF picker and mention autocomplete components.

use crate::domain::mention::MentionType;
use crate::media::gif::client::GifResult;

// ============================================================================
// GIF Data
// ============================================================================

/// GIF data for Slint model binding.
///
/// Maps to the `GifData` struct defined in `gif_picker.slint`.
#[derive(Clone, Debug, Default)]
pub struct GifData {
    /// Tenor media ID.
    pub id: String,
    /// Preview URL (tinygif for thumbnails).
    pub preview_url: String,
    /// Full GIF URL for sending.
    pub full_url: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl GifData {
    /// Create a new GIF data entry.
    #[must_use]
    pub fn new(
        id: String,
        preview_url: String,
        full_url: String,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            id,
            preview_url,
            full_url,
            width,
            height,
        }
    }
}

impl From<GifResult> for GifData {
    fn from(result: GifResult) -> Self {
        Self {
            id: result.id,
            preview_url: result.preview_url,
            full_url: result.url,
            width: result.width,
            height: result.height,
        }
    }
}

impl From<&GifResult> for GifData {
    fn from(result: &GifResult) -> Self {
        Self {
            id: result.id.clone(),
            preview_url: result.preview_url.clone(),
            full_url: result.url.clone(),
            width: result.width,
            height: result.height,
        }
    }
}

// ============================================================================
// Mention Suggestion
// ============================================================================

/// Mention suggestion for Slint model binding.
///
/// Maps to the `MentionSuggestion` struct defined in `mention_autocomplete.slint`.
#[derive(Clone, Debug, Default)]
pub struct MentionSuggestionData {
    /// Entity ID (user ID, channel ID, or "here"/"all" for special mentions).
    pub id: String,
    /// Display name (without @ or # prefix).
    pub name: String,
    /// Mention type as string: "user", "channel", "here", "all".
    pub mention_type: String,
}

impl MentionSuggestionData {
    /// Create a user mention suggestion.
    #[must_use]
    pub fn user(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            mention_type: "user".to_string(),
        }
    }

    /// Create a channel mention suggestion.
    #[must_use]
    pub fn channel(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            mention_type: "channel".to_string(),
        }
    }

    /// Create a @here mention suggestion.
    #[must_use]
    pub fn here() -> Self {
        Self {
            id: "here".to_string(),
            name: "here".to_string(),
            mention_type: "here".to_string(),
        }
    }

    /// Create an @all mention suggestion.
    #[must_use]
    pub fn all() -> Self {
        Self {
            id: "all".to_string(),
            name: "all".to_string(),
            mention_type: "all".to_string(),
        }
    }

    /// Get the full mention text (e.g., "@alice", "#general").
    #[must_use]
    pub fn mention_text(&self) -> String {
        match self.mention_type.as_str() {
            "channel" => format!("#{}", self.name),
            _ => format!("@{}", self.name),
        }
    }

    /// Get the mention type as enum.
    #[must_use]
    pub fn as_mention_type(&self) -> Option<MentionType> {
        match self.mention_type.as_str() {
            "user" => Some(MentionType::User),
            "channel" => Some(MentionType::Channel),
            "here" => Some(MentionType::Here),
            "all" => Some(MentionType::All),
            _ => None,
        }
    }
}

impl From<&crate::domain::mention::MentionSuggestion> for MentionSuggestionData {
    fn from(suggestion: &crate::domain::mention::MentionSuggestion) -> Self {
        let mention_type = match suggestion.suggestion_type {
            MentionType::User => "user",
            MentionType::Channel => "channel",
            MentionType::Here => "here",
            MentionType::All => "all",
        };

        Self {
            id: suggestion.id.clone(),
            name: suggestion.display_name.clone(),
            mention_type: mention_type.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gif_data_from_gif_result() {
        let result = GifResult {
            id: "123".to_string(),
            title: "Test GIF".to_string(),
            preview_url: "https://example.com/preview.gif".to_string(),
            url: "https://example.com/full.gif".to_string(),
            width: 200,
            height: 150,
        };

        let data: GifData = result.into();

        assert_eq!(data.id, "123");
        assert_eq!(data.preview_url, "https://example.com/preview.gif");
        assert_eq!(data.full_url, "https://example.com/full.gif");
        assert_eq!(data.width, 200);
        assert_eq!(data.height, 150);
    }

    #[test]
    fn test_mention_suggestion_user() {
        let suggestion = MentionSuggestionData::user("user123", "alice");

        assert_eq!(suggestion.id, "user123");
        assert_eq!(suggestion.name, "alice");
        assert_eq!(suggestion.mention_type, "user");
        assert_eq!(suggestion.mention_text(), "@alice");
        assert_eq!(suggestion.as_mention_type(), Some(MentionType::User));
    }

    #[test]
    fn test_mention_suggestion_channel() {
        let suggestion = MentionSuggestionData::channel("chan456", "general");

        assert_eq!(suggestion.id, "chan456");
        assert_eq!(suggestion.name, "general");
        assert_eq!(suggestion.mention_type, "channel");
        assert_eq!(suggestion.mention_text(), "#general");
        assert_eq!(suggestion.as_mention_type(), Some(MentionType::Channel));
    }

    #[test]
    fn test_mention_suggestion_here_all() {
        let here = MentionSuggestionData::here();
        assert_eq!(here.mention_text(), "@here");
        assert_eq!(here.as_mention_type(), Some(MentionType::Here));

        let all = MentionSuggestionData::all();
        assert_eq!(all.mention_text(), "@all");
        assert_eq!(all.as_mention_type(), Some(MentionType::All));
    }

}
