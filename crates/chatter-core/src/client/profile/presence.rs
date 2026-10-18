use super::*;

impl ChatterClient {
    /// A peer's presence as this device shows it: the stored presence, or
    /// [`Self::peer_display_presence`] of it. Every dot and "online" line for
    /// another user reads this, never `Profile::presence` directly.
    pub fn peer_presence(
        &self,
        id: &crate::domain::ids::UserId,
    ) -> crate::domain::user::PresenceStatus {
        let stored = self
            .get_profile_local(id)
            .map(|p| p.presence)
            .unwrap_or_default();
        self.peer_display_presence(id, stored)
    }

    /// `reported` (a stored or live presence) as it may be shown. A blocked
    /// user's presence is hidden — always offline — as Signal shows none for
    /// people you blocked; unblocking shows it again.
    pub fn peer_display_presence(
        &self,
        id: &crate::domain::ids::UserId,
        reported: crate::domain::user::PresenceStatus,
    ) -> crate::domain::user::PresenceStatus {
        if self.is_profile_user_blocked(id) {
            crate::domain::user::PresenceStatus::Offline
        } else {
            reported
        }
    }

}
