//! The Nostr event and the kinds bitchat uses.
//!
//! Frozen types: `event`, `builders`, `wrap`, and `relay` all depend on these, so
//! they are defined once here rather than negotiated between modules.

use serde::{Deserialize, Serialize};

/// A Nostr event.
///
/// `id` and `sig` are hex strings rather than byte arrays because that is how they
/// travel on the wire and how they are hashed — converting back and forth would add
/// a class of bug (case, padding) for no benefit.
///
/// `sig` is `None` on an unsigned draft. `id` is empty until signing computes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NostrEvent {
    /// Empty on an unsigned draft, so it carries `#[serde(default)]`: a kind-14
    /// rumor is legitimately never signed and has neither `id` nor `sig`.
    #[serde(default)]
    pub id: String,
    pub pubkey: String,
    pub created_at: i64,
    pub kind: u16,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

impl NostrEvent {
    /// An unsigned event. `id` and `sig` are filled in by signing.
    pub fn unsigned(
        pubkey: String,
        created_at: i64,
        kind: EventKind,
        tags: Vec<Vec<String>>,
        content: String,
    ) -> Self {
        Self {
            id: String::new(),
            pubkey,
            created_at,
            kind: kind.as_u16(),
            tags,
            content,
            sig: None,
        }
    }

    /// First value of the first tag with this name, if any.
    ///
    /// Tag lookup is by name at index 0, value at index 1 — the NIP-01 shape.
    ///
    /// Deliberately lenient: a name-only tag is skipped rather than shadowing a
    /// well-formed one later in the list. A peer emitting one junk `["g"]` tag must
    /// not cause us to drop the real geohash sitting behind it.
    pub fn tag_value(&self, name: &str) -> Option<&str> {
        self.tags
            .iter()
            .filter(|tag| tag.first().map(String::as_str) == Some(name))
            .find_map(|tag| tag.get(1))
            .map(String::as_str)
    }

    /// Every value for a repeated tag name.
    pub fn tag_values(&self, name: &str) -> Vec<&str> {
        self.tags
            .iter()
            .filter(|tag| tag.first().map(String::as_str) == Some(name))
            .filter_map(|tag| tag.get(1))
            .map(String::as_str)
            .collect()
    }

    pub fn event_kind(&self) -> Option<EventKind> {
        EventKind::from_u16(self.kind)
    }
}

/// The event kinds bitchat uses.
///
/// Kept as a closed enum for the ones we produce, with `from_u16` returning `None`
/// for anything else — inbound events of unknown kind are ignored, not rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum EventKind {
    /// NIP-01 text note. Used for persistent location notes.
    TextNote = 1,
    /// NIP-09 deletion.
    Deletion = 5,
    /// NIP-17 DM rumor — never signed, never published on its own.
    DirectMessage = 14,
    /// NIP-59 seal.
    Seal = 13,
    /// NIP-59 gift wrap, published under a throwaway key.
    GiftWrap = 1059,
    /// Courier drop: an opaque sealed envelope addressed by rotating tag.
    /// Regular (stored) kind, so it survives until its own expiry.
    CourierDrop = 1401,
    /// Geohash chat message. Ephemeral — relays do not retain it.
    EphemeralEvent = 20000,
    /// Geohash presence heartbeat. Empty content, `g` tag only.
    GeohashPresence = 20001,
}

impl EventKind {
    pub fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            1 => Self::TextNote,
            5 => Self::Deletion,
            13 => Self::Seal,
            14 => Self::DirectMessage,
            1059 => Self::GiftWrap,
            1401 => Self::CourierDrop,
            20000 => Self::EphemeralEvent,
            20001 => Self::GeohashPresence,
            _ => return None,
        })
    }

    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Ephemeral kinds are not stored by relays, so they can never be fetched back
    /// and must not be relied on for history.
    pub const fn is_ephemeral(self) -> bool {
        matches!(self, Self::EphemeralEvent | Self::GeohashPresence)
    }
}

/// Geohash precision level, and the privacy rule attached to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GeohashLevel {
    Region = 2,
    Province = 4,
    City = 5,
    Neighborhood = 6,
    Block = 7,
    Building = 8,
}

impl GeohashLevel {
    /// The finest precision at which a presence heartbeat may be broadcast.
    ///
    /// Announcing presence in a block- or building-sized cell would publish the
    /// user's location to anyone watching the relay. bitchat's spec forbids it, and
    /// the builder enforces it rather than trusting call sites.
    pub const MAX_PRESENCE_PRECISION: usize = 5;

    pub const fn precision(self) -> usize {
        self as usize
    }

    pub const fn allows_presence(self) -> bool {
        self.precision() <= Self::MAX_PRESENCE_PRECISION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kinds_round_trip() {
        for kind in [
            EventKind::TextNote,
            EventKind::Deletion,
            EventKind::Seal,
            EventKind::DirectMessage,
            EventKind::GiftWrap,
            EventKind::CourierDrop,
            EventKind::EphemeralEvent,
            EventKind::GeohashPresence,
        ] {
            assert_eq!(EventKind::from_u16(kind.as_u16()), Some(kind));
        }
        assert_eq!(EventKind::from_u16(0), None);
        assert_eq!(EventKind::from_u16(30023), None);
    }

    #[test]
    fn only_the_two_geohash_kinds_are_ephemeral() {
        assert!(EventKind::EphemeralEvent.is_ephemeral());
        assert!(EventKind::GeohashPresence.is_ephemeral());
        assert!(!EventKind::CourierDrop.is_ephemeral());
        assert!(!EventKind::GiftWrap.is_ephemeral());
    }

    #[test]
    fn presence_is_allowed_only_through_city_precision() {
        assert!(GeohashLevel::Region.allows_presence());
        assert!(GeohashLevel::Province.allows_presence());
        assert!(GeohashLevel::City.allows_presence());
        assert!(!GeohashLevel::Neighborhood.allows_presence());
        assert!(!GeohashLevel::Block.allows_presence());
        assert!(!GeohashLevel::Building.allows_presence());
    }

    #[test]
    fn tag_lookup_tolerates_malformed_tags() {
        let event = NostrEvent {
            id: String::new(),
            pubkey: "ab".into(),
            created_at: 0,
            kind: 20000,
            tags: vec![
                vec![],                                   // empty tag
                vec!["g".into()],                         // name with no value
                vec!["g".into(), "dr5rsj7".into()],       // the real one
                vec!["n".into(), "alice".into()],
                vec!["g".into(), "dr5".into()],           // repeated name
            ],
            content: String::new(),
            sig: None,
        };
        assert_eq!(event.tag_value("g"), Some("dr5rsj7"));
        assert_eq!(event.tag_value("n"), Some("alice"));
        assert_eq!(event.tag_value("absent"), None);
        assert_eq!(event.tag_values("g"), vec!["dr5rsj7", "dr5"]);
    }
}
