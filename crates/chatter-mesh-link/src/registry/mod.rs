//! Which physical connection belongs to which peer identity.
//!
//! Ported from `BLELinkStateStore.swift`, `BLEPeerRegistry.swift` and
//! `BLEIngressLinkRegistry.swift`, together with the rebind guards that live in
//! `BLEService.rebindLinkAfterVerifiedDirectAnnounce`. Three types, one concern
//! each:
//!
//! - [`LinkStateStore`] — link↔peer bindings, both directions, plus the guarded
//!   rebind path.
//! - [`PeerRegistry`] — per-peer records, last-seen, staleness eviction.
//! - [`IngressRegistry`] — which link a packet arrived on, for split-horizon.
//!
//! # Why the binding is security-critical
//!
//! A [`LinkId`] is a physical connection. A [`PeerId`] is a claimed identity. The
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
//! Every guard here exists to bound what that forgery buys. The design rule is
//! that the type system, not caller discipline, enforces them:
//!
//! - An unverified announce reaches only [`LinkStateStore::bind_if_unbound`],
//!   which refuses to move an already-bound link.
//! - Moving a bound link requires a [`VerifiedDirectAnnounce`], which cannot be
//!   constructed without asserting both signature verification and directness.
//! - Even then [`LinkStateStore::rebind_after_verified_direct`] applies the
//!   containment checks: no stealing an identity that already owns a live link,
//!   and at most one rebind per link per [`LINK_REBIND_COOLDOWN_MS`] window.
//!
//! # Sans-IO
//!
//! No clocks and no async. Every time-dependent entry point takes
//! `now_unix_millis`. Elapsed times are computed with `saturating_sub`, so a
//! caller whose clock steps backwards sees "zero elapsed" — which refuses a
//! rebind and keeps a peer reachable, the conservative direction in both cases.

mod ingress;
mod link_state;
mod peer;

pub use ingress::{IngressRecord, IngressRegistry, INGRESS_RECORD_LIFETIME_MS};
pub use link_state::{
    is_direct_announce, BindOutcome, LinkPresence, LinkState, LinkStateStore, Rebind,
    RebindOutcome, RebindRefusal, VerifiedDirectAnnounce, LINK_REBIND_COOLDOWN_MS,
};
pub use peer::{
    reachability_retention_ms, AnnounceUpdate, ConnectivityChanges, MalformedPins, PeerRecord,
    PeerRegistry, PinnedIdentity, RemovedPeer, MAX_PINNED_IDENTITIES, PEER_INACTIVITY_TIMEOUT_MS,
    REACHABILITY_RETENTION_UNVERIFIED_MS, REACHABILITY_RETENTION_VERIFIED_MS,
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
