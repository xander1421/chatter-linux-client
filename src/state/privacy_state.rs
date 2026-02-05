//! Privacy state.
//!
//! Per-conversation privacy settings with global defaults.

use std::collections::HashMap;

use tokio::sync::broadcast;

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
    /// Create new settings with all features enabled.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create new settings with all features disabled.
    #[must_use]
    pub fn private() -> Self {
        Self {
            read_receipts_enabled: false,
            typing_indicators_enabled: false,
            screenshots_allowed: false,
        }
    }
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

    /// Get the current privacy state data.
    #[must_use]
    pub fn get(&self) -> PrivacyStateData {
        self.store.get()
    }

    /// Subscribe to privacy state changes.
    ///
    /// The receiver will receive the state each time it changes.
    /// Call `get()` first if you need the current state immediately.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<PrivacyStateData> {
        self.store.subscribe()
    }

    /// Get the effective privacy settings for a conversation.
    ///
    /// Returns per-conversation settings if set, otherwise creates
    /// settings based on global defaults.
    #[must_use]
    pub fn get_settings(&self, channel_id: &ChannelId) -> ConversationPrivacySettings {
        let data = self.store.get();
        data.conversation_settings
            .get(channel_id)
            .cloned()
            .unwrap_or(ConversationPrivacySettings {
                read_receipts_enabled: data.global_read_receipts,
                typing_indicators_enabled: data.global_typing_indicators,
                screenshots_allowed: true,
            })
    }

    /// Set per-conversation privacy settings.
    ///
    /// These override global defaults for this specific conversation.
    pub fn set_settings(&self, channel_id: ChannelId, settings: ConversationPrivacySettings) {
        self.store.update(|state| {
            state.conversation_settings.insert(channel_id, settings);
        });
    }

    /// Remove per-conversation privacy settings.
    ///
    /// The conversation will fall back to global defaults.
    pub fn remove_settings(&self, channel_id: &ChannelId) {
        self.store.update(|state| {
            state.conversation_settings.remove(channel_id);
        });
    }

    /// Set the global default for read receipts.
    pub fn set_global_read_receipts(&self, enabled: bool) {
        self.store.update(|state| {
            state.global_read_receipts = enabled;
        });
    }

    /// Set the global default for typing indicators.
    pub fn set_global_typing_indicators(&self, enabled: bool) {
        self.store.update(|state| {
            state.global_typing_indicators = enabled;
        });
    }

    /// Check if read receipts are enabled for a conversation.
    ///
    /// Returns per-conversation setting if set, otherwise global default.
    #[must_use]
    pub fn are_read_receipts_enabled(&self, channel_id: &ChannelId) -> bool {
        let data = self.store.get();
        data.conversation_settings
            .get(channel_id)
            .map(|s| s.read_receipts_enabled)
            .unwrap_or(data.global_read_receipts)
    }

    /// Check if typing indicators are enabled for a conversation.
    ///
    /// Returns per-conversation setting if set, otherwise global default.
    #[must_use]
    pub fn are_typing_indicators_enabled(&self, channel_id: &ChannelId) -> bool {
        let data = self.store.get();
        data.conversation_settings
            .get(channel_id)
            .map(|s| s.typing_indicators_enabled)
            .unwrap_or(data.global_typing_indicators)
    }

    /// Reset all settings to defaults.
    pub fn clear(&self) {
        self.store.set(PrivacyStateData::default());
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
    use uuid::Uuid;

    fn test_channel_id() -> ChannelId {
        ChannelId::from_uuid(Uuid::new_v4())
    }

    #[test]
    fn test_privacy_state_default() {
        let state = PrivacyState::new();
        let data = state.get();

        assert!(data.conversation_settings.is_empty());
        assert!(data.global_read_receipts);
        assert!(data.global_typing_indicators);
    }

    #[test]
    fn test_conversation_privacy_settings_default() {
        let settings = ConversationPrivacySettings::default();

        assert!(settings.read_receipts_enabled);
        assert!(settings.typing_indicators_enabled);
        assert!(settings.screenshots_allowed);
    }

    #[test]
    fn test_conversation_privacy_settings_private() {
        let settings = ConversationPrivacySettings::private();

        assert!(!settings.read_receipts_enabled);
        assert!(!settings.typing_indicators_enabled);
        assert!(!settings.screenshots_allowed);
    }

    #[test]
    fn test_get_settings_falls_back_to_global() {
        let state = PrivacyState::new();
        let channel_id = test_channel_id();

        // No per-conversation setting, should use global defaults
        let settings = state.get_settings(&channel_id);
        assert!(settings.read_receipts_enabled);
        assert!(settings.typing_indicators_enabled);

        // Change global
        state.set_global_read_receipts(false);

        let settings = state.get_settings(&channel_id);
        assert!(!settings.read_receipts_enabled);
        assert!(settings.typing_indicators_enabled);
    }

    #[test]
    fn test_set_and_get_settings() {
        let state = PrivacyState::new();
        let channel_id = test_channel_id();

        state.set_settings(channel_id, ConversationPrivacySettings::private());

        let settings = state.get_settings(&channel_id);
        assert!(!settings.read_receipts_enabled);
        assert!(!settings.typing_indicators_enabled);
        assert!(!settings.screenshots_allowed);
    }

    #[test]
    fn test_remove_settings() {
        let state = PrivacyState::new();
        let channel_id = test_channel_id();

        // Set custom settings
        state.set_settings(channel_id, ConversationPrivacySettings::private());
        assert!(!state.are_read_receipts_enabled(&channel_id));

        // Remove custom settings
        state.remove_settings(&channel_id);

        // Should fall back to global (enabled by default)
        assert!(state.are_read_receipts_enabled(&channel_id));
    }

    #[test]
    fn test_global_read_receipts() {
        let state = PrivacyState::new();
        let channel_id = test_channel_id();

        assert!(state.are_read_receipts_enabled(&channel_id));

        state.set_global_read_receipts(false);
        assert!(!state.are_read_receipts_enabled(&channel_id));

        state.set_global_read_receipts(true);
        assert!(state.are_read_receipts_enabled(&channel_id));
    }

    #[test]
    fn test_global_typing_indicators() {
        let state = PrivacyState::new();
        let channel_id = test_channel_id();

        assert!(state.are_typing_indicators_enabled(&channel_id));

        state.set_global_typing_indicators(false);
        assert!(!state.are_typing_indicators_enabled(&channel_id));

        state.set_global_typing_indicators(true);
        assert!(state.are_typing_indicators_enabled(&channel_id));
    }

    #[test]
    fn test_per_conversation_overrides_global() {
        let state = PrivacyState::new();
        let channel_id = test_channel_id();

        // Set global to enabled
        state.set_global_read_receipts(true);
        assert!(state.are_read_receipts_enabled(&channel_id));

        // Override per-conversation to disabled
        state.set_settings(channel_id, ConversationPrivacySettings::private());
        assert!(!state.are_read_receipts_enabled(&channel_id));

        // Global change doesn't affect overridden conversation
        state.set_global_read_receipts(false);
        assert!(!state.are_read_receipts_enabled(&channel_id));

        // Remove override, now follows global
        state.remove_settings(&channel_id);
        assert!(!state.are_read_receipts_enabled(&channel_id));
    }

    #[test]
    fn test_clear() {
        let state = PrivacyState::new();
        let channel_id = test_channel_id();

        // Modify state
        state.set_global_read_receipts(false);
        state.set_global_typing_indicators(false);
        state.set_settings(channel_id, ConversationPrivacySettings::private());

        // Clear
        state.clear();
        let data = state.get();

        assert!(data.conversation_settings.is_empty());
        assert!(data.global_read_receipts);
        assert!(data.global_typing_indicators);
    }

    #[tokio::test]
    async fn test_subscribe() {
        let state = PrivacyState::new();
        let mut rx = state.subscribe();
        let channel_id = test_channel_id();

        state.set_settings(channel_id, ConversationPrivacySettings::private());

        let received = rx.recv().await.unwrap();
        assert!(received.conversation_settings.contains_key(&channel_id));
    }

    #[test]
    fn test_clone_shares_state() {
        let state1 = PrivacyState::new();
        let state2 = state1.clone();
        let channel_id = test_channel_id();

        state1.set_settings(channel_id, ConversationPrivacySettings::private());

        // Both handles see the same state
        assert!(!state2.are_read_receipts_enabled(&channel_id));
    }
}
