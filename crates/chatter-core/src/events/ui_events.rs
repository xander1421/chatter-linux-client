//! UI-specific events for view layer reactivity.
//!
//! These events trigger UI updates that don't correspond to domain changes.
//! Domain events (message received, typing, etc.) are handled separately
//! via `DomainEvent` from `inbox::handlers`.
//!
//! # Event Categories
//!
//! - **Navigation**: Channel/thread selection, scroll positions
//! - **Modal**: Opening/closing dialogs
//! - **Input**: Composer focus, draft updates
//! - **Sidebar**: Section toggling, collapse state
//! - **Search**: Search UI state
//! - **Selection**: Message selection for bulk actions
//! - **Settings**: Settings panel navigation
//! - **Toast**: Transient notifications

use crate::domain::ids::{ChannelId, MessageId};
use crate::state::ui_state::ModalType;

/// UI-specific events for view layer reactivity.
///
/// These events are emitted by UI components and consumed by other UI components
/// or the application shell. They do NOT represent domain/business events.
///
/// # Example
///
/// ```ignore
/// use chatter_linux::events::{EventBus, UiEvent};
/// use chatter_linux::domain::ids::ChannelId;
///
/// let bus = EventBus::new();
///
/// // Navigate to a channel
/// bus.emit_ui(UiEvent::ChannelSelected { channel_id });
///
/// // Show a toast notification
/// bus.emit_ui(UiEvent::ToastShown {
///     id: "toast-1".to_string(),
///     message: "Message sent!".to_string(),
///     level: ToastLevel::Success,
/// });
/// ```
#[derive(Debug, Clone)]
pub enum UiEvent {
    // ========================================================================
    // Navigation Events
    // ========================================================================
    /// User selected a channel to view.
    ChannelSelected {
        /// The channel to navigate to.
        channel_id: ChannelId,
    },

    /// User opened a thread panel.
    ThreadOpened {
        /// Channel containing the thread.
        channel_id: ChannelId,
        /// Root message ID of the thread.
        thread_id: MessageId,
    },

    /// User closed a thread panel.
    ThreadClosed {
        /// The thread that was closed.
        thread_id: MessageId,
    },

    // ========================================================================
    // Modal Events
    // ========================================================================
    /// A modal dialog was opened.
    ModalOpened {
        /// The type of modal that was opened.
        modal: ModalType,
    },

    /// A modal dialog was closed.
    ModalClosed {
        /// The type of modal that was closed.
        modal: ModalType,
    },

    // ========================================================================
    // Scroll Events
    // ========================================================================
    /// Request to scroll to a specific message.
    ///
    /// Used for "jump to message" features like search results,
    /// quote replies, or pinned message navigation.
    ScrollToMessage {
        /// The message to scroll into view.
        message_id: MessageId,
    },

    /// Request to scroll to the bottom of a channel.
    ///
    /// Used when new messages arrive or user clicks "new messages" indicator.
    ScrollToBottom {
        /// The channel to scroll.
        channel_id: ChannelId,
    },

    // ========================================================================
    // Input Events
    // ========================================================================
    /// A draft was updated in the composer.
    ///
    /// Emitted when the user types in the message composer.
    /// Used for draft persistence and typing indicator triggers.
    DraftUpdated {
        /// Channel where the draft is being edited.
        channel_id: ChannelId,
        /// Thread ID if editing in a thread, None for main channel.
        thread_id: Option<MessageId>,
    },

    /// Composer received focus.
    ComposerFocused {
        /// Channel whose composer received focus.
        channel_id: ChannelId,
    },

    /// Composer lost focus.
    ComposerBlurred {
        /// Channel whose composer lost focus.
        channel_id: ChannelId,
    },

    // ========================================================================
    // Sidebar Events
    // ========================================================================
    /// Sidebar expanded/collapsed state changed.
    SidebarToggled {
        /// Whether the sidebar is now expanded.
        expanded: bool,
    },

    /// A sidebar section was toggled (e.g., Channels, DMs).
    SidebarSectionToggled {
        /// The section that was toggled.
        section: SidebarSection,
        /// Whether the section is now expanded.
        expanded: bool,
    },

    // ========================================================================
    // Search Events
    // ========================================================================
    /// Search panel was opened.
    SearchOpened,

    /// Search panel was closed.
    SearchClosed,

    /// Search query changed.
    SearchQueryChanged {
        /// The new search query.
        query: String,
    },

    // ========================================================================
    // Selection Events
    // ========================================================================
    /// A message was selected (for bulk operations).
    MessageSelected {
        /// The message that was selected.
        message_id: MessageId,
    },

    /// A message was deselected.
    MessageDeselected {
        /// The message that was deselected.
        message_id: MessageId,
    },

    /// All message selections were cleared.
    SelectionCleared,

    // ========================================================================
    // Settings Events
    // ========================================================================
    /// Settings panel was opened.
    SettingsOpened {
        /// The tab to open in settings.
        tab: SettingsTab,
    },

    /// Settings panel was closed.
    SettingsClosed,

    // ========================================================================
    // Toast Notification Events
    // ========================================================================
    /// A toast notification was shown.
    ToastShown {
        /// Unique identifier for this toast (for dismissal).
        id: String,
        /// Message to display.
        message: String,
        /// Severity level of the toast.
        level: ToastLevel,
    },

    /// A toast notification was dismissed.
    ToastDismissed {
        /// ID of the toast that was dismissed.
        id: String,
    },
}

impl UiEvent {
    /// Check if this is a navigation event.
    ///
    /// Navigation events change what content is displayed.
    #[must_use]
    pub fn is_navigation(&self) -> bool {
        matches!(
            self,
            UiEvent::ChannelSelected { .. }
                | UiEvent::ThreadOpened { .. }
                | UiEvent::ThreadClosed { .. }
                | UiEvent::ScrollToMessage { .. }
                | UiEvent::ScrollToBottom { .. }
        )
    }

    /// Check if this is a modal event.
    #[must_use]
    pub fn is_modal(&self) -> bool {
        matches!(self, UiEvent::ModalOpened { .. } | UiEvent::ModalClosed { .. })
    }

    /// Check if this is an input event.
    ///
    /// Input events relate to user text input in composers.
    #[must_use]
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            UiEvent::DraftUpdated { .. }
                | UiEvent::ComposerFocused { .. }
                | UiEvent::ComposerBlurred { .. }
        )
    }

    /// Check if this is a search event.
    #[must_use]
    pub fn is_search(&self) -> bool {
        matches!(
            self,
            UiEvent::SearchOpened | UiEvent::SearchClosed | UiEvent::SearchQueryChanged { .. }
        )
    }

    /// Check if this is a selection event.
    #[must_use]
    pub fn is_selection(&self) -> bool {
        matches!(
            self,
            UiEvent::MessageSelected { .. }
                | UiEvent::MessageDeselected { .. }
                | UiEvent::SelectionCleared
        )
    }

    /// Check if this is a settings event.
    #[must_use]
    pub fn is_settings(&self) -> bool {
        matches!(
            self,
            UiEvent::SettingsOpened { .. } | UiEvent::SettingsClosed
        )
    }

    /// Check if this is a toast event.
    #[must_use]
    pub fn is_toast(&self) -> bool {
        matches!(
            self,
            UiEvent::ToastShown { .. } | UiEvent::ToastDismissed { .. }
        )
    }

    /// Get the channel ID associated with this event, if any.
    #[must_use]
    pub fn channel_id(&self) -> Option<ChannelId> {
        match self {
            UiEvent::ChannelSelected { channel_id } => Some(*channel_id),
            UiEvent::ThreadOpened { channel_id, .. } => Some(*channel_id),
            UiEvent::ScrollToBottom { channel_id } => Some(*channel_id),
            UiEvent::DraftUpdated { channel_id, .. } => Some(*channel_id),
            UiEvent::ComposerFocused { channel_id } => Some(*channel_id),
            UiEvent::ComposerBlurred { channel_id } => Some(*channel_id),
            _ => None,
        }
    }
}

/// Sidebar sections for collapse state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SidebarSection {
    /// Channels section (group chats, public channels).
    Channels,
    /// Direct messages section.
    DirectMessages,
    /// Saved messages / bookmarks section.
    SavedMessages,
}

impl SidebarSection {
    /// Get the display name for this section.
    #[must_use]
    pub fn display_name(&self) -> &'static str {
        match self {
            SidebarSection::Channels => "Channels",
            SidebarSection::DirectMessages => "Direct Messages",
            SidebarSection::SavedMessages => "Saved Messages",
        }
    }
}

/// Settings tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettingsTab {
    /// Account settings (username, email, etc.).
    Account,
    /// Privacy and security settings.
    Privacy,
    /// Appearance settings (theme, compact mode, etc.).
    Appearance,
    /// Notification preferences.
    Notifications,
    /// Encryption keys and device management.
    Encryption,
}

impl SettingsTab {
    /// Get the display name for this tab.
    #[must_use]
    pub fn display_name(&self) -> &'static str {
        match self {
            SettingsTab::Account => "Account",
            SettingsTab::Privacy => "Privacy & Security",
            SettingsTab::Appearance => "Appearance",
            SettingsTab::Notifications => "Notifications",
            SettingsTab::Encryption => "Encryption",
        }
    }

    /// Get all settings tabs in display order.
    #[must_use]
    pub fn all() -> &'static [SettingsTab] {
        &[
            SettingsTab::Account,
            SettingsTab::Privacy,
            SettingsTab::Appearance,
            SettingsTab::Notifications,
            SettingsTab::Encryption,
        ]
    }
}

/// Toast notification severity levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToastLevel {
    /// Informational message.
    Info,
    /// Success confirmation.
    Success,
    /// Warning that requires attention.
    Warning,
    /// Error that needs user action.
    Error,
}

impl ToastLevel {
    /// Get the CSS class suffix for this level.
    #[must_use]
    pub fn css_class(&self) -> &'static str {
        match self {
            ToastLevel::Info => "info",
            ToastLevel::Success => "success",
            ToastLevel::Warning => "warning",
            ToastLevel::Error => "error",
        }
    }

    /// Check if this level indicates an error condition.
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, ToastLevel::Error | ToastLevel::Warning)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn make_channel_id() -> ChannelId {
        ChannelId::from_uuid(Uuid::new_v4())
    }

    fn make_message_id() -> MessageId {
        MessageId::from_uuid(Uuid::new_v4())
    }

    // ========================================================================
    // UiEvent Tests
    // ========================================================================

    #[test]
    fn test_is_navigation() {
        let channel_id = make_channel_id();
        let message_id = make_message_id();

        assert!(UiEvent::ChannelSelected { channel_id }.is_navigation());
        assert!(UiEvent::ThreadOpened {
            channel_id,
            thread_id: message_id
        }
        .is_navigation());
        assert!(UiEvent::ThreadClosed {
            thread_id: message_id
        }
        .is_navigation());
        assert!(UiEvent::ScrollToMessage { message_id }.is_navigation());
        assert!(UiEvent::ScrollToBottom { channel_id }.is_navigation());

        assert!(!UiEvent::SearchOpened.is_navigation());
        assert!(!UiEvent::SettingsClosed.is_navigation());
    }

    #[test]
    fn test_is_modal() {
        let modal = ModalType::Settings;

        assert!(UiEvent::ModalOpened {
            modal: modal.clone()
        }
        .is_modal());
        assert!(UiEvent::ModalClosed { modal }.is_modal());

        assert!(!UiEvent::SearchOpened.is_modal());
    }

    #[test]
    fn test_is_input() {
        let channel_id = make_channel_id();

        assert!(UiEvent::DraftUpdated {
            channel_id,
            thread_id: None
        }
        .is_input());
        assert!(UiEvent::ComposerFocused { channel_id }.is_input());
        assert!(UiEvent::ComposerBlurred { channel_id }.is_input());

        assert!(!UiEvent::SearchOpened.is_input());
    }

    #[test]
    fn test_is_search() {
        assert!(UiEvent::SearchOpened.is_search());
        assert!(UiEvent::SearchClosed.is_search());
        assert!(UiEvent::SearchQueryChanged {
            query: "test".to_string()
        }
        .is_search());

        assert!(!UiEvent::SettingsClosed.is_search());
    }

    #[test]
    fn test_is_selection() {
        let message_id = make_message_id();

        assert!(UiEvent::MessageSelected { message_id }.is_selection());
        assert!(UiEvent::MessageDeselected { message_id }.is_selection());
        assert!(UiEvent::SelectionCleared.is_selection());

        assert!(!UiEvent::SearchOpened.is_selection());
    }

    #[test]
    fn test_is_settings() {
        assert!(UiEvent::SettingsOpened {
            tab: SettingsTab::Account
        }
        .is_settings());
        assert!(UiEvent::SettingsClosed.is_settings());

        assert!(!UiEvent::SearchOpened.is_settings());
    }

    #[test]
    fn test_is_toast() {
        assert!(UiEvent::ToastShown {
            id: "1".to_string(),
            message: "Test".to_string(),
            level: ToastLevel::Info,
        }
        .is_toast());
        assert!(UiEvent::ToastDismissed {
            id: "1".to_string()
        }
        .is_toast());

        assert!(!UiEvent::SearchOpened.is_toast());
    }

    #[test]
    fn test_channel_id_extraction() {
        let channel_id = make_channel_id();
        let message_id = make_message_id();

        assert_eq!(
            UiEvent::ChannelSelected { channel_id }.channel_id(),
            Some(channel_id)
        );
        assert_eq!(
            UiEvent::ThreadOpened {
                channel_id,
                thread_id: message_id
            }
            .channel_id(),
            Some(channel_id)
        );
        assert_eq!(
            UiEvent::ScrollToBottom { channel_id }.channel_id(),
            Some(channel_id)
        );
        assert_eq!(
            UiEvent::DraftUpdated {
                channel_id,
                thread_id: None
            }
            .channel_id(),
            Some(channel_id)
        );
        assert_eq!(
            UiEvent::ComposerFocused { channel_id }.channel_id(),
            Some(channel_id)
        );
        assert_eq!(
            UiEvent::ComposerBlurred { channel_id }.channel_id(),
            Some(channel_id)
        );

        // Events without channel ID
        assert_eq!(UiEvent::SearchOpened.channel_id(), None);
        assert_eq!(
            UiEvent::ThreadClosed {
                thread_id: message_id
            }
            .channel_id(),
            None
        );
        assert_eq!(
            UiEvent::ScrollToMessage { message_id }.channel_id(),
            None
        );
    }

    // ========================================================================
    // SidebarSection Tests
    // ========================================================================

    #[test]
    fn test_sidebar_section_display_name() {
        assert_eq!(SidebarSection::Channels.display_name(), "Channels");
        assert_eq!(
            SidebarSection::DirectMessages.display_name(),
            "Direct Messages"
        );
        assert_eq!(SidebarSection::SavedMessages.display_name(), "Saved Messages");
    }

    // ========================================================================
    // SettingsTab Tests
    // ========================================================================

    #[test]
    fn test_settings_tab_display_name() {
        assert_eq!(SettingsTab::Account.display_name(), "Account");
        assert_eq!(SettingsTab::Privacy.display_name(), "Privacy & Security");
        assert_eq!(SettingsTab::Appearance.display_name(), "Appearance");
        assert_eq!(SettingsTab::Notifications.display_name(), "Notifications");
        assert_eq!(SettingsTab::Encryption.display_name(), "Encryption");
    }

    #[test]
    fn test_settings_tab_all() {
        let all = SettingsTab::all();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0], SettingsTab::Account);
        assert_eq!(all[4], SettingsTab::Encryption);
    }

    // ========================================================================
    // ToastLevel Tests
    // ========================================================================

    #[test]
    fn test_toast_level_css_class() {
        assert_eq!(ToastLevel::Info.css_class(), "info");
        assert_eq!(ToastLevel::Success.css_class(), "success");
        assert_eq!(ToastLevel::Warning.css_class(), "warning");
        assert_eq!(ToastLevel::Error.css_class(), "error");
    }

    #[test]
    fn test_toast_level_is_error() {
        assert!(!ToastLevel::Info.is_error());
        assert!(!ToastLevel::Success.is_error());
        assert!(ToastLevel::Warning.is_error());
        assert!(ToastLevel::Error.is_error());
    }

    // ========================================================================
    // Clone Tests
    // ========================================================================

    #[test]
    fn test_ui_event_clone() {
        let event = UiEvent::ToastShown {
            id: "test".to_string(),
            message: "Hello".to_string(),
            level: ToastLevel::Success,
        };
        let cloned = event.clone();
        if let UiEvent::ToastShown { id, message, level } = cloned {
            assert_eq!(id, "test");
            assert_eq!(message, "Hello");
            assert_eq!(level, ToastLevel::Success);
        } else {
            panic!("Clone changed variant");
        }
    }

    #[test]
    fn test_enum_copy_traits() {
        // SidebarSection is Copy
        let section = SidebarSection::Channels;
        let copied = section;
        assert_eq!(copied, SidebarSection::Channels);

        // SettingsTab is Copy
        let tab = SettingsTab::Account;
        let copied = tab;
        assert_eq!(copied, SettingsTab::Account);

        // ToastLevel is Copy
        let level = ToastLevel::Info;
        let copied = level;
        assert_eq!(copied, ToastLevel::Info);
    }
}
