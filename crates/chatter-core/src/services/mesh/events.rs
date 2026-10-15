//! What [`MeshService`](super::MeshService) reports upward.
//!
//! A `tokio::sync::broadcast` stream, matching the other client-managed
//! conversation types in this crate. Everything here is already shaped: P7
//! renders these values and makes no decisions about them.

use chatter_mesh::ids::PeerId;

use crate::domain::message::Message;

use super::mapping::MeshConversation;

/// A mesh message, in domain form plus the identity context the domain type has
/// nowhere to put.
#[derive(Clone, Debug, PartialEq)]
pub struct MeshMessage {
    /// The timeline row.
    pub message: Message,
    /// Which mesh scope it belongs to.
    pub conversation: MeshConversation,
    /// The sending peer.
    pub sender: PeerId,
    /// The sender's self-declared nickname at the time it was sent.
    ///
    /// Unauthenticated for public traffic — anyone can claim any nickname on a
    /// broadcast — so it is a display string and never an identity.
    pub nickname: String,
    /// Whether this device authored the message.
    pub is_local: bool,
    /// Whether the sender's announce signature verified against the Ed25519 key
    /// it advertises. Only ever true for traffic that carried a signature.
    pub sender_verified: bool,
}

/// Delivery progress for a message this device sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeshDelivery {
    /// Queued to a live link with an established Noise session; retained until ACK.
    Secure,
    /// A live link exists, but the DM waits for a Noise handshake.
    Insecure,
    /// The peer was recently reachable, but no live secure delivery is known.
    Reachable,
    /// Nothing could carry it. Retained, and offered to couriers.
    Unreachable,
    /// At least one courier, or a relay drop, accepted a sealed copy.
    Carried,
    /// The recipient acknowledged receipt.
    Delivered,
    /// The recipient read it.
    Read,
    /// It left the outbox without ever being acknowledged.
    Dropped,
    /// Put on the air for the room or a cell.
    ///
    /// Every other variant describes a one-to-one delivery. A broadcast has no
    /// per-recipient notion at all: nobody acknowledges it, so there is no
    /// "delivered" to progress to and no outbox retaining it. Reusing `Secure`
    /// or `Reachable` here would claim something about a specific recipient
    /// that was never true.
    Broadcast,
}

impl MeshDelivery {
    /// The mesh's own name for this state.
    ///
    /// Canonical: it is what goes in the durable store (V060) *and* what the UI
    /// renders, so there is exactly one delivery vocabulary rather than a
    /// storage one and a display one drifting apart. Deliberately disjoint from
    /// the server vocabulary in `Message::delivery_status` — the mesh's "sent"
    /// and the server's "sent" are not the same claim.
    ///
    /// A queued DM without a Noise session stays "waiting" until encryption
    /// can happen; reachability alone makes no delivery claim.
    /// The distinctions that survive are the ones with consequences: it has not
    /// left yet, something else is carrying it, it never arrived, or there was
    /// never a recipient to arrive at.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            // Queued for a live link with an established Noise session.
            Self::Secure => "sent",
            Self::Insecure | Self::Reachable => "waiting",
            // Nothing could carry it; it is retained and offered to couriers.
            Self::Unreachable => "waiting",
            // A courier or a relay drop took a sealed copy.
            Self::Carried => "carried",
            Self::Delivered => "delivered",
            Self::Read => "read",
            Self::Dropped => "undelivered",
            // Nobody acknowledges a broadcast, so there is no later state to
            // progress to and nothing to be "delivered" to.
            Self::Broadcast => "broadcast",
        }
    }
}

/// One peer as the mesh currently sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeshPeer {
    /// Valid versioned Hello received inside this peer's current Noise session.
    pub chatter_capable: bool,
    pub peer: PeerId,
    pub nickname: String,
    /// A live link binding exists right now.
    pub connected: bool,
    /// Seen recently enough that the mesh still considers it reachable. This is
    /// a freshness heuristic and outlives the radio going quiet by up to a
    /// minute — never treat it as "the send will arrive".
    pub reachable: bool,
    /// Their announce signature verified.
    pub verified: bool,
    /// Account binding verified privately. Always false until that protocol exists.
    pub account_verified: bool,
    /// An established Noise session exists, so a DM can be encrypted right now.
    pub has_session: bool,
    /// SHA-256 of their Noise static key, lowercase hex — the thing users
    /// compare out of band.
    pub fingerprint: Option<String>,
}

/// Which transports are live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct MeshStatus {
    /// The radio is started.
    pub radio_running: bool,
    /// The Bluetooth adapter is powered.
    pub adapter_powered: bool,
    /// How many links are up and writable.
    pub connected_links: usize,
    /// How many peers the registry currently considers connected.
    pub connected_peers: usize,
    /// How many Nostr relays are connected.
    pub connected_relays: usize,
}

/// Why the radio is not running, as far as the mesh can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RadioDown {
    /// The adapter reported itself unpowered: the user can fix this.
    BluetoothOff,
    /// Bluetooth is not known to be off, yet the radio is down — it would not
    /// start, or the mesh refused to start it (see the `Degraded` reason). Saying
    /// "Bluetooth off" here sends the user to a switch that is already on.
    Unavailable,
}

impl MeshStatus {
    /// `None` while the radio runs.
    #[must_use]
    pub fn radio_down(&self) -> Option<RadioDown> {
        if self.radio_running {
            None
        } else if !self.adapter_powered {
            Some(RadioDown::BluetoothOff)
        } else {
            Some(RadioDown::Unavailable)
        }
    }
}

#[cfg(test)]
mod radio_down_tests {
    use super::*;

    #[test]
    fn only_an_unpowered_adapter_reads_as_bluetooth_off() {
        let up = MeshStatus {
            radio_running: true,
            adapter_powered: true,
            ..Default::default()
        };
        assert_eq!(up.radio_down(), None);
        let off = MeshStatus {
            radio_running: false,
            adapter_powered: false,
            ..Default::default()
        };
        assert_eq!(off.radio_down(), Some(RadioDown::BluetoothOff));
        // The S9 case: Bluetooth on, radio refused to start over unreadable pins.
        let refused = MeshStatus {
            radio_running: false,
            adapter_powered: true,
            ..Default::default()
        };
        assert_eq!(refused.radio_down(), Some(RadioDown::Unavailable));
    }
}

/// Everything the service surfaces.
#[derive(Clone, Debug, PartialEq)]
pub enum MeshEvent {
    /// An announcement tried to replace a pinned identity key.
    IdentityKeyChanged { peer: PeerId },
    /// A message arrived, or one we sent was mirrored into the timeline.
    Message(Box<MeshMessage>),
    /// Delivery state changed for a message this device sent.
    ///
    /// `peer` is `None` for a relay-side courier drop, which resolves against a
    /// rotating recipient tag rather than a peer ID — the whole point of the tag
    /// is that the publisher does not have to name the recipient.
    Delivery {
        message_id: String,
        peer: Option<PeerId>,
        state: MeshDelivery,
    },
    /// The peer list changed. Carries the whole list because the alternative —
    /// per-peer deltas — makes the UI reconstruct state it would then have to
    /// keep in sync itself.
    Peers(Vec<MeshPeer>),
    /// Transport state changed.
    Status(MeshStatus),
    /// A geohash channel was joined or left.
    GeohashJoined(String),
    /// A geohash channel was left.
    GeohashLeft(String),
    /// A signed channel message advertised a nearby public channel.
    ChannelObserved(String),
    /// Something went wrong that the user should know about but that did not
    /// stop the service.
    Degraded(String),
}
