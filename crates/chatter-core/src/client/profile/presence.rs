use super::*;

/// How long a peer's reported status holds without a refresh: the server's
/// five-minute presence window plus one missed two-minute heartbeat.
pub const PRESENCE_STALE_SECS: i64 = 7 * 60;

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
    /// user's presence is hidden. A disconnected client also hides the last
    /// reported online state until it can receive fresh presence again. A
    /// status not refreshed for [`PRESENCE_STALE_SECS`] is offline: live
    /// clients republish every two minutes, so silence means the peer's app
    /// died without sending Offline.
    pub fn peer_display_presence(
        &self,
        id: &crate::domain::ids::UserId,
        reported: crate::domain::user::PresenceStatus,
    ) -> crate::domain::user::PresenceStatus {
        if !self.offline_snapshot().is_online || self.is_profile_user_blocked(id) {
            return crate::domain::user::PresenceStatus::Offline;
        }
        if reported != crate::domain::user::PresenceStatus::Offline {
            let repo =
                crate::storage::repositories::UserRepository::new(self.services.database.clone());
            let stale = repo.last_seen_at(id).ok().flatten().is_none_or(|seen| {
                chrono::Utc::now().timestamp() - seen > PRESENCE_STALE_SECS
            });
            if stale {
                return crate::domain::user::PresenceStatus::Offline;
            }
        }
        reported
    }

    /// Anonymous chats have no live peer presence feed. Show their active
    /// indicator only while this device can reach the server.
    pub fn anon_peer_presence(&self) -> crate::domain::user::PresenceStatus {
        if self.offline_snapshot().is_online {
            crate::domain::user::PresenceStatus::Online
        } else {
            crate::domain::user::PresenceStatus::Offline
        }
    }

    /// Presence for the currently open DM, from the same core policy as its row.
    pub fn active_dm_presence(&self) -> Option<crate::domain::user::PresenceStatus> {
        match self.active_conversation() {
            crate::state::ActiveConversation::ServerDm { peer_id } => {
                Some(self.peer_presence(&peer_id))
            }
            crate::state::ActiveConversation::AnonDm(_) => Some(self.anon_peer_presence()),
            _ => None,
        }
    }
}
