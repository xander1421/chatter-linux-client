//! Presence proto <-> domain conversion helpers.
//!
//! Identified presence wiring (the whole pipeline):
//! - producer  — `ChatterClient::set_presence` calls the identified
//!   `UserService.UpdatePresence` RPC (Online on start, Offline on shutdown,
//!   Away via UI idle detection) alongside the anon-DM E2E presence fan-out.
//! - fan-out   — the server publishes an `EventTypeUserPresenceUpdated`
//!   UserEvent to each FRIEND's `StreamUserEvents` channel (friend-scoped;
//!   there is no global presence stream, by design).
//! - consumer  — `inbox::streaming::user_events` maps `PresenceChanged` to
//!   `DomainEvent::UserPresenceChanged` using the converters below.

use crate::domain::user::PresenceStatus;
use crate::proto::chat_v1::UserPresence;

/// Convert domain PresenceStatus to proto UserPresence.
pub(crate) fn domain_presence_to_proto(status: PresenceStatus) -> UserPresence {
    match status {
        PresenceStatus::Offline => UserPresence::Offline,
        PresenceStatus::Online => UserPresence::Online,
        PresenceStatus::Away => UserPresence::Away,
        PresenceStatus::DoNotDisturb => UserPresence::DoNotDisturb,
    }
}

/// Convert proto UserPresence to domain PresenceStatus.
pub(crate) fn proto_presence_to_domain(proto: UserPresence) -> PresenceStatus {
    match proto {
        UserPresence::PresenceUnspecified => PresenceStatus::Offline,
        UserPresence::Offline => PresenceStatus::Offline,
        UserPresence::Online => PresenceStatus::Online,
        UserPresence::Away => PresenceStatus::Away,
        UserPresence::DoNotDisturb => PresenceStatus::DoNotDisturb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_domain_presence_to_proto() {
        assert_eq!(
            domain_presence_to_proto(PresenceStatus::Offline),
            UserPresence::Offline
        );
        assert_eq!(
            domain_presence_to_proto(PresenceStatus::Online),
            UserPresence::Online
        );
        assert_eq!(
            domain_presence_to_proto(PresenceStatus::Away),
            UserPresence::Away
        );
        assert_eq!(
            domain_presence_to_proto(PresenceStatus::DoNotDisturb),
            UserPresence::DoNotDisturb
        );
    }

    #[test]
    fn test_proto_presence_to_domain() {
        assert_eq!(
            proto_presence_to_domain(UserPresence::Offline),
            PresenceStatus::Offline
        );
        assert_eq!(
            proto_presence_to_domain(UserPresence::Online),
            PresenceStatus::Online
        );
        assert_eq!(
            proto_presence_to_domain(UserPresence::Away),
            PresenceStatus::Away
        );
        assert_eq!(
            proto_presence_to_domain(UserPresence::DoNotDisturb),
            PresenceStatus::DoNotDisturb
        );
        // Unspecified defaults to Offline
        assert_eq!(
            proto_presence_to_domain(UserPresence::PresenceUnspecified),
            PresenceStatus::Offline
        );
    }

    #[test]
    fn test_presence_roundtrip() {
        for status in [
            PresenceStatus::Offline,
            PresenceStatus::Online,
            PresenceStatus::Away,
            PresenceStatus::DoNotDisturb,
        ] {
            let proto = domain_presence_to_proto(status);
            let back = proto_presence_to_domain(proto);
            assert_eq!(back, status);
        }
    }
}
