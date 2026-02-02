//! Mention domain entity.
//!
//! Represents @mentions in messages.

/// Mention in a message.
pub struct Mention;

/// Mention type variants.
pub enum MentionType {
    /// @user mention.
    User,
    /// @channel mention.
    Channel,
    /// @here mention (online users).
    Here,
    /// @all mention (everyone).
    All,
}

/// Mention autocomplete suggestion.
pub struct MentionSuggestion;
