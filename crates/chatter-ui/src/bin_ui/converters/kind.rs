//! The one conversion between core's `ConversationType` and the Slint
//! `ConversationKind` enum. Both matches are exhaustive, so a new core
//! variant fails to compile here instead of rendering as some other kind.

use chatter_core::domain::message::ConversationType;

use crate::{ConversationKind, MainWindow};

pub fn conversation_kind(ct: &ConversationType) -> ConversationKind {
    match ct {
        ConversationType::Channel => ConversationKind::Channel,
        ConversationType::SavedMessages => ConversationKind::SavedMessages,
        ConversationType::ServerDm => ConversationKind::ServerDm,
        ConversationType::AnonDm => ConversationKind::AnonDm,
        ConversationType::AnonGroup => ConversationKind::AnonGroup,
        ConversationType::Mesh => ConversationKind::Mesh,
        ConversationType::MeshDm => ConversationKind::MeshDm,
    }
}

pub fn conversation_type(kind: ConversationKind) -> ConversationType {
    match kind {
        ConversationKind::Channel => ConversationType::Channel,
        ConversationKind::SavedMessages => ConversationType::SavedMessages,
        ConversationKind::ServerDm => ConversationType::ServerDm,
        ConversationKind::AnonDm => ConversationType::AnonDm,
        ConversationKind::AnonGroup => ConversationType::AnonGroup,
        ConversationKind::Mesh => ConversationType::Mesh,
        ConversationKind::MeshDm => ConversationType::MeshDm,
    }
}

/// Set the open conversation's kind on the header, thread panel and privacy
/// line, and whether core offers the GIF picker there (not in a secret
/// chat). `private_channel` only matters for a channel (invite-only). Clears
/// the privacy-line override, so a Jarvis statement never survives a switch
/// into another conversation; the Jarvis view sets it again after this.
pub fn set_conversation_kind(w: &MainWindow, ct: &ConversationType, private_channel: bool) {
    w.set_conversation_kind(conversation_kind(ct));
    w.set_private_channel(private_channel);
    w.set_composer_gifs_available(ct.gif_picker_available());
    w.set_privacy_line_override(Default::default());
    w.set_privacy_line_exposed(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_conversation_type_round_trips() {
        for ct in [
            ConversationType::Channel,
            ConversationType::SavedMessages,
            ConversationType::ServerDm,
            ConversationType::AnonDm,
            ConversationType::AnonGroup,
            ConversationType::Mesh,
            ConversationType::MeshDm,
        ] {
            assert_eq!(conversation_type(conversation_kind(&ct)), ct);
        }
    }
}
