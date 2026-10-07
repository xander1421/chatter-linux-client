//! Cached avatar resolution shared by forward destination renderers.
use super::ChatterClient;
use crate::domain::message::ConversationType;
impl ChatterClient {
    /// Return real cached bytes; a missing photo stays absent rather than fabricated.
    pub fn forward_destination_avatar(&self, kind: &ConversationType, id: &str) -> Option<Vec<u8>> {
        match kind {
            ConversationType::ServerDm => id.parse().ok().and_then(|peer| self.user_avatar(&peer)),
            ConversationType::AnonDm => id
                .parse()
                .ok()
                .and_then(|conversation| self.get_anon_dm_conversation(&conversation))
                .and_then(|dm| dm.peer_avatar),
            ConversationType::Channel => id
                .parse()
                .ok()
                .and_then(|channel| self.get_channel_local(&channel))
                .and_then(|channel| channel.icon),
            ConversationType::SavedMessages => self.my_avatar(),
            // Private groups currently have no stored group avatar field.
            _ => None,
        }
        .filter(|bytes| !bytes.is_empty())
    }
}
