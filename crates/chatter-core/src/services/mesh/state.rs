//! The shared, synchronously-lockable mesh state.
//!
//! Everything in `chatter-mesh-link` is sans-IO policy that owns no clock and no
//! task: a scheduler, a duty cycle, two registries, a guard, a set of buffers.
//! They all have to live somewhere with a lifetime longer than one radio event,
//! and several of them have to be readable from a *synchronous* context —
//! [`RouterTransport`](chatter_mesh_store::RouterTransport)'s predicates take
//! `&self` and return immediately, so they cannot await a lock.
//!
//! Hence `parking_lot::Mutex` rather than a tokio one. The critical sections are
//! all pure computation over in-memory maps; nothing inside one does IO, and
//! nothing holds the guard across an `.await`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use chatter_mesh::dedup::MessageDeduplicator;
use chatter_mesh::fragment::FragmentAssembler;
use chatter_mesh::ids::PeerId;
use chatter_mesh_link::backpressure::{InboundAssembler, OutboundWriteBuffer};
use chatter_mesh_link::duty::ScanDutyCycle;
use chatter_mesh_link::fanout::LinkCandidate;
use chatter_mesh_link::guard::SelfBroadcastTracker;
use chatter_mesh_link::planner::DirectedRelaySpool;
use chatter_mesh_link::registry::{IngressRegistry, LinkStateStore, PeerRecord, PeerRegistry};
use chatter_mesh_link::scheduler::ConnectionScheduler;
use chatter_mesh_link::LinkId;
use chatter_mesh_store::PeerSnapshot;
use parking_lot::Mutex;

use super::events::{MeshPeer, MeshStatus};
use super::session::SessionManager;

/// Milliseconds since the Unix epoch.
///
/// Every policy type in the link crate takes `now_ms` as a parameter, which is
/// what makes them testable; this is the single place the real clock is read.
#[must_use]
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The mutable half, behind the lock.
pub struct LinkStateInner {
    pub links: LinkStateStore,
    pub peers: PeerRegistry,
    pub ingress: IngressRegistry,
    pub scheduler: ConnectionScheduler,
    pub duty: ScanDutyCycle,
    pub dedup: MessageDeduplicator,
    pub fragments: FragmentAssembler,
    pub assembler: InboundAssembler,
    pub writes: OutboundWriteBuffer,
    pub self_broadcasts: SelfBroadcastTracker,
    pub spool: DirectedRelaySpool,
    pub sessions: SessionManager,
    /// Our one-time prekeys and peers' verified bundles.
    pub prekeys: super::prekeys::PrekeyState,
    /// Negotiated write ceiling per link. Not derivable from the link store:
    /// it is negotiated per connection and arrives on the `Connected` event.
    pub max_write: BTreeMap<LinkId, usize>,
    /// Which device address a link was dialled at, so a disconnect can be
    /// reported back to the scheduler, which is keyed by address rather than by
    /// link.
    pub address_of: BTreeMap<LinkId, String>,
    /// Addresses we have dialled and not yet resolved.
    pub connecting: BTreeSet<String>,
    /// Messages waiting for a Noise session to finish, keyed by peer.
    pub pending_dms: BTreeMap<PeerId, Vec<PendingDm>>,
    pub last_announce_ms: u64,
    /// An announce was requested inside the floor and is owed at its expiry.
    pub announce_deferred: bool,
    pub last_traffic_ms: u64,
    pub maintenance_cycle: u32,
    pub adapter_powered: bool,
    pub radio_running: bool,
    /// Whether the app is in the foreground. The duty cycle scans continuously
    /// in the foreground and intermittently otherwise.
    pub app_is_active: bool,
    /// Whether to duty-cycle the scan at all.
    pub duty_enabled: bool,
}

/// A private message held until its Noise session is established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDm {
    pub message_id: String,
    pub content: String,
    pub queued_at_ms: u64,
}

impl Default for LinkStateInner {
    fn default() -> Self {
        Self {
            links: LinkStateStore::new(),
            peers: PeerRegistry::new(),
            ingress: IngressRegistry::new(),
            scheduler: ConnectionScheduler::new(),
            duty: ScanDutyCycle::new(),
            dedup: MessageDeduplicator::default(),
            fragments: FragmentAssembler::new(),
            assembler: InboundAssembler::new(),
            writes: OutboundWriteBuffer::new(),
            self_broadcasts: SelfBroadcastTracker::new(),
            spool: DirectedRelaySpool::new(),
            sessions: SessionManager::new(),
            prekeys: super::prekeys::PrekeyState::default(),
            max_write: BTreeMap::new(),
            address_of: BTreeMap::new(),
            connecting: BTreeSet::new(),
            pending_dms: BTreeMap::new(),
            last_announce_ms: 0,
            announce_deferred: false,
            last_traffic_ms: 0,
            maintenance_cycle: 0,
            adapter_powered: true,
            radio_running: false,
            app_is_active: true,
            duty_enabled: true,
        }
    }
}

/// Everything the BLE half of the mesh knows, shared between the radio loop and
/// the router's transport view of it.
pub struct MeshState {
    local_peer: PeerId,
    inner: Mutex<LinkStateInner>,
}

impl MeshState {
    #[must_use]
    pub fn new(local_peer: PeerId) -> Self {
        Self {
            local_peer,
            inner: Mutex::new(LinkStateInner::default()),
        }
    }

    #[must_use]
    pub fn local_peer(&self) -> &PeerId {
        &self.local_peer
    }

    /// Run `f` under the lock.
    ///
    /// Never call this with anything that awaits or does IO: the guard is not
    /// `Send`, and the whole point of the synchronous lock is that critical
    /// sections stay short enough that a radio event never waits on a database.
    pub fn with<R>(&self, f: impl FnOnce(&mut LinkStateInner) -> R) -> R {
        let mut guard = self.inner.lock();
        f(&mut guard)
    }

    /// A live link binding to this peer exists.
    ///
    /// **Forgeable** — see the `RouterTransport` docs. Link bindings heal on
    /// announces whose "directness" rides on an unsigned TTL.
    #[must_use]
    pub fn is_connected(&self, peer: &PeerId) -> bool {
        self.with(|state| !state.links.links_to(peer).is_empty())
    }

    /// The mesh's freshness heuristic: seen recently enough to be worth a send.
    #[must_use]
    pub fn is_reachable(&self, peer: &PeerId) -> bool {
        let now = now_millis();
        self.with(|state| state.peers.is_reachable(peer, now))
    }

    /// A write to this peer will actually leave the device: a link bound to it
    /// is up *and writable*.
    ///
    /// Writability is not implied by the binding. `LinkEvent::Connected` is only
    /// emitted once a link can carry a write, and the store tracks it from
    /// there; a link whose characteristic vanished is bound but not writable,
    /// and a send on it is silently dropped by the controller.
    #[must_use]
    pub fn can_write_to(&self, peer: &PeerId) -> bool {
        self.with(|state| {
            let links = state.links.links_to(peer);
            links.iter().any(|link| state.links.is_writable(link))
        })
    }

    /// An established Noise session exists — the only signal strong enough to
    /// send against without retaining a copy.
    #[must_use]
    pub fn has_session(&self, peer: &PeerId) -> bool {
        self.with(|state| state.sessions.has_session(peer))
    }

    /// Everything the courier selector needs about every peer we know.
    #[must_use]
    pub fn peer_snapshots(&self) -> Vec<PeerSnapshot> {
        self.with(|state| {
            let records: Vec<PeerRecord> = state.peers.records().cloned().collect();
            records
                .into_iter()
                .map(|record| {
                    // Prefer the key the announce advertised; fall back to the
                    // one a completed handshake proved. They agree in the normal
                    // case, and the announce is the only source before a session
                    // exists.
                    let announced = record
                        .noise_public_key
                        .as_deref()
                        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok());
                    let noise_public_key =
                        announced.or_else(|| state.sessions.remote_static(&record.peer));
                    PeerSnapshot {
                        is_connected: !state.links.links_to(&record.peer).is_empty(),
                        peer: record.peer,
                        noise_public_key,
                        is_verified: record.verified_nickname,
                    }
                })
                .collect()
        })
    }

    /// The peer list, shaped for the UI.
    #[must_use]
    pub fn peers(&self) -> Vec<MeshPeer> {
        let now = now_millis();
        self.with(|state| {
            let records: Vec<PeerRecord> = state.peers.records().cloned().collect();
            let mut peers: Vec<MeshPeer> = records
                .into_iter()
                .map(|record| MeshPeer {
                    connected: !state.links.links_to(&record.peer).is_empty(),
                    reachable: state.peers.is_reachable(&record.peer, now),
                    verified: record.verified_nickname,
                    has_session: state.sessions.has_session(&record.peer),
                    fingerprint: record
                        .noise_public_key
                        .as_deref()
                        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
                        .map(|key| chatter_mesh_noise::fingerprint_of(&key))
                        .or_else(|| state.sessions.fingerprint(&record.peer)),
                    peer: record.peer,
                    nickname: record.nickname,
                })
                .collect();
            disambiguate_nicknames(&mut peers);
            // Deterministic order so the UI never re-sorts, and so two renders of
            // an unchanged mesh are identical.
            peers.sort_by(|a, b| a.peer.cmp(&b.peer));
            peers
        })
    }

    /// Transport status, minus the relay count, which the Nostr half owns.
    #[must_use]
    pub fn status(&self, connected_relays: usize) -> MeshStatus {
        self.with(|state| {
            let connected_links = state
                .links
                .links()
                .filter(|(_, link_state)| link_state.writable)
                .count();
            MeshStatus {
                radio_running: state.radio_running,
                adapter_powered: state.adapter_powered,
                connected_links,
                connected_peers: state.peers.connected_count(),
                connected_relays,
            }
        })
    }

    /// Every writable link, as fanout candidates.
    ///
    /// A link that is up but not yet writable is deliberately excluded rather
    /// than included with a zero write ceiling: the planner would size fragments
    /// against it and the writes would be dropped by the controller.
    #[must_use]
    pub fn link_candidates(&self) -> Vec<LinkCandidate> {
        self.with(|state| link_candidates_locked(state))
    }
}

/// Fanout candidates, computed with the lock already held.
pub fn link_candidates_locked(state: &LinkStateInner) -> Vec<LinkCandidate> {
    let preferred = state.links.preferred_links().clone();
    state
        .links
        .links()
        .filter(|(link, link_state)| link_state.writable && state.max_write.contains_key(*link))
        .map(|(link, link_state)| {
            let max_write = state.max_write.get(link).copied().unwrap_or(0);
            let mut candidate = LinkCandidate::new(link.clone(), link_state.role, max_write);
            if let Some(peer) = link_state.peer.clone() {
                let is_preferred = preferred.get(&peer) == Some(link);
                candidate = candidate.bound_to(peer);
                if is_preferred {
                    candidate = candidate.preferred();
                }
            }
            candidate
        })
        .collect()
}

/// What the Nostr half knows, readable synchronously for the same reason.
pub struct NostrState {
    inner: Mutex<NostrStateInner>,
}

#[derive(Default)]
pub struct NostrStateInner {
    /// Relays currently connected. `can_deliver_promptly` is false whenever this
    /// is empty: a publish with no socket only joins a queue.
    pub connected_relays: BTreeSet<String>,
    /// Geohash cells we are subscribed to.
    pub joined: BTreeSet<String>,
    /// Which cell a `nostr_`-prefixed DM peer was last seen in. A geohash DM is
    /// addressed from the identity of the cell the conversation happens in, so
    /// without this there is no sender key to sign with.
    pub dm_origin: BTreeMap<PeerId, String>,
    /// Display names seen for geohash participants, keyed by their `nostr:` or
    /// `nostr_` peer ID.
    pub nicknames: BTreeMap<PeerId, String>,
}

impl Default for NostrState {
    fn default() -> Self {
        Self::new()
    }
}

impl NostrState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(NostrStateInner::default()),
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut NostrStateInner) -> R) -> R {
        let mut guard = self.inner.lock();
        f(&mut guard)
    }

    /// Whether any relay is connected.
    #[must_use]
    pub fn any_relay_connected(&self) -> bool {
        self.with(|state| !state.connected_relays.is_empty())
    }

    #[must_use]
    pub fn connected_relay_count(&self) -> usize {
        self.with(|state| state.connected_relays.len())
    }

    /// Whether we know how to address this peer over Nostr at all.
    ///
    /// True exactly for the geohash DM namespace with a well-formed 64-hex
    /// pubkey — that pubkey *is* the gift-wrap recipient, so nothing else is
    /// required to seal to them.
    #[must_use]
    pub fn knows_peer(&self, peer: &PeerId) -> bool {
        recipient_pubkey(peer).is_some()
    }

    /// The cell a DM peer was last seen in.
    #[must_use]
    pub fn dm_origin(&self, peer: &PeerId) -> Option<String> {
        self.with(|state| state.dm_origin.get(peer).cloned())
    }

    /// Record where a geohash participant was seen, so DMs to them can be signed
    /// with the matching cell identity.
    pub fn note_participant(&self, peer: PeerId, cell: &str, nickname: Option<String>) {
        self.with(|state| {
            state.dm_origin.insert(peer.clone(), cell.to_string());
            if let Some(nickname) = nickname {
                state.nicknames.insert(peer, nickname);
            }
        });
    }

    #[must_use]
    pub fn nickname(&self, peer: &PeerId) -> Option<String> {
        self.with(|state| state.nicknames.get(peer).cloned())
    }

    #[must_use]
    pub fn joined_cells(&self) -> Vec<String> {
        self.with(|state| state.joined.iter().cloned().collect())
    }
}

/// The 64-hex secp256k1 pubkey a Nostr DM peer ID carries, if it carries one.
///
/// The `nostr_` namespace is the geohash DM one; `nostr:` is geohash *chat*,
/// which is a broadcast identity and never a DM recipient.
#[must_use]
pub fn recipient_pubkey(peer: &PeerId) -> Option<String> {
    if !peer.is_geo_dm() {
        return None;
    }
    let bare = peer.bare();
    let is_hex = bare.len() == 64 && bare.bytes().all(|b| b.is_ascii_hexdigit());
    is_hex.then(|| bare.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh_link::LinkRole;

    fn peer(s: &str) -> PeerId {
        PeerId::from_str(s)
    }

    #[test]
    fn colliding_nicknames_use_stable_fingerprint_suffixes() {
        let state = MeshState::new(peer("aaaaaaaaaaaaaaaa"));
        let first = peer("1111111111111111");
        let second = peer("2222222222222222");
        state.with(|s| {
            for (id, key) in [(&first, [1u8; 32]), (&second, [2u8; 32])] {
                let announce = chatter_mesh::tlv::announce::AnnouncementPacket::new(
                    "Alice".into(), key.to_vec(), vec![3; 32], None, None, None,
                );
                s.peers.upsert_verified_announce(id, &announce, true, now_millis());
            }
        });
        let peers = state.peers();
        assert_eq!(peers[0].nickname, format!("Alice#{}", &chatter_mesh_noise::fingerprint_of(&[1; 32])[..4]));
        assert_eq!(peers[1].nickname, format!("Alice#{}", &chatter_mesh_noise::fingerprint_of(&[2; 32])[..4]));
        state.with(|s| { s.peers.remove(&second); });
        assert_eq!(state.peers()[0].nickname, "Alice");
    }

    #[test]
    fn a_fresh_state_knows_nothing() {
        let state = MeshState::new(peer("aaaaaaaaaaaaaaaa"));
        let other = peer("bbbbbbbbbbbbbbbb");
        assert!(!state.is_connected(&other));
        assert!(!state.is_reachable(&other));
        assert!(!state.can_write_to(&other));
        assert!(!state.has_session(&other));
        assert!(state.peers().is_empty());
        assert!(state.peer_snapshots().is_empty());
        assert!(state.link_candidates().is_empty());
    }

    #[test]
    fn a_bound_link_is_connected_but_only_writable_once_it_says_so() {
        let state = MeshState::new(peer("aaaaaaaaaaaaaaaa"));
        let other = peer("bbbbbbbbbbbbbbbb");
        let link = LinkId::new("/link/1");

        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.bind_if_unbound(&link, &other);
        });
        assert!(state.is_connected(&other));
        assert!(!state.can_write_to(&other), "binding alone is not writability");

        state.with(|inner| {
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });
        assert!(state.can_write_to(&other));
        assert_eq!(state.link_candidates().len(), 1);
    }

    #[test]
    fn candidates_exclude_links_with_no_negotiated_write_ceiling() {
        // Guessing a ceiling means oversized packets the controller drops in
        // silence, so a link that never reported one is not a candidate.
        let state = MeshState::new(peer("aaaaaaaaaaaaaaaa"));
        let link = LinkId::new("/link/1");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Peripheral);
            inner.links.set_writable(&link, true);
        });
        assert!(state.link_candidates().is_empty());
    }

    #[test]
    fn status_reports_only_writable_links() {
        let state = MeshState::new(peer("aaaaaaaaaaaaaaaa"));
        let up = LinkId::new("/link/up");
        let half = LinkId::new("/link/half");
        state.with(|inner| {
            inner.radio_running = true;
            inner.links.link_up(up.clone(), LinkRole::Central);
            inner.links.set_writable(&up, true);
            inner.links.link_up(half.clone(), LinkRole::Central);
        });
        let status = state.status(2);
        assert!(status.radio_running);
        assert_eq!(status.connected_links, 1);
        assert_eq!(status.connected_relays, 2);
    }

    #[test]
    fn nostr_prompt_delivery_needs_a_relay() {
        let state = NostrState::new();
        assert!(!state.any_relay_connected());
        state.with(|inner| {
            inner.connected_relays.insert("wss://relay.example".into());
        });
        assert!(state.any_relay_connected());
        assert_eq!(state.connected_relay_count(), 1);
    }

    #[test]
    fn only_the_geo_dm_namespace_yields_a_recipient_key() {
        let key = "a".repeat(64);
        assert_eq!(
            recipient_pubkey(&peer(&format!("nostr_{key}"))),
            Some(key.clone())
        );
        // Geohash *chat* identities are broadcast, not DM recipients.
        assert!(recipient_pubkey(&peer(&format!("nostr:{key}"))).is_none());
        // A bare mesh peer has no Nostr key.
        assert!(recipient_pubkey(&peer("abcdef0123456789")).is_none());
        // Wrong length, and non-hex, are both rejected.
        assert!(recipient_pubkey(&peer("nostr_abcd")).is_none());
        assert!(recipient_pubkey(&peer(&format!("nostr_{}", "z".repeat(64)))).is_none());
    }

    #[test]
    fn participants_are_remembered_with_the_cell_they_were_seen_in() {
        let state = NostrState::new();
        let key = format!("nostr_{}", "b".repeat(64));
        state.note_participant(peer(&key), "u4pruy", Some("wanderer".into()));
        assert_eq!(state.dm_origin(&peer(&key)).as_deref(), Some("u4pruy"));
        assert_eq!(state.nickname(&peer(&key)).as_deref(), Some("wanderer"));
        assert!(state.knows_peer(&peer(&key)));
    }
}


// A fingerprint belongs to the identity, so a collision suffix stays stable
// when peer-list ordering changes.
fn disambiguate_nicknames(peers: &mut [MeshPeer]) {
    let mut counts = std::collections::BTreeMap::new();
    for peer in peers.iter() {
        *counts.entry(peer.nickname.clone()).or_insert(0usize) += 1;
    }
    for peer in peers {
        if counts.get(&peer.nickname).copied().unwrap_or(0) > 1 {
            let fingerprint = peer.fingerprint.as_deref().unwrap_or(peer.peer.as_str());
            let suffix: String = fingerprint.chars().filter(|c| c.is_ascii_hexdigit()).take(4).collect();
            peer.nickname = format!("{}#{}", peer.nickname, suffix.to_lowercase());
        }
    }
}
