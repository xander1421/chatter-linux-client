//! Privacy state.
//!
//! Per-conversation privacy settings with global defaults.

use std::collections::HashMap;


use crate::domain::ids::ChannelId;
use crate::state::store::Store;

/// Per-conversation privacy settings.
///
/// Controls which signals are sent in a specific conversation.
/// These override global settings when explicitly set.
#[derive(Clone, Debug, PartialEq)]
pub struct ConversationPrivacySettings {
    /// Whether to send read receipts in this conversation.
    pub read_receipts_enabled: bool,
    /// Whether to send typing indicators in this conversation.
    pub typing_indicators_enabled: bool,
    /// Whether screenshots are allowed (for ephemeral message contexts).
    pub screenshots_allowed: bool,
}

impl Default for ConversationPrivacySettings {
    fn default() -> Self {
        Self {
            read_receipts_enabled: true,
            typing_indicators_enabled: true,
            screenshots_allowed: true,
        }
    }
}

impl ConversationPrivacySettings {

}

/// Privacy state data.
///
/// Tracks per-conversation privacy settings with global defaults.
/// This provides the UI with reactive access to privacy controls.
#[derive(Clone, Debug)]
pub struct PrivacyStateData {
    /// Per-conversation privacy settings (overrides).
    pub conversation_settings: HashMap<ChannelId, ConversationPrivacySettings>,
    /// Global default for read receipts.
    pub global_read_receipts: bool,
    /// Global default for typing indicators.
    pub global_typing_indicators: bool,
}

impl Default for PrivacyStateData {
    fn default() -> Self {
        Self {
            conversation_settings: HashMap::new(),
            global_read_receipts: true,
            global_typing_indicators: true,
        }
    }
}

/// Reactive privacy state manager.
///
/// Provides a reactive wrapper around `PrivacyStateData` that allows
/// UI components to subscribe to privacy setting changes.
///
/// # Per-conversation vs Global Settings
///
/// - Global settings apply to all conversations by default
/// - Per-conversation settings override globals when explicitly set
/// - Removing per-conversation settings falls back to global defaults
///
/// # Example
///
/// ```ignore
/// let privacy_state = PrivacyState::new();
///
/// // UI subscribes to privacy changes
/// let mut rx = privacy_state.subscribe();
///
/// // Check effective settings for a conversation
/// let enabled = privacy_state.are_read_receipts_enabled(&channel_id);
///
/// // Override for specific conversation
/// privacy_state.set_settings(channel_id, ConversationPrivacySettings::private());
///
/// // UI receives update
/// let state = rx.recv().await.unwrap();
/// ```
#[derive(Clone)]
pub struct PrivacyState {
    store: Store<PrivacyStateData>,
}

impl PrivacyState {
    /// Create a new privacy state with default values.
    ///
    /// Global read receipts and typing indicators are enabled by default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(PrivacyStateData::default()),
        }
    }

}

impl Default for PrivacyState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conversation_privacy_settings_default() {
        let settings = ConversationPrivacySettings::default();

        assert!(settings.read_receipts_enabled);
        assert!(settings.typing_indicators_enabled);
        assert!(settings.screenshots_allowed);
    }

}
