//! Which physical connection belongs to which peer identity.
//!
//! Ported from `BLELinkStateStore.swift`, `BLEPeerRegistry.swift` and
//! `BLEIngressLinkRegistry.swift`:
//!
//! - [`LinkStateStore`] — discovery hints and Noise-confirmed link↔peer bindings.
//! - [`PeerRegistry`] — per-peer records, last-seen, staleness eviction.
//! - [`IngressRegistry`] — which link a packet arrived on, for split-horizon.
//!
//! # Why the binding is security-critical
//!
//! A [`crate::link::LinkId`] is a physical connection. A [`chatter_mesh::ids::PeerId`] is a claimed identity. The
//! mapping between them is the only thing that makes "this packet came from Bob"
//! mean anything, and it is established from data an attacker in radio range can
//! replay.
//!
//! bitchat signs its announces but **excludes the TTL from the signature**,
//! because relays have to decrement it. So "this announce arrived directly, one
//! hop, from the device on the other end of this link" is an *unauthenticated*
//! property: anyone can capture Bob's announce, restore its TTL to the origin
//! value and emit it on their own link, and it will verify and look direct.
//!
//! Direct announces can record a discovery hint. Hints allow public sync and
//! an attempt to open Noise without affecting preferred routing, link presence
//! or registry retention. The runtime promotes a hint only after Noise proof.
//!
//! # Sans-IO
//!
//! No clocks and no async. Every time-dependent entry point takes
//! `now_unix_millis`. Elapsed times are computed with `saturating_sub`, so a
//! caller whose clock steps backwards sees "zero elapsed" and keeps a peer
//! reachable.

mod ingress;
mod link_state;
mod peer;

pub use ingress::{INGRESS_RECORD_LIFETIME_MS, IngressRecord, IngressRegistry};
pub use link_state::{BindOutcome, LinkPresence, LinkState, LinkStateStore};
pub use peer::{
    AnnounceUpdate, ConnectivityChanges, MAX_PINNED_IDENTITIES, MalformedPins,
    PEER_INACTIVITY_TIMEOUT_MS, PeerRecord, PeerRegistry, PinnedIdentity,
    REACHABILITY_RETENTION_UNVERIFIED_MS, REACHABILITY_RETENTION_VERIFIED_MS, RemovedPeer,
    reachability_retention_ms,
};

#[cfg(test)]
use crate::link::{LinkId, LinkRole};
#[cfg(test)]
use chatter_mesh::ids::PeerId;
#[cfg(test)]
use chatter_mesh::packet::{BitchatPacket, MessageType};
#[cfg(test)]
use chatter_mesh::tlv::announce::AnnouncementPacket;
#[cfg(test)]
use chatter_mesh::tlv::capabilities::PeerCapabilities;
#[cfg(test)]
use peer::PIN_BLOB_VERSION;
#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests;
