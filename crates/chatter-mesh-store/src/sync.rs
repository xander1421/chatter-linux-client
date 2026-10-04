//! Gossip sync: reconciling public history with whoever we meet.
//!
//! Ported from `Sync/GossipSyncManager.swift` (777 lines),
//! `Sync/RequestSyncManager.swift` and `Sync/SyncResponseRateLimiter.swift`, with
//! the constants from `Services/TransportConfig.swift`.
//!
//! Two peers that meet exchange a compact set filter describing what each already
//! holds, and each sends back only the difference. That is the whole mechanism, and
//! it is what lets a device that walks between two partitions carry a room's recent
//! messages across: the history it heard on one side is offered to the other side
//! the moment a link comes up.
//!
//! # Sans-IO
//!
//! Nothing here reads a clock, starts a timer, or sends a packet. Every entry point
//! takes `now_unix_secs` and returns the packets it wants sent as [`SyncSend`]
//! values. The runtime layer owns the cadence (see [`MAINTENANCE_INTERVAL_SECS`] and
//! [`INITIAL_SYNC_DELAY_SECS`]), the signing key, and the radio.
//!
//! # The security properties, and why each one is shaped the way it is
//!
//! - **`REQUEST_SYNC` is link-local.** Every request this module originates carries
//!   `ttl = 0`, and `chatter_mesh::relay` refuses to forward the type at all — its
//!   very first branch, ahead of even the self-checks. Both halves matter: a peer
//!   can craft a request with TTL headroom hoping to turn every reachable node into
//!   a full-store responder, and the relay side is what stops that. Our side's job
//!   is never to *originate* one with a non-zero TTL.
//!
//! - **Responses are marked RSR with `ttl = 0`.** The RSR flag exempts a packet from
//!   the receiver's ±2-minute freshness check. Without that exemption historical
//!   data could not sync at all: everything worth carrying across a partition is by
//!   definition older than two minutes.
//!
//! - **The exemption applies only to *solicited* responses.** The flag is a header
//!   bit an attacker sets for free, so honouring it on its own would make every
//!   captured packet replayable forever. [`RequestSyncTracker`] records which peers
//!   we actually asked and when; an RSR packet from a peer with no outstanding
//!   request, or one outside the 30-second window, is rejected. This is also why
//!   periodic sync is **unicast per connected peer** rather than broadcast — a
//!   broadcast request cannot attribute the answers it provokes.
//!
//! - **Response rate limiting is per peer**, 8 per 30 seconds. One response can
//!   replay the entire store, so a peer requesting in a tight loop must not be able
//!   to drain everyone's airtime and battery.
//!
//! - **The filter is a compact GCS** — a ~400-byte budget at a 1% false-positive
//!   rate. A false positive means we skip sending something the peer actually lacks.
//!   That is the designed tradeoff, not a defect: the miss is self-healing on the
//!   next round (the filter is rebuilt from a different candidate set each time),
//!   and the alternative — an exact set exchange — costs orders of magnitude more
//!   airtime on a link this slow.
//!
//! # Retention differs per type, and the differences are load-bearing
//!
//! Whole public messages stay sync-able for six hours and their store persists
//! across restarts ([`crate::archive`]), because that is the history a courier
//! device carries. Fragments, file transfers and announces keep a fifteen-minute
//! window: a fragment older than that belongs to a reassembly nobody is still
//! waiting on. Prekey bundles keep a day, since their entire purpose is being
//! reachable while their owner is away. Each type also gets its own sync round and
//! its own capacity, so heavy fragment traffic cannot crowd messages out of the
//! filter.

use std::collections::BTreeMap;

use chatter_mesh::gcs;
use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{BROADCAST_RECIPIENT, BitchatPacket, MessageType, SENDER_ID_LEN};
use chatter_mesh::sync_flags::SyncTypeFlags;
use chatter_mesh::tlv::request_sync::{
    FRAGMENT_ID_LEN, MAX_P, RequestSyncPacket, encode_fragment_id_filter,
};
use chatter_mesh_noise::prekey::PrekeyBundle;

use crate::archive::GossipMessageArchive;
use crate::error::StoreError;

/// How long a sync request stays answerable, in seconds.
///
/// An RSR packet arriving later than this from a peer we did ask is rejected all
/// the same: the window is what stops a captured response being replayed hours
/// later against a request that has long since been served.
pub const RESPONSE_WINDOW_SECS: u64 = 30;

/// Responses one peer may provoke per window.
pub const MAX_RESPONSES_PER_WINDOW: usize = 8;

/// Sliding window for the response rate limit, in seconds.
pub const RESPONSE_RATE_LIMIT_WINDOW_SECS: u64 = 30;

/// Cadence the runtime should call [`GossipSyncManager::maintenance`] at.
///
/// Advisory: nothing here schedules anything. It is stated so the scheduling layer
/// does not have to guess, and so the per-type intervals in [`SyncConfig`] are read
/// against the tick they are actually sampled at.
pub const MAINTENANCE_INTERVAL_SECS: u64 = 30;

/// How long to wait after a peer connects before the first sync request.
///
/// Also advisory. The delay gives the link's own handshake and announce exchange
/// time to finish, so the first request is not competing with them.
pub const INITIAL_SYNC_DELAY_SECS: u64 = 5;

/// Tunables for one sync driver. Defaults are bitchat's shipping values.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncConfig {
    /// Public messages held (and offered) at once.
    pub seen_capacity: usize,
    /// Byte budget for the GCS filter in a request.
    pub gcs_max_bytes: usize,
    /// Target false-positive rate for the filter.
    pub gcs_target_fpr: f64,
    /// Freshness window for fragments, file transfers and announces.
    pub max_message_age_secs: u64,
    /// Freshness window for whole public messages and group messages — the
    /// town-crier window. Must not exceed the receive side's acceptance window, or
    /// packets we serve would be dropped as stale by the peer that asked for them.
    pub public_message_max_age_secs: u64,
    /// Freshness window for prekey bundles.
    pub prekey_bundle_max_age_secs: u64,
    /// How often stale-announce cleanup runs, in seconds.
    pub stale_peer_cleanup_interval_secs: u64,
    /// An announce older than this makes its peer stale.
    pub stale_peer_timeout_secs: u64,
    pub fragment_capacity: usize,
    pub file_transfer_capacity: usize,
    pub group_message_capacity: usize,
    pub prekey_bundle_capacity: usize,
    pub board_capacity: usize,
    /// Per-type round intervals. A zero interval disables that round entirely.
    pub message_sync_interval_secs: u64,
    pub fragment_sync_interval_secs: u64,
    pub file_transfer_sync_interval_secs: u64,
    pub prekey_bundle_sync_interval_secs: u64,
    pub board_sync_interval_secs: u64,
    pub response_rate_limit_max_responses: usize,
    pub response_rate_limit_window_secs: u64,
    /// How long an outstanding request keeps accepting RSR answers.
    pub request_response_window_secs: u64,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            seen_capacity: 1000,
            gcs_max_bytes: 400,
            gcs_target_fpr: 0.01,
            max_message_age_secs: 15 * 60,
            public_message_max_age_secs: 6 * 60 * 60,
            prekey_bundle_max_age_secs: 24 * 60 * 60,
            stale_peer_cleanup_interval_secs: 60,
            stale_peer_timeout_secs: 60,
            fragment_capacity: 600,
            file_transfer_capacity: 200,
            group_message_capacity: 200,
            prekey_bundle_capacity: 200,
            board_capacity: 200,
            message_sync_interval_secs: 15,
            fragment_sync_interval_secs: 30,
            file_transfer_sync_interval_secs: 60,
            prekey_bundle_sync_interval_secs: 60,
            board_sync_interval_secs: 60,
            response_rate_limit_max_responses: MAX_RESPONSES_PER_WINDOW,
            response_rate_limit_window_secs: RESPONSE_RATE_LIMIT_WINDOW_SECS,
            request_response_window_secs: RESPONSE_WINDOW_SECS,
        }
    }
}

/// One packet the sync driver wants sent, and to whom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncSend {
    /// `Some(peer)` for a unicast send — the normal case, and the one that lets the
    /// answers be attributed. `None` is the discovery fallback: a link-local
    /// broadcast used when no peer is connected yet, whose answers therefore cannot
    /// be validated as solicited.
    pub to: Option<PeerId>,
    pub packet: BitchatPacket,
    /// Whether the caller must sign the packet before it goes out.
    ///
    /// True for `REQUEST_SYNC`: it is ours and unsigned. False for a response,
    /// which is another peer's already-signed packet forwarded verbatim — signing
    /// it again would replace its author's signature with ours and make it
    /// unverifiable to everyone downstream.
    pub sign_before_send: bool,
}

impl SyncSend {
    fn request(to: Option<&PeerId>, packet: BitchatPacket) -> Self {
        Self {
            to: to.cloned(),
            packet,
            sign_before_send: true,
        }
    }

    /// Marks a stored packet as a solicited response: `ttl = 0` so it stays on this
    /// link, `is_rsr` so the peer that asked exempts it from the freshness check.
    ///
    /// Neither field is inside the signature preimage
    /// (`BitchatPacket::signing_preimage` clears both), so mutating them here keeps
    /// the original author's signature valid.
    fn response(to: &PeerId, packet: &BitchatPacket) -> Self {
        let mut packet = packet.clone();
        packet.ttl = 0;
        packet.is_rsr = true;
        Self {
            to: Some(to.clone()),
            packet,
            sign_before_send: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Outstanding requests
// ---------------------------------------------------------------------------

/// Tracks which peers we asked for sync, so their answers can be told apart from
/// an attacker's replay.
///
/// Ported from `RequestSyncManager.swift`. The whole point is stated in
/// `docs/REQUEST_SYNC_MANAGER.md`: the RSR flag exempts a packet from timestamp
/// validation, and an exemption an attacker can claim for itself is not an
/// exemption, it is a hole. Only a peer we sent a request to, within the window,
/// gets the exemption.
///
/// One timestamp per peer, not one per request: a single request is answered by
/// many packets, and each of them must pass.
#[derive(Clone, Debug)]
pub struct RequestSyncTracker {
    window_secs: u64,
    /// Peer to the time we last asked it.
    pending: BTreeMap<PeerId, u64>,
}

impl Default for RequestSyncTracker {
    fn default() -> Self {
        Self::new(RESPONSE_WINDOW_SECS)
    }
}

impl RequestSyncTracker {
    pub fn new(window_secs: u64) -> Self {
        Self {
            window_secs,
            pending: BTreeMap::new(),
        }
    }

    /// Records that we are sending a sync request to `peer`.
    ///
    /// A second request to the same peer refreshes the timestamp rather than
    /// stacking, which is what makes the window slide with our own requests
    /// instead of expiring mid-answer.
    pub fn register_request(&mut self, peer: &PeerId, now_unix_secs: u64) {
        self.pending.insert(peer.clone(), now_unix_secs);
    }

    /// Whether an inbound packet is a valid answer to something we asked for.
    ///
    /// A non-RSR packet is never "a response" — it takes the ordinary freshness
    /// path and this returns false for it, which is not a rejection.
    pub fn is_valid_response(&self, peer: &PeerId, is_rsr: bool, now_unix_secs: u64) -> bool {
        if !is_rsr {
            return false;
        }
        let Some(requested_at) = self.pending.get(peer) else {
            // Unsolicited RSR: a peer we never asked, claiming the exemption.
            return false;
        };
        // Saturating: a clock that jumped backwards makes the age zero rather than
        // wrapping to something enormous, so a legitimate answer survives it.
        now_unix_secs.saturating_sub(*requested_at) <= self.window_secs
    }

    /// Drops requests whose window has closed.
    pub fn cleanup(&mut self, now_unix_secs: u64) {
        let window = self.window_secs;
        self.pending
            .retain(|_, requested_at| now_unix_secs.saturating_sub(*requested_at) <= window);
    }

    /// The peers we have outstanding sync requests to.
    ///
    /// Diagnostic only. An unsolicited-response rejection while this is non-empty
    /// means the key we registered and the sender we looked up disagree, which
    /// the count alone cannot tell you.
    pub fn pending_peers(&self) -> Vec<PeerId> {
        self.pending.keys().cloned().collect()
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

// ---------------------------------------------------------------------------
// Response rate limiting
// ---------------------------------------------------------------------------

/// Sliding-window limiter for `REQUEST_SYNC` responses, per peer.
///
/// Ported from `SyncResponseRateLimiter.swift`. A single response pass can replay
/// the entire gossip store, so a peer that requests in a tight loop must not be able
/// to drain the airtime and battery of everyone in radio range. Legitimate peers
/// send at most a few requests per maintenance tick — one per due type schedule,
/// plus the initial sync — so eight per thirty seconds is generous.
#[derive(Clone, Debug)]
pub struct SyncResponseRateLimiter {
    max_responses: usize,
    window_secs: u64,
    history: BTreeMap<PeerId, Vec<u64>>,
}

impl Default for SyncResponseRateLimiter {
    fn default() -> Self {
        Self::new(MAX_RESPONSES_PER_WINDOW, RESPONSE_RATE_LIMIT_WINDOW_SECS)
    }
}

impl SyncResponseRateLimiter {
    pub fn new(max_responses: usize, window_secs: u64) -> Self {
        Self {
            // A zero budget would mute sync entirely; Swift clamps the same way.
            max_responses: max_responses.max(1),
            window_secs,
            history: BTreeMap::new(),
        }
    }

    /// Whether `peer` is still under its budget — and, if so, records the response.
    pub fn should_respond(&mut self, peer: &PeerId, now_unix_secs: u64) -> bool {
        let cutoff = now_unix_secs.saturating_sub(self.window_secs);
        let recent = self.history.entry(peer.clone()).or_default();
        // Inclusive of the cutoff, matching Swift's `>=`: an entry exactly one
        // window old still counts against the budget.
        recent.retain(|at| *at >= cutoff);
        if recent.len() >= self.max_responses {
            return false;
        }
        recent.push(now_unix_secs);
        true
    }

    /// Drops history outside the window so departed peers do not accumulate.
    pub fn prune(&mut self, now_unix_secs: u64) {
        let cutoff = now_unix_secs.saturating_sub(self.window_secs);
        self.history.retain(|_, times| {
            times.retain(|at| *at >= cutoff);
            !times.is_empty()
        });
    }

    /// Responses recorded for `peer` inside the current window.
    pub fn recorded(&self, peer: &PeerId) -> usize {
        self.history.get(peer).map_or(0, Vec::len)
    }
}

// ---------------------------------------------------------------------------
// The per-type packet stores
// ---------------------------------------------------------------------------

/// Bounded insertion-ordered packet store, keyed by gossip-sync packet ID.
///
/// FIFO by *first* insertion: re-seeing a packet replaces the stored copy (relays
/// differ in ttl) without moving it up the queue, so a packet endlessly re-relayed
/// cannot pin itself in the store and starve newer traffic.
#[derive(Clone, Debug, Default)]
struct PacketStore {
    packets: BTreeMap<[u8; 16], BitchatPacket>,
    order: Vec<[u8; 16]>,
}

impl PacketStore {
    fn insert(&mut self, id: [u8; 16], packet: BitchatPacket, capacity: usize) {
        if capacity == 0 {
            return;
        }
        if let std::collections::btree_map::Entry::Occupied(mut e) = self.packets.entry(id) {
            e.insert(packet);
            return;
        }
        self.packets.insert(id, packet);
        self.order.push(id);
        while self.order.len() > capacity {
            if self.order.is_empty() {
                break;
            }
            let victim = self.order.remove(0);
            self.packets.remove(&victim);
        }
    }

    fn all_packets(&self, is_fresh: impl Fn(&BitchatPacket) -> bool) -> Vec<&BitchatPacket> {
        self.order
            .iter()
            .filter_map(|id| self.packets.get(id))
            .filter(|packet| is_fresh(packet))
            .collect()
    }

    fn remove_where(&mut self, should_remove: impl Fn(&BitchatPacket) -> bool) {
        let previous = core::mem::take(&mut self.order);
        let mut next = Vec::with_capacity(previous.len());
        for id in previous {
            let Some(packet) = self.packets.get(&id) else {
                // Key with no packet: drop it from the order, as Swift does.
                continue;
            };
            if should_remove(packet) {
                self.packets.remove(&id);
            } else {
                next.push(id);
            }
        }
        self.order = next;
    }

    fn remove_expired(&mut self, is_fresh: impl Fn(&BitchatPacket) -> bool) {
        self.remove_where(|packet| !is_fresh(packet));
    }

    fn len(&self) -> usize {
        self.packets.len()
    }
}

// ---------------------------------------------------------------------------
// Freshness
// ---------------------------------------------------------------------------

/// The retention window for a packet type, in seconds.
///
/// Free function rather than a method so callers can hold a `&SyncConfig` while
/// mutating a store — the borrow checker sees the two fields as disjoint, where a
/// `&self` method would conflict.
fn max_age_secs(config: &SyncConfig, msg_type: u8) -> u64 {
    match MessageType::from_u8(msg_type) {
        // Group messages share the whole-message window: members off the mesh for a
        // while should backfill their crew's history like public chat does.
        Some(MessageType::Message | MessageType::GroupMessage) => {
            config.public_message_max_age_secs
        }
        Some(MessageType::PrekeyBundle) => config.prekey_bundle_max_age_secs,
        _ => config.max_message_age_secs,
    }
}

/// Whether a packet is still inside its type's retention window.
///
/// The boundary is inclusive: a packet exactly `max_age` old is still fresh, which
/// is Swift's `packet.timestamp >= cutoffMs`.
///
/// A device whose clock says the epoch has barely started cannot compute a cutoff,
/// so everything is accepted rather than everything being purged — Swift's explicit
/// `guard nowMs >= ageThresholdMs else { return true }`, reproduced here by the
/// saturating subtraction (the cutoff floors at zero and every timestamp clears it).
fn packet_is_fresh(config: &SyncConfig, packet: &BitchatPacket, now_unix_secs: u64) -> bool {
    let now_ms = now_unix_secs.saturating_mul(1000);
    let window_ms = max_age_secs(config, packet.msg_type).saturating_mul(1000);
    packet.timestamp >= now_ms.saturating_sub(window_ms)
}

/// Whether an announce still counts its peer as present.
///
/// Separate from [`packet_is_fresh`] and much shorter: presence is a claim about
/// *now*, so it uses the stale-peer timeout rather than the retention window.
fn announcement_is_fresh(config: &SyncConfig, packet: &BitchatPacket, now_unix_secs: u64) -> bool {
    if config.stale_peer_timeout_secs == 0 {
        return true;
    }
    let now_ms = now_unix_secs.saturating_mul(1000);
    let window_ms = config.stale_peer_timeout_secs.saturating_mul(1000);
    packet.timestamp >= now_ms.saturating_sub(window_ms)
}

/// Swift's `isBroadcastRecipient`: an absent recipient field counts as broadcast,
/// an all-`0xFF` recipient counts as broadcast, anything else is directed.
///
/// Deliberately not [`BitchatPacket::is_broadcast`], which is false for an absent
/// recipient — a distinction that matters for routing but not here, and using it
/// would silently drop every real public message (they carry no recipient at all).
fn is_broadcast_recipient(packet: &BitchatPacket) -> bool {
    match packet.recipient_id {
        None => true,
        Some(recipient) => recipient == BROADCAST_RECIPIENT,
    }
}

// ---------------------------------------------------------------------------
// Scheduling
// ---------------------------------------------------------------------------

/// One periodic round: which types it covers and how often it runs.
///
/// One request per due schedule rather than a single union filter, so each type
/// group gets the whole GCS byte budget and its own since-cursor. A union would let
/// heavy fragment traffic crowd messages out of the filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SyncSchedule {
    types: SyncTypeFlags,
    interval_secs: u64,
    /// `None` means "never sent", which is due immediately.
    last_sent_secs: Option<u64>,
}

impl SyncSchedule {
    fn is_due(&self, now_unix_secs: u64) -> bool {
        match self.last_sent_secs {
            None => true,
            Some(last) => now_unix_secs.saturating_sub(last) >= self.interval_secs,
        }
    }
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// Gossip-sync driver: carries recent public traffic and reconciles it with peers.
pub struct GossipSyncManager {
    my_sender_id: [u8; SENDER_ID_LEN],
    config: SyncConfig,
    requests: RequestSyncTracker,
    rate_limiter: SyncResponseRateLimiter,

    messages: PacketStore,
    fragments: PacketStore,
    file_transfers: PacketStore,
    group_messages: PacketStore,
    /// Latest announce per sender.
    latest_announce_by_peer: BTreeMap<PeerId, BitchatPacket>,
    /// Latest prekey bundle per *owner*.
    ///
    /// Keyed by the bundle's own authenticated identity — its Noise static key —
    /// and never by the packet's sender ID. One valid bundle re-broadcast under a
    /// hundred fabricated sender IDs must produce one entry, not a hundred, or the
    /// per-owner cap becomes a trivial way to starve legitimate bundles.
    ///
    /// Unlike announces these are *not* dropped when their peer goes stale: their
    /// whole purpose is reaching a sender while the owner is away.
    latest_prekey_bundle_by_owner: BTreeMap<PeerId, BitchatPacket>,
    /// Board posts, injected by their retention owner. `None` means no board source
    /// is wired up, which skips the board round entirely.
    board_packets: Option<Vec<BitchatPacket>>,

    schedules: Vec<SyncSchedule>,
    archive: Option<GossipMessageArchive>,
    archive_dirty: bool,
    last_stale_peer_cleanup_secs: Option<u64>,
}

impl core::fmt::Debug for GossipSyncManager {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GossipSyncManager")
            .field("messages", &self.messages.len())
            .field("fragments", &self.fragments.len())
            .field("file_transfers", &self.file_transfers.len())
            .field("group_messages", &self.group_messages.len())
            .field("announces", &self.latest_announce_by_peer.len())
            .field("prekey_bundles", &self.latest_prekey_bundle_by_owner.len())
            .finish_non_exhaustive()
    }
}

impl GossipSyncManager {
    /// Builds a driver and, when an archive is supplied, restores the public
    /// messages that were still inside their window when the process died.
    pub fn new(
        my_peer: &PeerId,
        config: SyncConfig,
        archive: Option<GossipMessageArchive>,
        now_unix_secs: u64,
    ) -> Self {
        let schedules = build_schedules(&config);
        let requests = RequestSyncTracker::new(config.request_response_window_secs);
        let rate_limiter = SyncResponseRateLimiter::new(
            config.response_rate_limit_max_responses,
            config.response_rate_limit_window_secs,
        );
        let mut manager = Self {
            // An ID with no 8-byte routing form cannot address a mesh packet at
            // all. Swift writes an empty sender field there; we write zeros, which
            // is what its encoder produces from that empty field anyway.
            my_sender_id: my_peer.to_routing_bytes().unwrap_or([0u8; SENDER_ID_LEN]),
            config,
            requests,
            rate_limiter,
            messages: PacketStore::default(),
            fragments: PacketStore::default(),
            file_transfers: PacketStore::default(),
            group_messages: PacketStore::default(),
            latest_announce_by_peer: BTreeMap::new(),
            latest_prekey_bundle_by_owner: BTreeMap::new(),
            board_packets: None,
            schedules,
            archive,
            archive_dirty: false,
            last_stale_peer_cleanup_secs: None,
        };
        manager.restore_archived_messages(now_unix_secs);
        manager
    }

    pub fn config(&self) -> &SyncConfig {
        &self.config
    }

    /// Wires up (or refreshes) the board-post source.
    ///
    /// The board store owns board retention — expiry, tombstones, caps, persistence
    /// — so sync serves whatever it hands over rather than keeping a second copy
    /// that could disagree about what has been deleted. Until this is called there
    /// is no board round at all.
    pub fn set_board_packets(&mut self, packets: Vec<BitchatPacket>) {
        self.board_packets = Some(packets);
    }

    // -- intake ------------------------------------------------------------

    /// Files a public packet we saw on the mesh into the store for its type.
    ///
    /// Directed traffic is ignored: gossiping a packet its sender addressed to one
    /// peer would spread something they chose not to broadcast.
    pub fn on_public_packet_seen(&mut self, packet: &BitchatPacket, now_unix_secs: u64) {
        let Some(message_type) = packet.message_type() else {
            return;
        };
        if !packet_is_fresh(&self.config, packet, now_unix_secs) {
            return;
        }

        match message_type {
            MessageType::Announce => {
                // Announces are the one type without a broadcast-recipient check,
                // matching Swift: a directed announce is still a claim of presence.
                let sender = PeerId::from_bytes(&packet.sender_id);
                if !announcement_is_fresh(&self.config, packet, now_unix_secs) {
                    // Not merely ignored — an announce this old says the peer left,
                    // so its carried traffic goes with it.
                    self.remove_state_for(&sender);
                    return;
                }
                self.latest_announce_by_peer.insert(sender, packet.clone());
            }
            MessageType::Message => {
                if !is_broadcast_recipient(packet) {
                    return;
                }
                let capacity = self.config.seen_capacity.max(1);
                self.messages
                    .insert(packet.packet_id(), packet.clone(), capacity);
                self.archive_dirty = true;
            }
            MessageType::Fragment => {
                if !is_broadcast_recipient(packet) {
                    return;
                }
                let capacity = self.config.fragment_capacity.max(1);
                self.fragments
                    .insert(packet.packet_id(), packet.clone(), capacity);
            }
            MessageType::FileTransfer => {
                if !is_broadcast_recipient(packet) {
                    return;
                }
                let capacity = self.config.file_transfer_capacity.max(1);
                self.file_transfers
                    .insert(packet.packet_id(), packet.clone(), capacity);
            }
            MessageType::GroupMessage => {
                // Opaque ciphertext to non-members; carried and served like any
                // other broadcast so members get backfill from any relay.
                if !is_broadcast_recipient(packet) {
                    return;
                }
                let capacity = self.config.group_message_capacity.max(1);
                self.group_messages
                    .insert(packet.packet_id(), packet.clone(), capacity);
            }
            MessageType::PrekeyBundle => {
                if !is_broadcast_recipient(packet) {
                    return;
                }
                self.insert_prekey_bundle(packet);
            }
            _ => {}
        }
    }

    /// Keyed by the bundle's owner, never by the packet's sender.
    fn insert_prekey_bundle(&mut self, packet: &BitchatPacket) {
        // Callers feed only verified bundles here (our own at send time, peers'
        // after signature verification), so gossip never spreads a bundle this node
        // could not attribute.
        let Ok(bundle) = PrekeyBundle::decode(&packet.payload) else {
            return;
        };
        let owner = PeerId::from_noise_key(&bundle.owner_static);

        if let Some(existing) = self.latest_prekey_bundle_by_owner.get(&owner) {
            // Older or identical: keep what we have. Ties go to the incumbent so a
            // replay cannot churn the entry.
            if existing.timestamp >= packet.timestamp {
                return;
            }
            self.latest_prekey_bundle_by_owner
                .insert(owner, packet.clone());
            return;
        }

        // Bounded owner count. Replacing a known owner's bundle above is always
        // allowed, so the cap can never block a refresh.
        if self.latest_prekey_bundle_by_owner.len() >= self.config.prekey_bundle_capacity.max(1) {
            return;
        }
        self.latest_prekey_bundle_by_owner
            .insert(owner, packet.clone());
    }

    // -- responding --------------------------------------------------------

    /// Answers a peer's `REQUEST_SYNC` with the packets its filter says it lacks.
    ///
    /// Every returned packet is marked RSR with `ttl = 0`: solicited, link-local,
    /// and exempt from the freshness check on the requester's side — but only
    /// because *they* asked, which their own [`RequestSyncTracker`] is what
    /// verifies.
    ///
    /// Returns nothing when the peer is over its response budget. That is the only
    /// input-dependent reason to stay silent; a request whose filter covers
    /// everything we hold simply produces an empty diff.
    pub fn handle_request_sync(
        &mut self,
        from: &PeerId,
        request: &RequestSyncPacket,
        now_unix_secs: u64,
    ) -> Vec<SyncSend> {
        // A response can replay the whole store, so bound how often one peer can
        // trigger a diff pass regardless of how fast it asks.
        if !self.rate_limiter.should_respond(from, now_unix_secs) {
            return Vec::new();
        }

        // An older client that never sent the field means the default round.
        let requested = match request.types {
            Some(raw) => SyncTypeFlags::from_raw(raw),
            None => SyncTypeFlags::PUBLIC_MESSAGES,
        };

        // The requester's filter only covers packets at or after this cursor. Older
        // packets are outside the filter but not *missing*, and without the cursor
        // we would re-send that entire tail every single round.
        let since = request.since_timestamp_ms;
        let modulus = request.m;
        let sorted = gcs::decode_to_sorted(u32::from(request.p), modulus, &request.data);
        let might_contain = |packet: &BitchatPacket| -> bool {
            gcs::contains(&sorted, gcs::bucket(&packet.packet_id(), modulus))
        };
        let newer_than_cursor = |packet: &BitchatPacket| -> bool {
            match since {
                Some(cursor) => packet.timestamp >= cursor,
                None => true,
            }
        };

        let config = &self.config;
        let fresh = |packet: &BitchatPacket| packet_is_fresh(config, packet, now_unix_secs);
        let mut sends: Vec<SyncSend> = Vec::new();

        // Announces are exempt from the since-cursor: they carry the signing keys
        // needed to verify everything else, and there is at most one per peer, so
        // the resend cost is negligible next to being unable to verify anything.
        if requested.contains(MessageType::Announce) {
            for packet in self.latest_announce_by_peer.values() {
                if fresh(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        if requested.contains(MessageType::Message) {
            for packet in self.messages.all_packets(fresh) {
                if newer_than_cursor(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        if requested.contains(MessageType::Fragment) {
            // A fragment-ID filter narrows the diff to exactly the named streams —
            // targeted resync for a stalled reassembly — and bypasses the
            // since-cursor for them, since the whole reason the reassembly stalled
            // is that the missing pieces are older than what the cursor covers. The
            // GCS filter still excludes the pieces the requester already holds.
            let wanted_streams = request.fragment_ids();
            for packet in self.fragments.all_packets(fresh) {
                let included = match &wanted_streams {
                    Some(streams) => {
                        fragment_stream_id(packet).is_some_and(|stream| streams.contains(&stream))
                    }
                    None => newer_than_cursor(packet),
                };
                if included && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        if requested.contains(MessageType::FileTransfer) {
            for packet in self.file_transfers.all_packets(fresh) {
                if newer_than_cursor(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        if requested.contains(MessageType::GroupMessage) {
            for packet in self.group_messages.all_packets(fresh) {
                if newer_than_cursor(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        // Like announces, bundles are exempt from the since-cursor: there is at
        // most one per owner (newer replaces older), so the resend cost is bounded,
        // and a joining peer must be able to learn bundles generated long before it
        // arrived.
        if requested.contains(MessageType::PrekeyBundle) {
            for packet in self.latest_prekey_bundle_by_owner.values() {
                if fresh(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        if requested.contains(MessageType::BoardPost) {
            // No freshness window applies: the board store already filtered to live
            // posts and tombstones, and posts sync until their own expiry.
            for packet in self.board_packets.iter().flatten() {
                if newer_than_cursor(packet) && !might_contain(packet) {
                    sends.push(SyncSend::response(from, packet));
                }
            }
        }

        sends
    }

    /// Whether an inbound RSR packet answers something we actually asked `from`.
    ///
    /// The receive pipeline calls this before granting the freshness exemption. A
    /// false answer here is what stops a captured response being replayed forever.
    pub fn is_valid_response(&self, from: &PeerId, is_rsr: bool, now_unix_secs: u64) -> bool {
        self.requests.is_valid_response(from, is_rsr, now_unix_secs)
    }

    // -- requesting --------------------------------------------------------

    /// The first request to a freshly connected peer, covering every enabled type
    /// at once so the link is not spent on five separate rounds.
    ///
    /// The runtime should delay this by [`INITIAL_SYNC_DELAY_SECS`].
    pub fn initial_sync_to(
        &mut self,
        peer: &PeerId,
        now_unix_secs: u64,
    ) -> Result<SyncSend, StoreError> {
        let types = self.initial_sync_types();
        self.build_request(Some(peer), types, None, now_unix_secs)
    }

    /// Targeted fragment recovery for reassemblies that have stalled.
    ///
    /// Asks each connected peer for exactly the named streams instead of waiting on
    /// the next periodic fragment round to happen to cover them. At most
    /// `chatter_mesh::tlv::request_sync::MAX_FRAGMENT_ID_FILTER_COUNT` streams ride
    /// in one request; the encoder truncates past that, which keeps the filter value
    /// inside the decoder's 1024-byte acceptance cap on the far side. An oversized
    /// value would be *ignored* by the peer while the rest of the request still
    /// decoded, silently degrading a targeted resync into an untargeted one.
    ///
    /// Returns nothing when no stream is named or no peer is connected — there is
    /// no broadcast fallback here, because an untargeted broadcast is precisely what
    /// this call exists to avoid.
    pub fn request_missing_fragments(
        &mut self,
        fragment_ids: &[[u8; FRAGMENT_ID_LEN]],
        connected_peers: &[PeerId],
        now_unix_secs: u64,
    ) -> Result<Vec<SyncSend>, StoreError> {
        let Some(filter) = encode_fragment_id_filter(fragment_ids) else {
            return Ok(Vec::new());
        };
        if connected_peers.is_empty() {
            return Ok(Vec::new());
        }

        let mut sends = Vec::with_capacity(connected_peers.len());
        for peer in connected_peers {
            sends.push(self.build_request(
                Some(peer),
                SyncTypeFlags::FRAGMENT,
                Some(filter.clone()),
                now_unix_secs,
            )?);
        }
        Ok(sends)
    }

    /// One maintenance tick: expire, prune, and emit the sync rounds now due.
    ///
    /// Does **not** persist the archive — [`persist_if_dirty`](Self::persist_if_dirty)
    /// is separate so a storage failure cannot swallow the packets this round wanted
    /// to send. The runtime calls both, in either order.
    pub fn maintenance(
        &mut self,
        connected_peers: &[PeerId],
        now_unix_secs: u64,
    ) -> Result<Vec<SyncSend>, StoreError> {
        self.cleanup_expired(now_unix_secs);
        self.cleanup_stale_announcements_if_needed(now_unix_secs);
        self.requests.cleanup(now_unix_secs);
        self.rate_limiter.prune(now_unix_secs);

        let has_board = self.board_packets.is_some();
        let mut due: Vec<SyncTypeFlags> = Vec::new();
        for schedule in &mut self.schedules {
            if schedule.interval_secs == 0 {
                continue;
            }
            // No board source wired up means nothing to offer or store; skip the
            // round entirely rather than asking for posts we would discard.
            if schedule.types == SyncTypeFlags::BOARD && !has_board {
                continue;
            }
            if schedule.is_due(now_unix_secs) {
                schedule.last_sent_secs = Some(now_unix_secs);
                due.push(schedule.types);
            }
        }

        let mut sends = Vec::new();
        for types in due {
            sends.extend(self.periodic_sync(types, connected_peers, now_unix_secs)?);
        }
        Ok(sends)
    }

    /// Unicast to every connected peer, or broadcast when there are none.
    ///
    /// Unicast is what makes the answers attributable: a response to a request we
    /// sent *this peer* can be checked against our own record of having sent it. The
    /// broadcast form exists only for the discovery phase, where there is no peer to
    /// address and therefore nothing to attribute.
    fn periodic_sync(
        &mut self,
        types: SyncTypeFlags,
        connected_peers: &[PeerId],
        now_unix_secs: u64,
    ) -> Result<Vec<SyncSend>, StoreError> {
        if connected_peers.is_empty() {
            return Ok(vec![self.build_request(
                None,
                types,
                None,
                now_unix_secs,
            )?]);
        }
        let mut sends = Vec::with_capacity(connected_peers.len());
        for peer in connected_peers {
            sends.push(self.build_request(Some(peer), types, None, now_unix_secs)?);
        }
        Ok(sends)
    }

    /// Builds one `REQUEST_SYNC` packet.
    ///
    /// `ttl` is 0 and nothing may change that: the request is link-local, and a
    /// non-zero TTL would ask every node the packet reached to answer.
    fn build_request(
        &mut self,
        to: Option<&PeerId>,
        types: SyncTypeFlags,
        fragment_id_filter: Option<String>,
        now_unix_secs: u64,
    ) -> Result<SyncSend, StoreError> {
        // Only a unicast request can be answered attributably, so only a unicast
        // request opens a response window.
        if let Some(peer) = to {
            self.requests.register_request(peer, now_unix_secs);
        }

        let payload = self
            .build_gcs_payload(types, fragment_id_filter, now_unix_secs)
            .encode()?;

        let packet = BitchatPacket::new(
            MessageType::RequestSync,
            self.my_sender_id,
            to.and_then(PeerId::to_routing_bytes),
            now_unix_secs.saturating_mul(1000),
            payload,
            0,
        );
        Ok(SyncSend::request(to, packet))
    }

    /// Builds the filter describing what we already hold, so the peer can send only
    /// the difference.
    fn build_gcs_payload(
        &self,
        types: SyncTypeFlags,
        fragment_id_filter: Option<String>,
        now_unix_secs: u64,
    ) -> RequestSyncPacket {
        let p = gcs::derive_p(self.config.gcs_target_fpr);
        // `derive_p` clamps to 1..=32, so the fallback is unreachable; it exists
        // because a panic on radio-adjacent code is never the right answer.
        let p_byte = u8::try_from(p).unwrap_or(MAX_P);

        let empty_filter =
            |packet_types: SyncTypeFlags, filter: Option<String>| RequestSyncPacket {
                p: p_byte,
                // `m = 1` decodes to the empty set on the far side, i.e. "I hold
                // nothing, send me everything".
                m: 1,
                data: Vec::new(),
                types: Some(packet_types.raw()),
                since_timestamp_ms: None,
                fragment_id_filter: filter,
            };

        let mut candidates = self.collect_candidates(types, now_unix_secs);
        if candidates.is_empty() {
            return empty_filter(types, fragment_id_filter);
        }

        // Newest first. Everything downstream depends on this order: the byte-budget
        // trim drops from the tail, so the covered set is always a contiguous
        // newest-prefix and the since-cursor is exact.
        candidates.sort_by_key(|c| core::cmp::Reverse(c.timestamp));

        let n_max = gcs::estimate_max_elements(self.config.gcs_max_bytes, p);
        let cap = self.capacity_for(types);
        let take = candidates.len().min(n_max.min(cap));
        if take == 0 {
            return empty_filter(types, fragment_id_filter);
        }

        let ids: Vec<[u8; 16]> = candidates
            .iter()
            .take(take)
            .map(|packet| packet.packet_id())
            .collect();
        let params = gcs::build_filter(&ids, self.config.gcs_max_bytes, self.config.gcs_target_fpr);

        // When the filter cannot cover every candidate — the store exceeds `take`,
        // or the encoder trimmed the tail to fit the byte budget — tell the
        // responder how far back the filter actually reaches. Packets older than
        // the cursor are outside the filter but not missing; without it the
        // responder would re-send that entire tail every round.
        let covered = params.included;
        let since_timestamp_ms = if covered > 0 && covered < candidates.len() {
            candidates.get(covered - 1).map(|packet| packet.timestamp)
        } else {
            None
        };

        RequestSyncPacket {
            p: u8::try_from(params.p).unwrap_or(MAX_P),
            m: params.m,
            data: params.data,
            types: Some(types.raw()),
            since_timestamp_ms,
            fragment_id_filter,
        }
    }

    /// Everything we hold that a round for `types` would cover.
    fn collect_candidates(&self, types: SyncTypeFlags, now_unix_secs: u64) -> Vec<&BitchatPacket> {
        let config = &self.config;
        let fresh = |packet: &BitchatPacket| packet_is_fresh(config, packet, now_unix_secs);
        let mut candidates: Vec<&BitchatPacket> = Vec::new();

        if types.contains(MessageType::Announce) {
            candidates.extend(
                self.latest_announce_by_peer
                    .values()
                    .filter(|packet| fresh(packet)),
            );
        }
        if types.contains(MessageType::Message) {
            candidates.extend(self.messages.all_packets(fresh));
        }
        if types.contains(MessageType::Fragment) {
            candidates.extend(self.fragments.all_packets(fresh));
        }
        if types.contains(MessageType::FileTransfer) {
            candidates.extend(self.file_transfers.all_packets(fresh));
        }
        if types.contains(MessageType::GroupMessage) {
            candidates.extend(self.group_messages.all_packets(fresh));
        }
        if types.contains(MessageType::PrekeyBundle) {
            candidates.extend(
                self.latest_prekey_bundle_by_owner
                    .values()
                    .filter(|packet| fresh(packet)),
            );
        }
        if types.contains(MessageType::BoardPost) {
            candidates.extend(self.board_packets.iter().flatten());
        }
        candidates
    }

    /// The store capacity a round's filter is allowed to draw on.
    ///
    /// Compares the whole set, as Swift does: a mixed round falls back to the
    /// public-message capacity, which is the largest of them.
    fn capacity_for(&self, types: SyncTypeFlags) -> usize {
        if types == SyncTypeFlags::FRAGMENT {
            self.config.fragment_capacity.max(1)
        } else if types == SyncTypeFlags::FILE_TRANSFER {
            self.config.file_transfer_capacity.max(1)
        } else if types == SyncTypeFlags::PREKEY_BUNDLE {
            self.config.prekey_bundle_capacity.max(1)
        } else if types == SyncTypeFlags::BOARD {
            self.config.board_capacity.max(1)
        } else {
            self.config.seen_capacity.max(1)
        }
    }

    /// Every type an initial sync asks for, honouring the disabled rounds.
    fn initial_sync_types(&self) -> SyncTypeFlags {
        let mut types = SyncTypeFlags::PUBLIC_MESSAGES;
        if self.config.group_message_capacity > 0 {
            types = types.union(SyncTypeFlags::GROUP_MESSAGE);
        }
        if self.config.fragment_capacity > 0 && self.config.fragment_sync_interval_secs > 0 {
            types = types.union(SyncTypeFlags::FRAGMENT);
        }
        if self.config.file_transfer_capacity > 0
            && self.config.file_transfer_sync_interval_secs > 0
        {
            types = types.union(SyncTypeFlags::FILE_TRANSFER);
        }
        if self.config.prekey_bundle_capacity > 0
            && self.config.prekey_bundle_sync_interval_secs > 0
        {
            types = types.union(SyncTypeFlags::PREKEY_BUNDLE);
        }
        if self.config.board_capacity > 0
            && self.config.board_sync_interval_secs > 0
            && self.board_packets.is_some()
        {
            types = types.union(SyncTypeFlags::BOARD);
        }
        types
    }

    // -- expiry and removal ------------------------------------------------

    fn cleanup_expired(&mut self, now_unix_secs: u64) {
        let config = &self.config;
        let fresh = |packet: &BitchatPacket| packet_is_fresh(config, packet, now_unix_secs);

        self.latest_announce_by_peer
            .retain(|_, packet| fresh(&*packet));

        let before = self.messages.len();
        self.messages.remove_expired(fresh);
        if self.messages.len() != before {
            self.archive_dirty = true;
        }

        self.fragments.remove_expired(fresh);
        self.file_transfers.remove_expired(fresh);
        self.group_messages.remove_expired(fresh);
        self.latest_prekey_bundle_by_owner
            .retain(|_, packet| fresh(&*packet));
    }

    fn cleanup_stale_announcements_if_needed(&mut self, now_unix_secs: u64) {
        if let Some(last) = self.last_stale_peer_cleanup_secs
            && now_unix_secs.saturating_sub(last) < self.config.stale_peer_cleanup_interval_secs
        {
            return;
        }
        self.last_stale_peer_cleanup_secs = Some(now_unix_secs);

        let config = &self.config;
        let stale: Vec<PeerId> = self
            .latest_announce_by_peer
            .iter()
            .filter(|(_, packet)| !announcement_is_fresh(config, packet, now_unix_secs))
            .map(|(peer, _)| peer.clone())
            .collect();
        for peer in stale {
            self.remove_state_for(&peer);
        }
    }

    /// Drops a peer's announce and its carried traffic. For LEAVE and stale peers.
    pub fn remove_announcement_for_peer(&mut self, peer: &PeerId) {
        self.remove_state_for(peer);
    }

    fn remove_state_for(&mut self, peer: &PeerId) {
        // Deliberately keeps the peer's prekey bundle: bundles exist to reach owners
        // who left the mesh, and they age out on their own, much longer schedule.
        self.latest_announce_by_peer.remove(peer);

        let before = self.messages.len();
        self.messages
            .remove_where(|packet| PeerId::from_bytes(&packet.sender_id) == *peer);
        if self.messages.len() != before {
            self.archive_dirty = true;
        }
        self.fragments
            .remove_where(|packet| PeerId::from_bytes(&packet.sender_id) == *peer);
        self.file_transfers
            .remove_where(|packet| PeerId::from_bytes(&packet.sender_id) == *peer);
        self.group_messages
            .remove_where(|packet| PeerId::from_bytes(&packet.sender_id) == *peer);
    }

    /// Block-time hygiene: drop the carried public messages from a blocked sender.
    ///
    /// Narrower than [`remove_announcement_for_peer`](Self::remove_announcement_for_peer)
    /// — the peer's announce and in-flight fragments are untouched — and it persists
    /// immediately rather than waiting for the next tick, because a relaunch in that
    /// gap would restore the purged messages straight off disk.
    pub fn remove_public_messages_from(
        &mut self,
        peer: &PeerId,
        now_unix_secs: u64,
    ) -> Result<(), StoreError> {
        let before = self.messages.len();
        self.messages
            .remove_where(|packet| PeerId::from_bytes(&packet.sender_id) == *peer);
        if self.messages.len() == before {
            return Ok(());
        }
        self.archive_dirty = true;
        self.persist_if_dirty(now_unix_secs)
    }

    // -- archive -----------------------------------------------------------

    /// Rebuilds the public-message store from disk, dropping anything that aged out
    /// while the process was dead.
    fn restore_archived_messages(&mut self, now_unix_secs: u64) {
        let Some(archive) = &self.archive else {
            return;
        };
        let config = &self.config;
        let capacity = config.seen_capacity.max(1);
        let restored: Vec<BitchatPacket> = archive
            .load_packets()
            .into_iter()
            .filter(|packet| packet.msg_type == MessageType::Message.as_u8())
            .filter(|packet| packet_is_fresh(config, packet, now_unix_secs))
            .collect();
        if restored.is_empty() {
            return;
        }
        for packet in restored {
            self.messages.insert(packet.packet_id(), packet, capacity);
        }
        self.archive_dirty = true;
    }

    /// Writes the carried public messages out, if anything changed since the last
    /// write. Cheap and idempotent when nothing did.
    pub fn persist_if_dirty(&mut self, now_unix_secs: u64) -> Result<(), StoreError> {
        if !self.archive_dirty {
            return Ok(());
        }
        let config = &self.config;
        let packets: Vec<BitchatPacket> = self
            .messages
            .all_packets(|packet| packet_is_fresh(config, packet, now_unix_secs))
            .into_iter()
            .cloned()
            .collect();
        if let Some(archive) = &self.archive {
            archive.save_packets(&packets)?;
        }
        // Cleared only on success, so a transient storage failure retries next tick
        // rather than silently dropping the write. A driver with no archive at all
        // clears it too: there is nothing to be dirty against.
        self.archive_dirty = false;
        Ok(())
    }

    // -- introspection -----------------------------------------------------

    /// The public messages we currently carry, for the "heard here earlier"
    /// timeline echo.
    pub fn public_message_packets(&self, now_unix_secs: u64) -> Vec<BitchatPacket> {
        let config = &self.config;
        self.messages
            .all_packets(|packet| packet_is_fresh(config, packet, now_unix_secs))
            .into_iter()
            .cloned()
            .collect()
    }

    /// Whether this exact public message or file transfer is already carried,
    /// by gossip packet ID (so a relayed or RSR copy matches its original).
    ///
    /// The "have we already shown this" answer for our own traffic coming
    /// back: anything we sent or already accepted is filed here, for longer than
    /// the self-broadcast tracker remembers it. Other types answer `false`.
    pub fn holds(&self, packet: &BitchatPacket) -> bool {
        let store = match packet.message_type() {
            Some(MessageType::Message) => &self.messages,
            Some(MessageType::FileTransfer) => &self.file_transfers,
            _ => return false,
        };
        store.packets.contains_key(&packet.packet_id())
    }

    pub fn has_announcement(&self, peer: &PeerId) -> bool {
        self.latest_announce_by_peer.contains_key(peer)
    }

    pub fn has_prekey_bundle(&self, owner: &PeerId) -> bool {
        self.latest_prekey_bundle_by_owner.contains_key(owner)
    }

    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    pub fn fragment_count(&self) -> usize {
        self.fragments.len()
    }

    /// See [`RequestSyncTracker::pending_peers`].
    pub fn pending_request_peers(&self) -> Vec<PeerId> {
        self.requests.pending_peers()
    }

    pub fn pending_request_count(&self) -> usize {
        self.requests.pending_count()
    }
}

/// The 8-byte stream ID prefixing every fragment payload.
///
/// A payload too short to carry one belongs to no stream and is never selected by a
/// targeted filter — the same outcome as Swift's short `prefix(8)`, which cannot
/// equal an 8-byte ID.
fn fragment_stream_id(packet: &BitchatPacket) -> Option<[u8; FRAGMENT_ID_LEN]> {
    let head = packet.payload.get(..FRAGMENT_ID_LEN)?;
    <[u8; FRAGMENT_ID_LEN]>::try_from(head).ok()
}

/// One schedule per enabled type group, in Swift's order.
fn build_schedules(config: &SyncConfig) -> Vec<SyncSchedule> {
    let mut schedules = Vec::new();

    if config.seen_capacity > 0 && config.message_sync_interval_secs > 0 {
        // Group messages ride the public-message cadence. Clients that predate the
        // bit ignore it and answer with announces and messages only.
        let mut types = SyncTypeFlags::PUBLIC_MESSAGES;
        if config.group_message_capacity > 0 {
            types = types.union(SyncTypeFlags::GROUP_MESSAGE);
        }
        schedules.push(SyncSchedule {
            types,
            interval_secs: config.message_sync_interval_secs,
            last_sent_secs: None,
        });
    }
    if config.fragment_capacity > 0 && config.fragment_sync_interval_secs > 0 {
        schedules.push(SyncSchedule {
            types: SyncTypeFlags::FRAGMENT,
            interval_secs: config.fragment_sync_interval_secs,
            last_sent_secs: None,
        });
    }
    if config.file_transfer_capacity > 0 && config.file_transfer_sync_interval_secs > 0 {
        schedules.push(SyncSchedule {
            types: SyncTypeFlags::FILE_TRANSFER,
            interval_secs: config.file_transfer_sync_interval_secs,
            last_sent_secs: None,
        });
    }
    if config.prekey_bundle_capacity > 0 && config.prekey_bundle_sync_interval_secs > 0 {
        schedules.push(SyncSchedule {
            types: SyncTypeFlags::PREKEY_BUNDLE,
            interval_secs: config.prekey_bundle_sync_interval_secs,
            last_sent_secs: None,
        });
    }
    if config.board_capacity > 0 && config.board_sync_interval_secs > 0 {
        schedules.push(SyncSchedule {
            types: SyncTypeFlags::BOARD,
            interval_secs: config.board_sync_interval_secs,
            last_sent_secs: None,
        });
    }
    schedules
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{MemoryStorage, Storage, StorageKey};
    use chatter_mesh::tlv::request_sync::MAX_FRAGMENT_ID_FILTER_COUNT;
    use chatter_mesh_noise::prekey::Prekey;
    use std::sync::Arc;

    /// A fixed "now" so every age in these tests is exact.
    const NOW: u64 = 1_700_000_000;
    const NOW_MS: u64 = NOW * 1000;

    fn peer(hex: &str) -> PeerId {
        PeerId::from_str(hex)
    }

    fn us() -> PeerId {
        peer("0011223344556677")
    }

    fn them() -> PeerId {
        peer("aabbccddeeff0011")
    }

    fn sender() -> [u8; SENDER_ID_LEN] {
        [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11]
    }

    fn packet_of(msg_type: MessageType, payload: &[u8], timestamp_ms: u64) -> BitchatPacket {
        BitchatPacket::new(msg_type, sender(), None, timestamp_ms, payload.to_vec(), 7)
    }

    fn message(payload: &[u8], timestamp_ms: u64) -> BitchatPacket {
        packet_of(MessageType::Message, payload, timestamp_ms)
    }

    fn fragment(stream: [u8; 8], index: u16, timestamp_ms: u64) -> BitchatPacket {
        let mut payload = Vec::new();
        payload.extend_from_slice(&stream);
        payload.extend_from_slice(&index.to_be_bytes());
        payload.extend_from_slice(&4u16.to_be_bytes());
        payload.push(MessageType::FileTransfer.as_u8());
        payload.push(0xEE);
        packet_of(MessageType::Fragment, &payload, timestamp_ms)
    }

    fn manager() -> GossipSyncManager {
        GossipSyncManager::new(&us(), SyncConfig::default(), None, NOW)
    }

    fn manager_with(config: SyncConfig) -> GossipSyncManager {
        GossipSyncManager::new(&us(), config, None, NOW)
    }

    /// A request whose filter is empty: "I hold nothing, send me everything."
    fn empty_request(types: SyncTypeFlags) -> RequestSyncPacket {
        RequestSyncPacket {
            p: 7,
            m: 1,
            data: Vec::new(),
            types: Some(types.raw()),
            since_timestamp_ms: None,
            fragment_id_filter: None,
        }
    }

    fn decode_request(send: &SyncSend) -> RequestSyncPacket {
        RequestSyncPacket::decode(&send.packet.payload).expect("request decodes")
    }

    #[test]
    fn holds_matches_a_stored_message_by_id_whatever_its_ttl_or_rsr() {
        let mut sync = manager();
        let original = message(b"hello", NOW_MS);
        assert!(!sync.holds(&original));
        sync.on_public_packet_seen(&original, NOW);

        let mut response = original.clone();
        response.ttl = 0;
        response.is_rsr = true;
        assert!(sync.holds(&response), "a sync copy is the same packet");
        assert!(!sync.holds(&message(b"other", NOW_MS)));
    }

    // -- solicitation ------------------------------------------------------

    #[test]
    fn a_solicited_rsr_is_accepted_and_an_unsolicited_one_is_not() {
        let mut tracker = RequestSyncTracker::default();
        // Nothing was asked of this peer: the flag alone buys nothing, which is the
        // whole point — it is attacker-controlled.
        assert!(!tracker.is_valid_response(&them(), true, NOW));

        tracker.register_request(&them(), NOW);
        assert!(tracker.is_valid_response(&them(), true, NOW));

        // Asking one peer does not open a window for another.
        assert!(!tracker.is_valid_response(&peer("1122334455667788"), true, NOW));
    }

    #[test]
    fn a_packet_without_the_rsr_flag_is_never_a_response() {
        let mut tracker = RequestSyncTracker::default();
        tracker.register_request(&them(), NOW);
        // Not a rejection: it simply takes the ordinary freshness path.
        assert!(!tracker.is_valid_response(&them(), false, NOW));
    }

    #[test]
    fn the_response_window_closes_exactly_at_thirty_seconds() {
        let mut tracker = RequestSyncTracker::default();
        tracker.register_request(&them(), NOW);

        assert!(tracker.is_valid_response(&them(), true, NOW + RESPONSE_WINDOW_SECS));
        assert!(!tracker.is_valid_response(&them(), true, NOW + RESPONSE_WINDOW_SECS + 1));
    }

    #[test]
    fn a_re_request_slides_the_window_and_cleanup_drops_closed_ones() {
        let mut tracker = RequestSyncTracker::default();
        tracker.register_request(&them(), NOW);
        tracker.register_request(&them(), NOW + 20);
        // The second request refreshes the timestamp rather than stacking.
        assert_eq!(tracker.pending_count(), 1);
        assert!(tracker.is_valid_response(&them(), true, NOW + 50));

        tracker.cleanup(NOW + 50);
        assert_eq!(tracker.pending_count(), 1);
        tracker.cleanup(NOW + 51);
        assert_eq!(tracker.pending_count(), 0);
        assert!(!tracker.is_valid_response(&them(), true, NOW + 51));
    }

    #[test]
    fn a_backwards_clock_does_not_invalidate_an_open_request() {
        let mut tracker = RequestSyncTracker::default();
        tracker.register_request(&them(), NOW);
        // Saturating: the age is zero, not a wrapped eternity.
        assert!(tracker.is_valid_response(&them(), true, NOW - 5));
    }

    #[test]
    fn the_manager_only_opens_a_window_for_unicast_requests() {
        let mut manager = manager();

        // No connected peers: the discovery fallback broadcasts, and a broadcast
        // cannot attribute its answers, so no window opens.
        let sends = manager.maintenance(&[], NOW).expect("maintenance");
        assert!(!sends.is_empty());
        assert!(sends.iter().all(|send| send.to.is_none()));
        assert_eq!(manager.pending_request_count(), 0);
        assert!(!manager.is_valid_response(&them(), true, NOW));

        // Unicast to a connected peer does.
        let sends = manager
            .maintenance(&[them()], NOW + 60)
            .expect("maintenance");
        assert!(sends.iter().all(|send| send.to == Some(them())));
        assert_eq!(manager.pending_request_count(), 1);
        assert!(manager.is_valid_response(&them(), true, NOW + 60));
        assert!(!manager.is_valid_response(&them(), true, NOW + 60 + RESPONSE_WINDOW_SECS + 1));
    }

    // -- rate limiting -----------------------------------------------------

    #[test]
    fn the_rate_limiter_allows_exactly_eight_per_window() {
        let mut limiter = SyncResponseRateLimiter::default();
        for attempt in 1..=MAX_RESPONSES_PER_WINDOW {
            assert!(
                limiter.should_respond(&them(), NOW),
                "response {attempt} of {MAX_RESPONSES_PER_WINDOW} must be allowed"
            );
        }
        // The ninth inside the window is refused.
        assert!(!limiter.should_respond(&them(), NOW));
        assert_eq!(limiter.recorded(&them()), MAX_RESPONSES_PER_WINDOW);
    }

    #[test]
    fn the_rate_limit_window_is_inclusive_and_slides() {
        let mut limiter = SyncResponseRateLimiter::default();
        for _ in 0..MAX_RESPONSES_PER_WINDOW {
            assert!(limiter.should_respond(&them(), NOW));
        }
        // Exactly one window later the first entries are still inside it.
        assert!(!limiter.should_respond(&them(), NOW + RESPONSE_RATE_LIMIT_WINDOW_SECS));
        // One second past, they fall out and the budget is free again.
        assert!(limiter.should_respond(&them(), NOW + RESPONSE_RATE_LIMIT_WINDOW_SECS + 1));
    }

    #[test]
    fn the_rate_limit_is_per_peer() {
        let mut limiter = SyncResponseRateLimiter::default();
        for _ in 0..MAX_RESPONSES_PER_WINDOW {
            assert!(limiter.should_respond(&them(), NOW));
        }
        assert!(!limiter.should_respond(&them(), NOW));
        // A flooding peer must not be able to mute everyone else.
        assert!(limiter.should_respond(&peer("1122334455667788"), NOW));
    }

    #[test]
    fn pruning_forgets_departed_peers() {
        let mut limiter = SyncResponseRateLimiter::default();
        assert!(limiter.should_respond(&them(), NOW));
        limiter.prune(NOW + RESPONSE_RATE_LIMIT_WINDOW_SECS);
        assert_eq!(limiter.recorded(&them()), 1);
        limiter.prune(NOW + RESPONSE_RATE_LIMIT_WINDOW_SECS + 1);
        assert_eq!(limiter.recorded(&them()), 0);
    }

    #[test]
    fn a_flooding_peer_stops_getting_answers_after_eight_rounds() {
        let mut manager = manager();
        manager.on_public_packet_seen(&message(b"hello", NOW_MS), NOW);

        for attempt in 1..=MAX_RESPONSES_PER_WINDOW {
            let sends =
                manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::MESSAGE), NOW);
            assert_eq!(sends.len(), 1, "round {attempt} must be answered");
        }
        assert!(
            manager
                .handle_request_sync(&them(), &empty_request(SyncTypeFlags::MESSAGE), NOW)
                .is_empty(),
            "the ninth request inside the window must be dropped"
        );
    }

    // -- link-local requests, RSR responses --------------------------------

    #[test]
    fn every_request_we_originate_is_link_local() {
        let mut manager = manager();
        manager.set_board_packets(Vec::new());
        let mut sends = manager.maintenance(&[them()], NOW).expect("maintenance");
        sends.push(manager.initial_sync_to(&them(), NOW).expect("initial"));
        sends.extend(
            manager
                .request_missing_fragments(&[[0x01; 8]], &[them()], NOW)
                .expect("fragments"),
        );

        assert!(!sends.is_empty());
        for send in &sends {
            assert_eq!(send.packet.msg_type, MessageType::RequestSync.as_u8());
            assert_eq!(send.packet.ttl, 0, "REQUEST_SYNC must never be relayable");
            assert!(!send.packet.is_rsr, "a request is not a response");
            assert!(send.sign_before_send, "our own packets go out signed");
        }
    }

    #[test]
    fn responses_are_rsr_marked_ttl_zero_and_never_re_signed() {
        let mut manager = manager();
        manager.on_public_packet_seen(&message(b"carried", NOW_MS), NOW);

        let sends =
            manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::MESSAGE), NOW);
        assert_eq!(sends.len(), 1);
        let send = &sends[0];
        assert_eq!(send.to, Some(them()));
        assert_eq!(send.packet.ttl, 0);
        assert!(send.packet.is_rsr);
        assert!(
            !send.sign_before_send,
            "re-signing would replace the author's signature with ours"
        );
        assert_eq!(send.packet.payload, b"carried".to_vec());
    }

    #[test]
    fn a_response_keeps_the_original_signature_valid() {
        // ttl and is_rsr are both excluded from the signing preimage, which is what
        // makes it safe to mutate them on a stored packet.
        let mut manager = manager();
        let mut original = message(b"signed", NOW_MS);
        original.signature = Some([0x5A; 64]);
        manager.on_public_packet_seen(&original, NOW);

        let sends =
            manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::MESSAGE), NOW);
        let sent = &sends[0].packet;
        assert_eq!(sent.signature, original.signature);
        assert_eq!(
            sent.signing_preimage().expect("preimage"),
            original.signing_preimage().expect("preimage")
        );
    }

    // -- the diff ----------------------------------------------------------

    #[test]
    fn a_peer_holding_a_subset_gets_exactly_the_difference() {
        let held: Vec<BitchatPacket> = (0..5u8)
            .map(|i| message(&[0x10 + i], NOW_MS + u64::from(i)))
            .collect();

        // The responder carries all five.
        let mut responder = manager();
        for packet in &held {
            responder.on_public_packet_seen(packet, NOW);
        }

        // The requester carries the first three, and builds a real filter over them.
        let mut requester = manager();
        for packet in held.iter().take(3) {
            requester.on_public_packet_seen(packet, NOW);
        }
        let request_send = requester.initial_sync_to(&them(), NOW).expect("request");
        let request = decode_request(&request_send);

        let sends = responder.handle_request_sync(&them(), &request, NOW);
        let payloads: Vec<Vec<u8>> = sends
            .iter()
            .map(|send| send.packet.payload.clone())
            .collect();
        assert_eq!(
            payloads,
            vec![vec![0x13], vec![0x14]],
            "only the two the requester lacks, in store order"
        );
    }

    #[test]
    fn a_peer_holding_everything_gets_nothing_back() {
        let held: Vec<BitchatPacket> = (0..5u8)
            .map(|i| message(&[0x20 + i], NOW_MS + u64::from(i)))
            .collect();

        let mut responder = manager();
        let mut requester = manager();
        for packet in &held {
            responder.on_public_packet_seen(packet, NOW);
            requester.on_public_packet_seen(packet, NOW);
        }

        let request = decode_request(&requester.initial_sync_to(&them(), NOW).expect("request"));
        assert!(
            responder
                .handle_request_sync(&them(), &request, NOW)
                .is_empty(),
            "a filter covering the whole store leaves nothing to send"
        );
    }

    #[test]
    fn the_diff_is_computed_over_recomputed_packet_ids() {
        // The filter is built from `packet_id()`, and the responder must recompute
        // it the same way — diffing anything else (a stored hex string, the raw
        // bytes) silently stops the filter from ever matching.
        let mut responder = manager();
        let carried = message(b"already-held", NOW_MS);
        responder.on_public_packet_seen(&carried, NOW);

        let params = gcs::build_filter(&[carried.packet_id()], 256, 0.01);
        let request = RequestSyncPacket {
            p: u8::try_from(params.p).expect("p fits"),
            m: params.m,
            data: params.data,
            types: Some(SyncTypeFlags::MESSAGE.raw()),
            since_timestamp_ms: None,
            fragment_id_filter: None,
        };
        assert!(
            responder
                .handle_request_sync(&them(), &request, NOW)
                .is_empty()
        );

        // A relayed copy of the same packet has a different ttl and still matches:
        // packet IDs deliberately exclude ttl.
        let relayed = BitchatPacket {
            ttl: 3,
            ..carried.clone()
        };
        assert_eq!(relayed.packet_id(), carried.packet_id());
    }

    // -- the since cursor --------------------------------------------------

    #[test]
    fn the_since_cursor_excludes_older_packets_but_never_announces() {
        let mut manager = manager();
        let announce = packet_of(MessageType::Announce, b"", NOW_MS - 50_000);
        let old = message(b"old", NOW_MS - 60_000);
        let new = message(b"new", NOW_MS);
        manager.on_public_packet_seen(&announce, NOW);
        manager.on_public_packet_seen(&old, NOW);
        manager.on_public_packet_seen(&new, NOW);

        let mut request = empty_request(SyncTypeFlags::PUBLIC_MESSAGES);
        request.since_timestamp_ms = Some(NOW_MS - 30_000);

        let sends = manager.handle_request_sync(&them(), &request, NOW);
        assert_eq!(sends.len(), 2);
        assert!(
            sends
                .iter()
                .any(|send| send.packet.msg_type == MessageType::Announce.as_u8()),
            "announces carry the keys needed to verify everything else"
        );
        let messages: Vec<&SyncSend> = sends
            .iter()
            .filter(|send| send.packet.msg_type == MessageType::Message.as_u8())
            .collect();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].packet.payload, b"new".to_vec());
    }

    #[test]
    fn the_cursor_boundary_is_inclusive() {
        let mut manager = manager();
        let exactly_at = message(b"at", NOW_MS - 10_000);
        manager.on_public_packet_seen(&exactly_at, NOW);

        let mut request = empty_request(SyncTypeFlags::MESSAGE);
        request.since_timestamp_ms = Some(NOW_MS - 10_000);
        assert_eq!(manager.handle_request_sync(&them(), &request, NOW).len(), 1);

        let mut request = empty_request(SyncTypeFlags::MESSAGE);
        request.since_timestamp_ms = Some(NOW_MS - 9_999);
        assert!(
            manager
                .handle_request_sync(&them(), &request, NOW)
                .is_empty()
        );
    }

    #[test]
    fn a_truncated_filter_carries_a_cursor_and_a_complete_one_does_not() {
        // 32 bytes of filter covers roughly 28 IDs at P=7, well under the 40 held.
        let mut manager = manager_with(SyncConfig {
            gcs_max_bytes: 32,
            ..SyncConfig::default()
        });
        for i in 0..40u64 {
            manager
                .on_public_packet_seen(&message(&[u8::try_from(i).unwrap_or(0)], NOW_MS + i), NOW);
        }

        let request = decode_request(&manager.initial_sync_to(&them(), NOW).expect("request"));
        let since = request
            .since_timestamp_ms
            .expect("a truncated filter needs a cursor");
        // The filter covers a newest-first prefix, so the cursor sits inside the
        // store's range and below its newest entry.
        assert!(since >= NOW_MS);
        assert!(since < NOW_MS + 40);

        // One message fits any budget, so no cursor is needed. Built through
        // `manager_with` because the local binding above shadows the `manager()`
        // helper for the rest of this function.
        let mut manager = manager_with(SyncConfig::default());
        manager.on_public_packet_seen(&message(b"only", NOW_MS), NOW);
        let request = decode_request(&manager.initial_sync_to(&them(), NOW).expect("request"));
        assert_eq!(request.since_timestamp_ms, None);
    }

    // -- expiry ------------------------------------------------------------

    #[test]
    fn public_messages_expire_at_exactly_six_hours() {
        let config = SyncConfig::default();
        assert_eq!(config.public_message_max_age_secs, 6 * 60 * 60);
        let window_ms = config.public_message_max_age_secs * 1000;

        let mut manager = manager();
        let at_boundary = message(b"boundary", NOW_MS - window_ms);
        let past_boundary = message(b"past", NOW_MS - window_ms - 1);

        manager.on_public_packet_seen(&at_boundary, NOW);
        assert_eq!(manager.message_count(), 1, "exactly at the window is fresh");

        manager.on_public_packet_seen(&past_boundary, NOW);
        assert_eq!(manager.message_count(), 1, "one millisecond past is stale");

        // And the boundary packet drops out a millisecond later.
        manager.maintenance(&[], NOW).expect("maintenance");
        assert_eq!(manager.message_count(), 1);
        manager.maintenance(&[], NOW + 1).expect("maintenance");
        assert_eq!(manager.message_count(), 0);
    }

    #[test]
    fn fragments_and_file_transfers_keep_the_short_window() {
        let config = SyncConfig::default();
        assert_eq!(config.max_message_age_secs, 15 * 60);
        let window_ms = config.max_message_age_secs * 1000;

        let mut manager = manager();
        manager.on_public_packet_seen(&fragment([0x01; 8], 0, NOW_MS - window_ms), NOW);
        assert_eq!(manager.fragment_count(), 1);
        manager.on_public_packet_seen(&fragment([0x02; 8], 0, NOW_MS - window_ms - 1), NOW);
        assert_eq!(manager.fragment_count(), 1, "past the window is refused");

        // File transfers share that short window.
        manager.on_public_packet_seen(
            &packet_of(MessageType::FileTransfer, b"at", NOW_MS - window_ms),
            NOW,
        );
        manager.on_public_packet_seen(
            &packet_of(MessageType::FileTransfer, b"past", NOW_MS - window_ms - 1),
            NOW,
        );
        assert_eq!(
            manager
                .handle_request_sync(&them(), &empty_request(SyncTypeFlags::FILE_TRANSFER), NOW)
                .len(),
            1
        );

        // A message of the same age is still carried: the windows differ per type,
        // and that difference is the whole town-crier property.
        manager.on_public_packet_seen(&message(b"old but public", NOW_MS - window_ms - 1), NOW);
        assert_eq!(manager.message_count(), 1);
    }

    #[test]
    fn prekey_bundles_keep_a_day_and_the_boundary_is_exact() {
        // The longest window of the three: a bundle's whole job is being reachable
        // while its owner is off the mesh.
        let window_ms = SyncConfig::default().prekey_bundle_max_age_secs * 1000;
        assert_eq!(window_ms, 24 * 60 * 60 * 1000);

        let mut manager = manager();
        manager.on_public_packet_seen(&bundle_packet([0xE1; 32], NOW_MS - window_ms), NOW);
        manager.on_public_packet_seen(&bundle_packet([0xE2; 32], NOW_MS - window_ms - 1), NOW);

        assert!(manager.has_prekey_bundle(&PeerId::from_noise_key(&[0xE1; 32])));
        assert!(
            !manager.has_prekey_bundle(&PeerId::from_noise_key(&[0xE2; 32])),
            "one millisecond past the window is stale"
        );
    }

    #[test]
    fn an_expired_packet_is_never_served() {
        let mut manager = manager();
        let window_ms = manager.config().public_message_max_age_secs * 1000;
        manager.on_public_packet_seen(&message(b"fresh now", NOW_MS - window_ms), NOW);
        assert_eq!(manager.message_count(), 1);

        // A second later it is outside the window; the store still holds it until
        // maintenance runs, but it must not be handed out.
        let sends =
            manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::MESSAGE), NOW + 1);
        assert!(sends.is_empty(), "a stale packet is not a sync candidate");
    }

    // -- fragment-ID filter ------------------------------------------------

    #[test]
    fn a_fragment_filter_restricts_the_diff_and_bypasses_the_cursor() {
        let mut manager = manager_with(SyncConfig {
            fragment_capacity: 10,
            ..SyncConfig::default()
        });

        let wanted = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let other = [0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18];
        manager.on_public_packet_seen(&fragment(wanted, 1, NOW_MS - 60_000), NOW);
        manager.on_public_packet_seen(&fragment(other, 2, NOW_MS), NOW);

        let mut request = empty_request(SyncTypeFlags::FRAGMENT);
        // A cursor past everything: without the filter this yields nothing.
        request.since_timestamp_ms = Some(NOW_MS + 1);
        request.fragment_id_filter = encode_fragment_id_filter(&[wanted]);

        let sends = manager.handle_request_sync(&them(), &request, NOW);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].packet.msg_type, MessageType::Fragment.as_u8());
        assert_eq!(sends[0].packet.payload.get(..8), Some(&wanted[..]));
        assert_eq!(sends[0].packet.ttl, 0);
        assert!(sends[0].packet.is_rsr);
    }

    #[test]
    fn a_request_naming_no_stream_we_hold_gets_nothing() {
        let mut manager = manager();
        manager.on_public_packet_seen(&fragment([0x01; 8], 0, NOW_MS), NOW);

        let mut request = empty_request(SyncTypeFlags::FRAGMENT);
        request.fragment_id_filter = encode_fragment_id_filter(&[[0x99; 8]]);
        assert!(
            manager
                .handle_request_sync(&them(), &request, NOW)
                .is_empty()
        );
    }

    #[test]
    fn a_fragment_shorter_than_a_stream_id_belongs_to_no_stream() {
        let mut manager = manager();
        // Malformed, but it arrived over the radio and must not panic anything.
        manager.on_public_packet_seen(
            &packet_of(MessageType::Fragment, &[0x01, 0x02], NOW_MS),
            NOW,
        );
        assert_eq!(manager.fragment_count(), 1);

        let mut request = empty_request(SyncTypeFlags::FRAGMENT);
        request.fragment_id_filter = encode_fragment_id_filter(&[[0x01; 8]]);
        assert!(
            manager
                .handle_request_sync(&them(), &request, NOW)
                .is_empty()
        );

        // Untargeted, it is served like any other fragment.
        assert_eq!(
            manager
                .handle_request_sync(&them(), &empty_request(SyncTypeFlags::FRAGMENT), NOW)
                .len(),
            1
        );
    }

    #[test]
    fn a_targeted_request_names_at_most_sixty_streams_and_fits_the_cap() {
        let mut manager = manager();
        let ids: Vec<[u8; 8]> = (0..100u8).map(|i| [i; 8]).collect();
        let sends = manager
            .request_missing_fragments(&ids, &[them()], NOW)
            .expect("request");
        assert_eq!(sends.len(), 1);

        let request = decode_request(&sends[0]);
        let filter = request
            .fragment_id_filter
            .as_deref()
            .expect("filter present");
        assert_eq!(filter.split(',').count(), MAX_FRAGMENT_ID_FILTER_COUNT);
        // 60 * 17 - 1 = 1019, inside the decoder's 1024-byte acceptance cap. One
        // more ID and the peer would ignore the value entirely.
        assert_eq!(filter.len(), 1019);
        assert!(filter.len() <= 1024);
        assert_eq!(
            request.fragment_ids().map(|ids| ids.len()),
            Some(MAX_FRAGMENT_ID_FILTER_COUNT)
        );
        assert_eq!(
            request.types.map(SyncTypeFlags::from_raw),
            Some(SyncTypeFlags::FRAGMENT)
        );
    }

    #[test]
    fn an_ignored_oversized_filter_still_leaves_a_working_sync() {
        // The decoder drops an oversized 0x06 value and keeps the rest of the
        // request. That must degrade to an untargeted fragment round, not to
        // silence — which is exactly what a `None` filter does here.
        let mut manager = manager();
        manager.on_public_packet_seen(&fragment([0x07; 8], 0, NOW_MS), NOW);

        let mut request = empty_request(SyncTypeFlags::FRAGMENT);
        request.fragment_id_filter = Some("z".repeat(2000));
        let encoded = request.encode().expect("encode");
        let decoded = RequestSyncPacket::decode(&encoded).expect("decode");
        assert_eq!(decoded.fragment_id_filter, None);

        assert_eq!(manager.handle_request_sync(&them(), &decoded, NOW).len(), 1);
    }

    #[test]
    fn targeted_requests_need_both_a_stream_and_a_peer() {
        let mut manager = manager();
        assert!(
            manager
                .request_missing_fragments(&[], &[them()], NOW)
                .expect("no streams")
                .is_empty()
        );
        assert!(
            manager
                .request_missing_fragments(&[[0x01; 8]], &[], NOW)
                .expect("no peers")
                .is_empty(),
            "targeted resync has no broadcast fallback by design"
        );
    }

    // -- intake rules ------------------------------------------------------

    #[test]
    fn directed_traffic_is_never_gossiped() {
        let mut manager = manager();
        let mut directed = message(b"private-ish", NOW_MS);
        directed.recipient_id = Some([0x22; SENDER_ID_LEN]);
        manager.on_public_packet_seen(&directed, NOW);
        assert_eq!(manager.message_count(), 0);

        // An explicit broadcast recipient, and an absent one, are both public.
        let mut broadcast = message(b"everyone", NOW_MS);
        broadcast.recipient_id = Some(BROADCAST_RECIPIENT);
        manager.on_public_packet_seen(&broadcast, NOW);
        manager.on_public_packet_seen(&message(b"no recipient", NOW_MS), NOW);
        assert_eq!(manager.message_count(), 2);
    }

    #[test]
    fn unknown_and_non_syncable_types_are_ignored() {
        let mut manager = manager();
        let mut unknown = message(b"future", NOW_MS);
        unknown.msg_type = 0x7F;
        manager.on_public_packet_seen(&unknown, NOW);
        manager.on_public_packet_seen(&packet_of(MessageType::VoiceFrame, b"audio", NOW_MS), NOW);
        manager.on_public_packet_seen(
            &packet_of(MessageType::CourierEnvelope, b"sealed", NOW_MS),
            NOW,
        );
        assert_eq!(manager.message_count(), 0);
        assert!(
            manager
                .handle_request_sync(&them(), &empty_request(SyncTypeFlags::PUBLIC_MESSAGES), NOW)
                .is_empty()
        );
    }

    #[test]
    fn the_store_is_bounded_and_evicts_oldest_first() {
        let mut manager = manager_with(SyncConfig {
            seen_capacity: 3,
            ..SyncConfig::default()
        });
        for i in 0..5u8 {
            manager.on_public_packet_seen(&message(&[i], NOW_MS + u64::from(i)), NOW);
        }
        assert_eq!(manager.message_count(), 3);

        let payloads: Vec<Vec<u8>> = manager
            .public_message_packets(NOW)
            .into_iter()
            .map(|packet| packet.payload)
            .collect();
        assert_eq!(payloads, vec![vec![2], vec![3], vec![4]]);
    }

    #[test]
    fn re_seeing_a_packet_does_not_move_it_up_the_queue() {
        let mut manager = manager_with(SyncConfig {
            seen_capacity: 2,
            ..SyncConfig::default()
        });
        let first = message(b"first", NOW_MS);
        manager.on_public_packet_seen(&first, NOW);
        manager.on_public_packet_seen(&message(b"second", NOW_MS + 1), NOW);
        // A relayed copy of the first, with a decremented ttl.
        manager.on_public_packet_seen(
            &BitchatPacket {
                ttl: 3,
                ..first.clone()
            },
            NOW,
        );
        manager.on_public_packet_seen(&message(b"third", NOW_MS + 2), NOW);

        let payloads: Vec<Vec<u8>> = manager
            .public_message_packets(NOW)
            .into_iter()
            .map(|packet| packet.payload)
            .collect();
        assert_eq!(
            payloads,
            vec![b"second".to_vec(), b"third".to_vec()],
            "an endlessly relayed packet must not pin itself in the store"
        );
    }

    // -- announces and peer removal ---------------------------------------

    #[test]
    fn a_stale_announce_evicts_the_peer_rather_than_being_stored() {
        let mut manager = manager_with(SyncConfig {
            stale_peer_timeout_secs: 60,
            ..SyncConfig::default()
        });

        manager.on_public_packet_seen(&message(b"theirs", NOW_MS), NOW);
        manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS), NOW);
        assert!(manager.has_announcement(&PeerId::from_bytes(&sender())));

        // An announce older than the stale timeout says the peer left.
        manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS - 61_000), NOW);
        assert!(!manager.has_announcement(&PeerId::from_bytes(&sender())));
        assert_eq!(manager.message_count(), 0, "their carried traffic goes too");
    }

    #[test]
    fn stale_cleanup_runs_on_its_own_interval() {
        let mut manager = manager_with(SyncConfig {
            stale_peer_cleanup_interval_secs: 60,
            stale_peer_timeout_secs: 30,
            ..SyncConfig::default()
        });

        manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS), NOW);
        // First tick records the cleanup time.
        manager.maintenance(&[], NOW).expect("maintenance");
        assert!(manager.has_announcement(&PeerId::from_bytes(&sender())));

        // 40 s later the announce is stale, but the cleanup interval has not
        // elapsed, so it survives.
        manager.maintenance(&[], NOW + 40).expect("maintenance");
        assert!(manager.has_announcement(&PeerId::from_bytes(&sender())));

        manager.maintenance(&[], NOW + 60).expect("maintenance");
        assert!(!manager.has_announcement(&PeerId::from_bytes(&sender())));
    }

    #[test]
    fn removing_public_messages_spares_everything_else() {
        let mut manager = manager();
        manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS), NOW);
        manager.on_public_packet_seen(&message(b"blocked", NOW_MS), NOW);
        manager.on_public_packet_seen(&fragment([0x03; 8], 0, NOW_MS), NOW);

        let blocked = PeerId::from_bytes(&sender());
        manager
            .remove_public_messages_from(&blocked, NOW)
            .expect("remove");
        assert_eq!(manager.message_count(), 0);
        assert_eq!(manager.fragment_count(), 1);
        assert!(manager.has_announcement(&blocked));
    }

    #[test]
    fn removing_a_peer_keeps_its_prekey_bundle() {
        let mut manager = manager();
        let owner_key = [0xAB; 32];
        manager.on_public_packet_seen(&bundle_packet(owner_key, NOW_MS), NOW);
        manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS), NOW);

        let owner = PeerId::from_noise_key(&owner_key);
        manager.remove_announcement_for_peer(&PeerId::from_bytes(&sender()));
        assert!(
            manager.has_prekey_bundle(&owner),
            "bundles exist to reach owners who left"
        );
    }

    // -- prekey bundles ----------------------------------------------------

    fn bundle_packet(owner_static: [u8; 32], timestamp_ms: u64) -> BitchatPacket {
        let bundle = PrekeyBundle {
            owner_static,
            prekeys: vec![Prekey {
                id: 0,
                public_key: [0x11; 32],
            }],
            generated_at: timestamp_ms,
            signature: [0u8; 64],
        };
        packet_of(
            MessageType::PrekeyBundle,
            &bundle.encode().expect("bundle encodes"),
            timestamp_ms,
        )
    }

    #[test]
    fn bundles_are_keyed_by_owner_not_by_sender_id() {
        // One valid bundle sprayed under many fabricated sender IDs must collapse to
        // a single entry, or the per-owner cap becomes a way to starve real bundles.
        let mut manager = manager();
        let owner_key = [0xCD; 32];
        let payload = bundle_packet(owner_key, NOW_MS).payload;

        for i in 0..5u64 {
            let mut packet = BitchatPacket::new(
                MessageType::PrekeyBundle,
                [u8::try_from(i).unwrap_or(0); SENDER_ID_LEN],
                None,
                NOW_MS + i,
                payload.clone(),
                7,
            );
            packet.recipient_id = Some(BROADCAST_RECIPIENT);
            manager.on_public_packet_seen(&packet, NOW);
            assert!(
                !manager.has_prekey_bundle(&PeerId::from_bytes(
                    &[u8::try_from(i).unwrap_or(0); SENDER_ID_LEN]
                )),
                "no fabricated sender ID gets its own entry"
            );
        }
        assert!(manager.has_prekey_bundle(&PeerId::from_noise_key(&owner_key)));
    }

    #[test]
    fn a_newer_bundle_replaces_an_older_one_and_a_replay_does_not() {
        let mut manager = manager();
        let owner_key = [0xCE; 32];
        manager.on_public_packet_seen(&bundle_packet(owner_key, NOW_MS), NOW);
        manager.on_public_packet_seen(&bundle_packet(owner_key, NOW_MS - 1000), NOW);

        let sends =
            manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::PREKEY_BUNDLE), NOW);
        assert_eq!(sends.len(), 1);
        assert_eq!(
            sends[0].packet.timestamp, NOW_MS,
            "the older copy is refused"
        );

        manager.on_public_packet_seen(&bundle_packet(owner_key, NOW_MS + 1000), NOW);
        let sends =
            manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::PREKEY_BUNDLE), NOW);
        assert_eq!(sends[0].packet.timestamp, NOW_MS + 1000);
    }

    #[test]
    fn an_undecodable_bundle_is_dropped() {
        let mut manager = manager();
        manager.on_public_packet_seen(
            &packet_of(MessageType::PrekeyBundle, b"not a bundle", NOW_MS),
            NOW,
        );
        assert!(
            manager
                .handle_request_sync(&them(), &empty_request(SyncTypeFlags::PREKEY_BUNDLE), NOW)
                .is_empty()
        );
    }

    #[test]
    fn the_owner_cap_bounds_the_bundle_store() {
        let mut manager = manager_with(SyncConfig {
            prekey_bundle_capacity: 2,
            ..SyncConfig::default()
        });
        for i in 0..4u8 {
            manager.on_public_packet_seen(&bundle_packet([0xD0 + i; 32], NOW_MS), NOW);
        }
        let sends =
            manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::PREKEY_BUNDLE), NOW);
        assert_eq!(sends.len(), 2);

        // A refresh from a known owner still lands: the cap must never block one.
        manager.on_public_packet_seen(&bundle_packet([0xD0; 32], NOW_MS + 5), NOW);
        let sends =
            manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::PREKEY_BUNDLE), NOW);
        assert_eq!(sends.len(), 2);
        assert!(sends.iter().any(|send| send.packet.timestamp == NOW_MS + 5));
    }

    // -- type routing ------------------------------------------------------

    #[test]
    fn a_round_answers_only_the_types_it_asked_for() {
        let mut manager = manager();
        manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);
        manager.on_public_packet_seen(&fragment([0x04; 8], 0, NOW_MS), NOW);
        manager.on_public_packet_seen(&packet_of(MessageType::FileTransfer, b"f", NOW_MS), NOW);
        manager.on_public_packet_seen(&packet_of(MessageType::GroupMessage, b"g", NOW_MS), NOW);

        let only_fragments =
            manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::FRAGMENT), NOW);
        assert_eq!(only_fragments.len(), 1);
        assert_eq!(
            only_fragments[0].packet.msg_type,
            MessageType::Fragment.as_u8()
        );

        let messages_and_groups = manager.handle_request_sync(
            &them(),
            &empty_request(SyncTypeFlags::PUBLIC_MESSAGES.union(SyncTypeFlags::GROUP_MESSAGE)),
            NOW,
        );
        assert_eq!(messages_and_groups.len(), 2);
    }

    #[test]
    fn a_request_with_no_type_field_means_the_default_round() {
        let mut manager = manager();
        manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);
        manager.on_public_packet_seen(&packet_of(MessageType::Announce, b"", NOW_MS), NOW);
        manager.on_public_packet_seen(&fragment([0x05; 8], 0, NOW_MS), NOW);

        let mut request = empty_request(SyncTypeFlags::MESSAGE);
        request.types = None;
        let sends = manager.handle_request_sync(&them(), &request, NOW);
        // announce + message, and no fragment.
        assert_eq!(sends.len(), 2);
        assert!(
            sends
                .iter()
                .all(|send| send.packet.msg_type != MessageType::Fragment.as_u8())
        );
    }

    #[test]
    fn maintenance_emits_one_request_per_due_schedule() {
        let mut manager = manager_with(SyncConfig {
            message_sync_interval_secs: 1,
            fragment_sync_interval_secs: 1,
            file_transfer_sync_interval_secs: 1,
            prekey_bundle_sync_interval_secs: 1,
            board_sync_interval_secs: 1,
            ..SyncConfig::default()
        });

        // No board source wired: that round is skipped entirely.
        let sends = manager.maintenance(&[them()], NOW).expect("maintenance");
        assert_eq!(sends.len(), 4);

        let types: Vec<SyncTypeFlags> = sends
            .iter()
            .map(|send| SyncTypeFlags::from_raw(decode_request(send).types.unwrap_or_default()))
            .collect();
        assert!(
            types.contains(&SyncTypeFlags::PUBLIC_MESSAGES.union(SyncTypeFlags::GROUP_MESSAGE))
        );
        assert!(types.contains(&SyncTypeFlags::FRAGMENT));
        assert!(types.contains(&SyncTypeFlags::FILE_TRANSFER));
        assert!(types.contains(&SyncTypeFlags::PREKEY_BUNDLE));

        // Wiring a board source adds its round.
        manager.set_board_packets(Vec::new());
        let sends = manager
            .maintenance(&[them()], NOW + 10)
            .expect("maintenance");
        assert_eq!(sends.len(), 5);
    }

    #[test]
    fn a_schedule_only_fires_once_per_interval() {
        let mut manager = manager_with(SyncConfig {
            message_sync_interval_secs: 15,
            fragment_sync_interval_secs: 0,
            file_transfer_sync_interval_secs: 0,
            prekey_bundle_sync_interval_secs: 0,
            board_sync_interval_secs: 0,
            ..SyncConfig::default()
        });

        assert_eq!(manager.maintenance(&[them()], NOW).expect("tick").len(), 1);
        assert_eq!(
            manager
                .maintenance(&[them()], NOW + 14)
                .expect("tick")
                .len(),
            0
        );
        assert_eq!(
            manager
                .maintenance(&[them()], NOW + 15)
                .expect("tick")
                .len(),
            1
        );
    }

    #[test]
    fn every_connected_peer_gets_its_own_unicast_request() {
        let mut manager = manager_with(SyncConfig {
            fragment_sync_interval_secs: 0,
            file_transfer_sync_interval_secs: 0,
            prekey_bundle_sync_interval_secs: 0,
            board_sync_interval_secs: 0,
            ..SyncConfig::default()
        });

        let peers = [them(), peer("1122334455667788")];
        let sends = manager.maintenance(&peers, NOW).expect("maintenance");
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0].to, Some(peers[0].clone()));
        assert_eq!(sends[1].to, Some(peers[1].clone()));
        // Both windows are open, which is what makes both sets of answers usable.
        assert_eq!(manager.pending_request_count(), 2);
        assert!(manager.is_valid_response(&peers[0], true, NOW));
        assert!(manager.is_valid_response(&peers[1], true, NOW));
    }

    #[test]
    fn the_initial_request_coalesces_every_enabled_type() {
        let mut manager = manager();
        manager.set_board_packets(Vec::new());
        let request = decode_request(&manager.initial_sync_to(&them(), NOW).expect("request"));
        let types = SyncTypeFlags::from_raw(request.types.expect("types present"));
        for expected in [
            MessageType::Announce,
            MessageType::Message,
            MessageType::GroupMessage,
            MessageType::Fragment,
            MessageType::FileTransfer,
            MessageType::PrekeyBundle,
            MessageType::BoardPost,
        ] {
            assert!(types.contains(expected), "{expected:?} must be asked for");
        }
    }

    #[test]
    fn board_posts_are_served_from_the_injected_source() {
        let mut manager = manager();
        let post = packet_of(
            MessageType::BoardPost,
            b"post",
            NOW_MS - 10 * 24 * 3600 * 1000,
        );
        manager.set_board_packets(vec![post.clone()]);

        // No freshness window applies: the board store owns retention, so a post
        // older than any sync window is still served until its own expiry.
        let sends = manager.handle_request_sync(&them(), &empty_request(SyncTypeFlags::BOARD), NOW);
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].packet.payload, post.payload);
        assert!(sends[0].packet.is_rsr);

        // The cursor still applies to them.
        let mut request = empty_request(SyncTypeFlags::BOARD);
        request.since_timestamp_ms = Some(NOW_MS);
        assert!(
            manager
                .handle_request_sync(&them(), &request, NOW)
                .is_empty()
        );
    }

    // -- hostile input -----------------------------------------------------

    #[test]
    fn a_degenerate_filter_is_read_as_holding_nothing() {
        let mut manager = manager();
        manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);

        // m = 1 decodes to the empty set; garbage data decodes to whatever it
        // decodes to. Neither may panic, and both must leave us willing to serve.
        for (p, m, data) in [
            (1u8, 1u32, vec![]),
            (32, 1, vec![0xFF; 64]),
            (7, u32::MAX, vec![0xFF; 400]),
            (7, 2, vec![0x00; 8]),
        ] {
            let request = RequestSyncPacket {
                p,
                m,
                data,
                types: Some(SyncTypeFlags::MESSAGE.raw()),
                since_timestamp_ms: None,
                fragment_id_filter: None,
            };
            let _ = manager.handle_request_sync(&them(), &request, NOW);
        }
    }

    #[test]
    fn an_empty_store_still_produces_a_well_formed_request() {
        let mut manager = manager();
        let send = manager.initial_sync_to(&them(), NOW).expect("request");
        let request = decode_request(&send);
        assert_eq!(request.m, 1, "m = 1 means: I hold nothing");
        assert!(request.data.is_empty());
        assert_eq!(request.since_timestamp_ms, None);
        assert_eq!(send.packet.ttl, 0);
    }

    #[test]
    fn a_far_future_timestamp_is_carried_rather_than_purged() {
        // Freshness only has a lower bound, matching Swift. A packet claiming a
        // timestamp from next year is not evidence of anything we can act on here;
        // the receive pipeline's ±2-minute check is where that is caught.
        let mut manager = manager();
        manager.on_public_packet_seen(&message(b"future", NOW_MS + 86_400_000), NOW);
        assert_eq!(manager.message_count(), 1);
    }

    #[test]
    fn a_zero_clock_accepts_everything_rather_than_purging_everything() {
        let mut manager = GossipSyncManager::new(&us(), SyncConfig::default(), None, 0);
        manager.on_public_packet_seen(&message(b"epoch", 0), 0);
        assert_eq!(manager.message_count(), 1);
    }

    // -- archive integration ----------------------------------------------

    #[test]
    fn public_messages_survive_a_restart() {
        let storage = Arc::new(MemoryStorage::new());

        let mut first = GossipSyncManager::new(
            &us(),
            SyncConfig::default(),
            Some(GossipMessageArchive::new(storage.clone())),
            NOW,
        );
        first.on_public_packet_seen(&message(b"carried across", NOW_MS), NOW);
        first.persist_if_dirty(NOW).expect("persist");

        // A new process, an hour later: the message is still inside its six-hour
        // window, so the device can still hand it to a partition that missed it.
        let second = GossipSyncManager::new(
            &us(),
            SyncConfig::default(),
            Some(GossipMessageArchive::new(storage.clone())),
            NOW + 3600,
        );
        assert_eq!(second.message_count(), 1);
        assert_eq!(
            second.public_message_packets(NOW + 3600)[0].payload,
            b"carried across".to_vec()
        );
    }

    #[test]
    fn the_archive_drops_what_aged_out_while_the_process_was_dead() {
        let storage = Arc::new(MemoryStorage::new());
        let mut first = GossipSyncManager::new(
            &us(),
            SyncConfig::default(),
            Some(GossipMessageArchive::new(storage.clone())),
            NOW,
        );
        first.on_public_packet_seen(&message(b"old", NOW_MS), NOW);
        first.persist_if_dirty(NOW).expect("persist");

        let window = SyncConfig::default().public_message_max_age_secs;
        let second = GossipSyncManager::new(
            &us(),
            SyncConfig::default(),
            Some(GossipMessageArchive::new(storage.clone())),
            NOW + window + 1,
        );
        assert_eq!(second.message_count(), 0);
    }

    #[test]
    fn only_whole_public_messages_are_archived() {
        let storage = Arc::new(MemoryStorage::new());
        let mut manager = GossipSyncManager::new(
            &us(),
            SyncConfig::default(),
            Some(GossipMessageArchive::new(storage.clone())),
            NOW,
        );
        manager.on_public_packet_seen(&fragment([0x06; 8], 0, NOW_MS), NOW);
        manager.on_public_packet_seen(&packet_of(MessageType::FileTransfer, b"f", NOW_MS), NOW);
        manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);
        manager.persist_if_dirty(NOW).expect("persist");

        let archive = GossipMessageArchive::new(storage.clone());
        let stored = archive.load_packets();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].msg_type, MessageType::Message.as_u8());
    }

    #[test]
    fn a_blocked_sender_is_purged_from_disk_immediately() {
        // Not at the next tick: a relaunch in that gap would restore them.
        let storage = Arc::new(MemoryStorage::new());
        let mut manager = GossipSyncManager::new(
            &us(),
            SyncConfig::default(),
            Some(GossipMessageArchive::new(storage.clone())),
            NOW,
        );
        manager.on_public_packet_seen(&message(b"blocked", NOW_MS), NOW);
        manager.persist_if_dirty(NOW).expect("persist");
        assert_eq!(GossipMessageArchive::new(storage.clone()).load().len(), 1);

        manager
            .remove_public_messages_from(&PeerId::from_bytes(&sender()), NOW)
            .expect("remove");
        assert!(
            GossipMessageArchive::new(storage.clone()).load().is_empty(),
            "the purge must reach disk before anything can restore it"
        );
    }

    #[test]
    fn persisting_is_idempotent_when_nothing_changed() {
        let storage = Arc::new(MemoryStorage::new());
        let mut manager = GossipSyncManager::new(
            &us(),
            SyncConfig::default(),
            Some(GossipMessageArchive::new(storage.clone())),
            NOW,
        );
        manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);
        manager.persist_if_dirty(NOW).expect("persist");
        let first = storage.load(StorageKey::PublicArchive).expect("load");

        manager.persist_if_dirty(NOW).expect("persist again");
        let second = storage.load(StorageKey::PublicArchive).expect("load");
        assert_eq!(first, second);
    }

    #[test]
    fn a_driver_without_an_archive_still_works() {
        let mut manager = manager();
        manager.on_public_packet_seen(&message(b"m", NOW_MS), NOW);
        manager
            .persist_if_dirty(NOW)
            .expect("no archive is not an error");
        assert_eq!(manager.message_count(), 1);
    }
}
