//! UI preferences state.
//!
//! Reactive theme, compact mode, and UI preferences state.

use tokio::sync::broadcast;

use crate::domain::ids::{ChannelId, MessageId, UserId};
use crate::state::store::Store;

/// UI theme options.
///
/// Currently only dark theme is supported (v1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    /// Dark theme (default and only option for v1).
    #[default]
    Dark,
}

/// Types of modals that can be displayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModalType {
    /// Application settings modal.
    Settings,
    /// Create new channel modal.
    CreateChannel,
    /// User profile view modal.
    UserProfile(UserId),
    /// Channel settings modal.
    ChannelSettings(ChannelId),
    /// Forward message modal.
    Forward(MessageId),
}

/// UI state data.
///
/// Tracks UI preferences and transient visual state.
#[derive(Clone, Debug)]
pub struct UIStateData {
    /// Current UI theme (dark only for v1).
    pub theme: Theme,
    /// Compact message display mode (reduced spacing).
    pub compact_mode: bool,
    /// Whether to show timestamps on messages.
    pub show_timestamps: bool,
    /// Whether to show user avatars in message list.
    pub show_avatars: bool,
    /// Whether the sidebar is collapsed.
    pub sidebar_collapsed: bool,
    /// Currently open modal, if any.
    pub active_modal: Option<ModalType>,
}

impl Default for UIStateData {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            compact_mode: false,
            show_timestamps: true,
            show_avatars: true,
            sidebar_collapsed: false,
            active_modal: None,
        }
    }
}

/// Reactive UI state manager.
///
/// Provides reactive UI preferences state that UI components can
/// subscribe to for live updates.
///
/// # Example
///
/// ```ignore
/// let ui_state = UIState::new();
///
/// // Subscribe to UI changes
/// let mut rx = ui_state.subscribe();
///
/// // Toggle compact mode
/// ui_state.set_compact_mode(true);
///
/// // Open a modal
/// ui_state.open_modal(ModalType::Settings);
///
/// // Close the modal
/// ui_state.close_modal();
/// ```
#[derive(Clone)]
pub struct UIState {
    store: Store<UIStateData>,
}

impl UIState {
    /// Create a new UI state with default preferences.
    ///
    /// Defaults:
    /// - Dark theme
    /// - Compact mode off
    /// - Timestamps shown
    /// - Avatars shown
    /// - Sidebar expanded
    /// - No active modal
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Store::new(UIStateData::default()),
        }
    }

    /// Get the current UI state.
    #[must_use]
    pub fn get(&self) -> UIStateData {
        self.store.get()
    }

    /// Subscribe to UI state changes (Arc-wrapped for efficiency).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<super::StateSnapshot<UIStateData>> {
        self.store.subscribe()
    }

    /// Set compact message display mode.
    pub fn set_compact_mode(&self, compact: bool) {
        self.store.update(|state| {
            state.compact_mode = compact;
        });
    }

    /// Set whether timestamps are shown.
    pub fn set_show_timestamps(&self, show: bool) {
        self.store.update(|state| {
            state.show_timestamps = show;
        });
    }

    /// Set whether avatars are shown.
    pub fn set_show_avatars(&self, show: bool) {
        self.store.update(|state| {
            state.show_avatars = show;
        });
    }

    /// Set sidebar collapsed state.
    pub fn set_sidebar_collapsed(&self, collapsed: bool) {
        self.store.update(|state| {
            state.sidebar_collapsed = collapsed;
        });
    }

    /// Toggle sidebar collapsed state.
    pub fn toggle_sidebar(&self) {
        self.store.update(|state| {
            state.sidebar_collapsed = !state.sidebar_collapsed;
        });
    }

    /// Open a modal.
    ///
    /// Replaces any currently open modal.
    pub fn open_modal(&self, modal: ModalType) {
        self.store.update(|state| {
            state.active_modal = Some(modal);
        });
    }

    /// Close the currently open modal.
    pub fn close_modal(&self) {
        self.store.update(|state| {
            state.active_modal = None;
        });
    }

    /// Check if a modal is currently open.
    #[must_use]
    pub fn has_modal(&self) -> bool {
        self.store.get().active_modal.is_some()
    }
}

impl Default for UIState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn test_ui_state_defaults() {
        let state = UIState::new();
        let data = state.get();

        assert_eq!(data.theme, Theme::Dark);
        assert!(!data.compact_mode);
        assert!(data.show_timestamps);
        assert!(data.show_avatars);
        assert!(!data.sidebar_collapsed);
        assert!(data.active_modal.is_none());
    }

    #[test]
    fn test_ui_state_compact_mode() {
        let state = UIState::new();

        state.set_compact_mode(true);
        assert!(state.get().compact_mode);

        state.set_compact_mode(false);
        assert!(!state.get().compact_mode);
    }

    #[test]
    fn test_ui_state_timestamps() {
        let state = UIState::new();
        assert!(state.get().show_timestamps);

        state.set_show_timestamps(false);
        assert!(!state.get().show_timestamps);
    }

    #[test]
    fn test_ui_state_avatars() {
        let state = UIState::new();
        assert!(state.get().show_avatars);

        state.set_show_avatars(false);
        assert!(!state.get().show_avatars);
    }

    #[test]
    fn test_ui_state_sidebar() {
        let state = UIState::new();
        assert!(!state.get().sidebar_collapsed);

        state.set_sidebar_collapsed(true);
        assert!(state.get().sidebar_collapsed);

        state.toggle_sidebar();
        assert!(!state.get().sidebar_collapsed);

        state.toggle_sidebar();
        assert!(state.get().sidebar_collapsed);
    }

    #[test]
    fn test_ui_state_modal_lifecycle() {
        let state = UIState::new();
        assert!(!state.has_modal());

        // Open settings modal
        state.open_modal(ModalType::Settings);
        assert!(state.has_modal());
        assert_eq!(state.get().active_modal, Some(ModalType::Settings));

        // Open different modal (replaces previous)
        let user_id = UserId::from_uuid(Uuid::new_v4());
        state.open_modal(ModalType::UserProfile(user_id));
        assert_eq!(state.get().active_modal, Some(ModalType::UserProfile(user_id)));

        // Close modal
        state.close_modal();
        assert!(!state.has_modal());
        assert!(state.get().active_modal.is_none());
    }

    #[test]
    fn test_modal_type_variants() {
        let user_id = UserId::from_uuid(Uuid::new_v4());
        let channel_id = ChannelId::from_uuid(Uuid::new_v4());
        let message_id = MessageId::from_uuid(Uuid::new_v4());

        // Test all variants can be created
        let _settings = ModalType::Settings;
        let _create_channel = ModalType::CreateChannel;
        let _user_profile = ModalType::UserProfile(user_id);
        let _channel_settings = ModalType::ChannelSettings(channel_id);
        let _forward = ModalType::Forward(message_id);
    }

    #[tokio::test]
    async fn test_ui_state_subscribe() {
        let state = UIState::new();
        let mut rx = state.subscribe();

        state.set_compact_mode(true);

        let received = rx.recv().await.unwrap();
        assert!(received.compact_mode);
    }

    #[test]
    fn test_ui_state_clone_shares_state() {
        let state1 = UIState::new();
        let state2 = state1.clone();

        state1.set_sidebar_collapsed(true);

        // Both handles see the same state
        assert!(state2.get().sidebar_collapsed);
    }
}
