//! UI events.
//!
//! UI-specific events.

/// UI event types.
pub enum UiEvent {
    /// Channel selected.
    ChannelSelected,
    /// Thread opened.
    ThreadOpened,
    /// Modal opened.
    ModalOpened,
    /// Modal closed.
    ModalClosed,
}
