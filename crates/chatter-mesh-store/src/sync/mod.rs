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

mod config;
mod limits;
mod packet_store;
mod request;
mod response;
#[cfg(test)]
mod tests;

pub use config::SyncConfig;
pub use limits::{RequestSyncTracker, SyncResponseRateLimiter};
use packet_store::PacketStore;

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

/// How old a peer's latest announce may be before the peer counts as gone and
/// the store purges the public traffic it authored.
///
/// bitchat-android's `STALE_PEER_TIMEOUT_MS` (`AppConstants.kt:15`), not Swift's
/// 60 s (`TransportConfig.syncStalePeerTimeoutSeconds`). Swift announces every
/// 4 s (`bleAnnounceIntervalSeconds`), so a minute is many announces there. Android
/// announces every 30 s in the foreground and every 60, 120 or 300 s in the
/// background or on low battery (`PowerManager.kt:338-345`). A 60 s timeout swept
/// every 60 s therefore purged a present Android peer's messages between two of
/// its announces, and the next sync round fetched them again.
pub const STALE_PEER_TIMEOUT_SECS: u64 = 180;

/// Tunables for one sync driver. Defaults are bitchat's shipping values.

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

    /// Whether an inbound RSR packet answers something we actually asked `from`.
    ///
    /// The receive pipeline calls this before granting the freshness exemption. A
    /// false answer here is what stops a captured response being replayed forever.
    pub fn is_valid_response(&self, from: &PeerId, is_rsr: bool, now_unix_secs: u64) -> bool {
        self.requests.is_valid_response(from, is_rsr, now_unix_secs)
    }

    /// Whether we have a request out to `from`, for an answer without the RSR
    /// flag. bitchat-android answers with TTL 0 and never sets RSR.
    pub fn awaits_answer_from(&self, from: &PeerId, now_unix_secs: u64) -> bool {
        self.requests.awaits_answer_from(from, now_unix_secs)
    }

    /// Whether `packet` is inside the retention window this store keeps for its
    /// type. One-sided, like the store: a future timestamp is inside.
    pub fn within_retention(&self, packet: &BitchatPacket, now_unix_secs: u64) -> bool {
        packet_is_fresh(&self.config, packet, now_unix_secs)
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
