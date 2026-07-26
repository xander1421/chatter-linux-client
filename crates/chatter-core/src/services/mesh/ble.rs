//! The radio runtime: the loop that drives the mesh.
//!
//! Every crate below this one is sans-IO. `chatter-mesh` parses and decides,
//! `chatter-mesh-link` schedules and plans, `chatter-mesh-store` retains and
//! syncs — and not one of them owns a clock, a task, or a socket. Something has
//! to read radio events, feed them through that policy, and put the resulting
//! bytes back on the air on a timer. This is that something.
//!
//! # Three inputs, one loop
//!
//! Link events, service commands, and a one-second tick, multiplexed so that no
//! two of them can touch the policy state concurrently. The tick is what makes
//! the duty cycle, the announce cadence, gossip sync, and outbox maintenance
//! happen at all: none of those layers can schedule themselves.

use std::sync::Arc;
use std::time::Duration;

use chatter_mesh::codec;
use chatter_mesh::dedup::MessageDeduplicator;
use chatter_mesh::fragment;
use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{
    BitchatPacket, MessageType, NoisePayloadType, SENDER_ID_LEN,
    decode_noise_payload, encode_noise_payload,
};
use chatter_mesh::relay::{self, RelayInput};
use chatter_mesh::tlv::announce::AnnouncementPacket;
use chatter_mesh::tlv::capabilities::PeerCapabilities;
use chatter_mesh::tlv::courier_envelope::CourierEnvelope;
use chatter_mesh::tlv::file::BitchatFilePacket;
use chatter_mesh::tlv::file_stream::{
    FileChunk, FileChunkRequest, FileManifest, MAX_REQUESTED_CHUNKS,
};
use chatter_mesh::tlv::hex_encode;
use chatter_mesh::tlv::private_message::PrivateMessagePacket;
use chatter_mesh::tlv::request_sync::RequestSyncPacket;
use chatter_mesh_link::backpressure::{
    InboundAppend, InboundChunk, InboundLimits, PENDING_WRITE_BUFFER_CAP_BYTES, should_buffer, WriteEnqueue,
};
use chatter_mesh_link::duty::{self, ScanCommand};
use chatter_mesh_link::guard::{IngressGuard, IngressInput};
use chatter_mesh_link::planner::{self, DIRECTED_SPOOL_WINDOW_SECS, OutboundPlanRequest};
use chatter_mesh_link::registry::VerifiedDirectAnnounce;
use chatter_mesh_link::scheduler::{
    DiscoveryDecision, ExistingConnectionState, MAX_CENTRAL_LINKS, PeripheralConnectionState,
    QueueDecision,
};
use chatter_mesh_link::{LinkEvent, LinkId, LinkRole, MeshLink};
use chatter_mesh_noise::seal::open_courier;
use chatter_mesh_noise::sign::{sign_packet, verify_packet};
use chatter_mesh_noise::{MeshIdentity, NoiseError};
use chatter_mesh_store::CourierTier;
use chatter_mesh_store::archive::GossipMessageArchive;
use chatter_mesh_store::courier::{AnnounceIngress, CourierStore};
use chatter_mesh_store::outbox::AckKind;
use chatter_mesh_store::sync::{GossipSyncManager, SyncConfig};
use tokio::sync::mpsc::UnboundedReceiver;

use super::config::MeshConfig;
use super::events::{MeshEvent, MeshMessage};
use super::file_stream::{
    CHUNK_REQUEST_QUIET_SECS, ChunkOutcome, FileStreamSpool, ManifestOutcome, OutboundTransfer,
    TRANSFER_ABANDON_SECS, TransferId,
};
use super::glue::SharedStorage;
use super::mapping::{self, InboundMessage, MeshConversation, outbound_message};
use super::router::{RouterCommand, RouterHandle};
use super::state::{MeshState, NostrState, link_candidates_locked, now_millis, now_secs};
use super::timeline::MeshEvents;
use super::transport::BleCommand;

/// How often the loop wakes to run the parts that nothing else can schedule.
const TICK: Duration = Duration::from_millis(500);

/// How soon to retry a frame the radio refused for being busy.
///
/// One ATT operation may be outstanding per link — Android's bridge enforces
/// that with an `OpGate` and reports the second write as retryable, and BlueZ
/// fills its controller queue the same way. The refused frame goes to
/// [`OutboundWriteBuffer`], and *when it is retried* is what sets throughput.
///
/// Measured on real hardware before this existed: the retry only happened on
/// the next [`TICK`], so exactly one 469-byte frame crossed per 500 ms — 0.9
/// kB/s, with the radio idle for 98% of it. A 32 KiB file is ~70 frames, so it
/// took 36 s to send and the receiver's 30 s reassembly window had already
/// dropped it. Silently: an incomplete reassembly is evicted without a log, so
/// the file simply never appeared. Nothing at the single-packet cap could ever
/// arrive.
///
/// 20 ms is a connection interval or two — long enough not to spin against a
/// gate that is still closed, short enough that the radio's own completion rate
/// becomes the limit again, which is where the limit belongs. This is a floor
/// on retry latency, not a rate: the loop only arms it while frames are queued.
const WRITE_RETRY_DELAY: Duration = Duration::from_millis(20);

/// The TTL an announce still carries when it has not been relayed.
///
/// Only ever a *hint*: the TTL is excluded from the signature preimage so relays
/// can decrement it, which means anyone can set it back and look adjacent.
const DIRECT_ANNOUNCE_TTL: u8 = chatter_mesh::relay::MESSAGE_TTL_DEFAULT;

/// Chunks of a streamed file put on the air per tick, per live transfer.
///
/// One, and the [`TICK`] is 500 ms, so a transfer offers ~64 kB/s to the write
/// queue — comfortably more than the radio can take, which is the point: the
/// queue is what paces this, not the loop. Emitting the whole file in one pass
/// (as the single-packet path does, by construction) would block the loop from
/// servicing link events for the entire transfer, which for a 16 MiB file is
/// minutes of not noticing a peer connect or disconnect.
const CHUNKS_PER_TICK: usize = 1;

/// Measured air throughput for one link, in bytes of payload per second.
///
/// A calibration figure, not a constant of nature, and deliberately set from a
/// *sustained* measurement rather than a burst.
///
/// A short isolated 32 KiB file crosses at ~21.5 kB/s, which is the figure this
/// started at. A 2 MiB transfer measured end to end came in at 10.5 kB/s — 3.11 s
/// per chunk against a 1.64 s budget — because a real transfer shares the link
/// with announces, gossip sync and the receiver's own traffic for its whole
/// duration, and a burst does not. The sustained number is the one that matters:
/// pacing under the real rate costs a little time, pacing over it costs the whole
/// transfer, because the excess is discarded rather than queued.
///
/// It exists because BlueZ gives no backpressure when it sends notifications.
/// Android's bridge refuses a second concurrent write and the layer above queues
/// and retries; BlueZ's `try_send` accepted 36,800 consecutive frames without one
/// refusal while the link could carry roughly a third of them. A sender that
/// waits to be told to slow down is never told, so it has to know.
///
/// Tune this if the radio or the platform changes. Too low is slow; too high is
/// silent loss.
const AIR_BYTES_PER_SEC: u64 = 10_000;

/// Queued outbound bytes above which a transfer skips its turn.
///
/// Backpressure the loop can see. `OutboundWriteBuffer` already trims from the
/// low-priority tail when it hits [`PENDING_WRITE_BUFFER_CAP_BYTES`], and a file
/// chunk *is* the low-priority tail — so pushing chunks into a full queue would
/// mostly evict earlier chunks of the same transfer. Better not to generate them.
const CHUNK_PACING_QUEUE_BYTES: usize = PENDING_WRITE_BUFFER_CAP_BYTES / 4;

/// How often a live transfer's manifest goes back on the air.
///
/// A manifest is the only thing that lets a peer make sense of the chunks it is
/// hearing, so a peer that arrives mid-transfer needs one — and there is no
/// request/response to fetch it with. Sixty seconds is roughly the announce
/// cadence: the same reasoning, and the same cost, as saying "I'm here" again.
const MANIFEST_REPEAT_SECS: u64 = 60;

/// The radio loop.
pub struct BleRuntime {
    radio: Arc<dyn MeshLink>,
    state: Arc<MeshState>,
    /// Read only, so a status event reports both transports rather than
    /// resetting the relay count to zero every time a link changes.
    nostr_state: Arc<NostrState>,
    identity: Arc<MeshIdentity>,
    config: Arc<MeshConfig>,
    events: MeshEvents,
    router: Arc<RouterHandle>,
    gossip: GossipSyncManager,
    couriers: CourierStore<SharedStorage>,
    guard: IngressGuard,
    /// Chunked transfers being received. Owned by the loop rather than by
    /// [`MeshState`], because every method on it touches disk and `MeshState` is
    /// behind a `parking_lot::Mutex` that must never be held across IO.
    files_in: FileStreamSpool,
    /// Chunked transfers being sent, keyed by transfer id. Each holds a path and
    /// a cursor, never the file's bytes.
    files_out: std::collections::BTreeMap<TransferId, OutboundTransfer>,
}

impl BleRuntime {
    #[must_use]
    pub fn new(
        radio: Arc<dyn MeshLink>,
        state: Arc<MeshState>,
        nostr_state: Arc<NostrState>,
        identity: Arc<MeshIdentity>,
        config: Arc<MeshConfig>,
        events: MeshEvents,
        router: Arc<RouterHandle>,
        storage: SharedStorage,
    ) -> Self {
        let now = now_secs();
        let archive = GossipMessageArchive::new(Arc::new(storage.clone()));
        let gossip = GossipSyncManager::new(
            &identity.peer_id(),
            SyncConfig::default(),
            Some(archive),
            now,
        );
        let couriers = CourierStore::new(storage);
        Self {
            radio,
            state,
            nostr_state,
            identity,
            config,
            events,
            router,
            gossip,
            couriers,
            guard: IngressGuard::default(),
            // Restored here, not on the first chunk: a transfer interrupted by the
            // process ending is exactly the case the persisted bitmap exists for,
            // and the runtime has to know about it before the sender's next
            // manifest arrives so that manifest resumes rather than restarts.
            files_in: FileStreamSpool::restored(),
            files_out: std::collections::BTreeMap::new(),
        }
    }

    /// Start the radio and run until the command channel or the radio closes.
    ///
    /// The receiver is a parameter rather than a field so that the `select!`
    /// branch reading it does not borrow the whole runtime, which every handler
    /// needs mutably.
    pub async fn run(mut self, mut commands: UnboundedReceiver<BleCommand>) {
        if let Err(error) = self.radio.start().await {
            self.degraded(format!("the radio would not start: {error}"));
            return;
        }
        self.state.with(|state| state.radio_running = true);
        self.emit_status();
        // Our own ID, said once. Every packet on the wire names its sender by
        // this derivation, so without it an unfamiliar sender in the log cannot
        // even be ruled out as our own traffic coming back.
        tracing::info!(
            target: "mesh::ble",
            local_peer = %self.identity.peer_id(),
            "radio up"
        );

        // Carrying anything already in the courier pool matters more than it
        // looks: a device that forgets its pool on restart drops other people's
        // mail on the floor with no way to recover it.
        if let Err(error) = self.couriers.restore(now_secs()) {
            self.degraded(format!("the courier pool could not be restored: {error}"));
        }

        self.announce().await;

        let radio = Arc::clone(&self.radio);
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let backlogged = !self.state.with(|state| state.writes.is_empty());
            tokio::select! {
                event = radio.next_event() => match event {
                    Some(event) => self.on_link_event(event).await,
                    // The radio has shut down for good.
                    None => break,
                },
                command = commands.recv() => match command {
                    Some(BleCommand::Stop) | None => break,
                    Some(command) => self.on_command(command).await,
                },
                _ = ticker.tick() => self.on_tick().await,
                // Only armed while something is queued, so a quiet mesh still
                // wakes twice a second and no faster.
                _ = tokio::time::sleep(WRITE_RETRY_DELAY), if backlogged => {
                    self.flush_buffered_writes().await;
                }
            }
        }

        let _ = self.radio.stop().await;
        self.state.with(|state| {
            state.radio_running = false;
            state.sessions.clear();
        });
        self.emit_status();
    }

    // ------------------------------------------------------------- link events

    async fn on_link_event(&mut self, event: LinkEvent) {
        match event {
            LinkEvent::Discovered { address, rssi } => self.on_discovered(&address, rssi).await,
            LinkEvent::Connected {
                link,
                role,
                max_write,
            } => self.on_connected(link, role, max_write).await,
            LinkEvent::Disconnected { link } => self.on_disconnected(&link).await,
            LinkEvent::Data { link, bytes } => self.on_data(&link, bytes).await,
            LinkEvent::AdapterPowerChanged { powered } => {
                self.state.with(|state| {
                    state.adapter_powered = powered;
                    if !powered {
                        // Every link dies with the adapter. Keeping the bindings
                        // would make the router believe it can still write.
                        state.links.clear();
                        state.max_write.clear();
                        state.address_of.clear();
                        state.connecting.clear();
                        state.scheduler.reset();
                    }
                });
                self.emit_status();
                self.emit_peers();
            }
        }
    }

    async fn on_discovered(&mut self, address: &str, rssi: i16) {
        let now = now_millis();
        let decision = self.state.with(|state| {
            let candidate =
                chatter_mesh_link::scheduler::ConnectionCandidate::new(address, rssi, now);
            let connected_or_connecting =
                state.links.count_role(LinkRole::Central) + state.connecting.len();
            let existing = ExistingConnectionState {
                is_connecting: state.connecting.contains(address),
                is_connected: state
                    .address_of
                    .values()
                    .any(|known| known.as_str() == address),
                last_connect_attempt_ms: None,
            };
            let peripheral = if state.links.count_role(LinkRole::Peripheral) > 0 {
                PeripheralConnectionState::Connected
            } else {
                PeripheralConnectionState::Disconnected
            };
            state.scheduler.handle_discovery(
                &candidate,
                connected_or_connecting,
                Some(existing),
                peripheral,
                now,
            )
        });

        match decision {
            DiscoveryDecision::ConnectNow => self.dial(address).await,
            DiscoveryDecision::CancelStaleConnection => {
                self.state.with(|state| {
                    state.connecting.remove(address);
                });
            }
            // Queued and retry are both "drain later", which the tick does.
            DiscoveryDecision::Queued
            | DiscoveryDecision::ScheduleRetry { .. }
            | DiscoveryDecision::Ignore => {}
        }
    }

    async fn dial(&mut self, address: &str) {
        let now = now_millis();
        self.state.with(|state| {
            state.connecting.insert(address.to_string());
            state.scheduler.note_connect_attempt(now);
        });
        match self.radio.connect(address).await {
            Ok(link) => {
                self.state.with(|state| {
                    state.address_of.insert(link, address.to_string());
                    state.scheduler.note_connected(address);
                });
            }
            Err(error) => {
                let timed_out = matches!(error, chatter_mesh_link::LinkError::ConnectTimedOut { .. });
                self.state.with(|state| {
                    state.connecting.remove(address);
                    // A timeout earns the long ignore window; an outright failure
                    // earns only the short one. `note_connect_timeout` existed and
                    // was never reachable from here, because the radio layer
                    // reported a timeout as an ordinary failure — so the window it
                    // documents had never once applied.
                    if timed_out {
                        state.scheduler.note_connect_timeout(address, now);
                    } else {
                        state.scheduler.note_connect_failed(address, now);
                    }
                });
                tracing::debug!(target: "mesh::ble", %address, %error, "connect failed");
            }
        }
    }

    async fn on_connected(&mut self, link: LinkId, role: LinkRole, max_write: usize) {
        self.state.with(|state| {
            state.links.link_up(link.clone(), role);
            // `Connected` means writable: the trait's contract is that an
            // implementation withholds this event until the link can carry a
            // write, so the flag is set here rather than waited for.
            state.links.set_writable(&link, true);
            state.max_write.insert(link.clone(), max_write);
            if let Some(address) = state.address_of.get(&link).cloned() {
                state.connecting.remove(&address);
            }
        });
        self.emit_status();
        // Announce straight away: a peer that has just connected knows nothing
        // about us until one arrives, and the cadence timer may be seconds out.
        self.announce().await;
        self.flush_buffered_writes().await;
    }

    async fn on_disconnected(&mut self, link: &LinkId) {
        let now = now_millis();
        let peer = self.state.with(|state| {
            let peer = state.links.link_down(link);
            state.max_write.remove(link);
            state.assembler.discard(link);
            state.writes.discard_all(link);
            if let Some(address) = state.address_of.remove(link) {
                state.connecting.remove(&address);
                state.scheduler.note_disconnected(&address, now);
            }
            // A session outlives its link on purpose — the peer may come back on
            // another one — but the link's Noise authentication does not.
            state.links.clear_noise_authentication(link);
            peer
        });
        self.reconcile_peers();
        self.emit_status();
        if let Some(peer) = peer {
            tracing::debug!(target: "mesh::ble", peer = %peer, "link down");
        }
    }

    async fn on_data(&mut self, link: &LinkId, bytes: Vec<u8>) {
        let now = now_millis();
        let appended = self.state.with(|state| {
            let offset = state.assembler.buffered_bytes(link);
            state.last_traffic_ms = now;
            state.assembler.append(
                link,
                &[InboundChunk { offset, bytes }],
                InboundLimits::default(),
                now,
            )
        });

        match appended {
            InboundAppend::Decoded { packet, .. } => self.on_packet(link, *packet).await,
            InboundAppend::Waiting { .. } => {}
            InboundAppend::Oversized { .. } => {
                tracing::debug!(target: "mesh::ble", link = %link, "oversized inbound frame discarded");
            }
        }
    }

    // ----------------------------------------------------------------- inbound

    async fn on_packet(&mut self, link: &LinkId, packet: BitchatPacket) {
        let now_ms = now_millis();
        let now_s = now_secs();
        let local = self.identity.peer_id();
        let claimed = PeerId::from_bytes(&packet.sender_id);
        let bound = self.state.with(|state| state.links.peer_for(link).cloned());

        // Which peer's outstanding request this could be answering.
        //
        // For a sync response that is the peer on the *link* — the one that
        // forwarded it — not the ID stamped on the packet. A gossip response
        // carries another peer's packet verbatim, so its sender is the original
        // author and can be anybody, including us. Asking "do we have a request
        // out to the author" therefore answered a question nobody had, and every
        // response was rejected as unsolicited: the guard names the link peer in
        // its rejection while this had computed the answer for the author.
        //
        // `IngressGuard::evaluate` picks the same peer for the same reason and
        // passes it to the closure below; the closure cannot use it directly only
        // because `self.gossip` may not be borrowed inside `self.state.with`.
        let validation_peer = if packet.is_rsr {
            bound.clone().unwrap_or_else(|| claimed.clone())
        } else {
            claimed.clone()
        };
        let solicited = self
            .gossip
            .is_valid_response(&validation_peer, packet.is_rsr, now_s);

        // Copied out so the closure below borrows only the state, not the whole
        // runtime.
        let guard = self.guard;
        let accepted = self.state.with(|state| {
            guard.evaluate(
                IngressInput {
                    packet: &packet,
                    claimed_sender: &claimed,
                    bound_peer: bound.as_ref(),
                    local_peer: &local,
                    now_unix_millis: now_ms,
                },
                &state.self_broadcasts,
                |_| solicited,
            )
        });

        let accepted = match accepted {
            Ok(accepted) => accepted,
            Err(rejection) => {
                // `pending` is the whole of the RSR decision: a response is
                // exempt from the freshness check only while we have an
                // outstanding request to that peer. Printing the count next to
                // the rejection is what distinguishes "we never asked" from
                // "we asked and the bookkeeping lost it".
                tracing::debug!(
                    target: "mesh::ble",
                    %rejection,
                    is_rsr = packet.is_rsr,
                    claimed_sender = %claimed,
                    pending_sync_peers = ?self.gossip.pending_request_peers(),
                    "packet rejected at ingress"
                );
                return;
            }
        };

        // Dedup before anything with a side effect. A flooded mesh delivers the
        // same packet on several links, and processing each one duplicates the
        // message and re-relays it.
        let duplicate = self.state.with(|state| {
            let key = MessageDeduplicator::dedup_key(&packet);
            if !MessageDeduplicator::should_deduplicate(&packet, &local) {
                return false;
            }
            if state.dedup.is_duplicate(&key, now_s) {
                return true;
            }
            state.dedup.mark_processed(&key, now_s);
            false
        });
        if duplicate {
            return;
        }

        self.state.with(|state| {
            state.ingress.record_if_new(
                &packet,
                link.clone(),
                accepted.received_from.clone(),
                now_ms,
                chatter_mesh_link::registry::INGRESS_RECORD_LIFETIME_MS,
            );
            state.peers.update_last_seen(&accepted.received_from, now_ms);
        });

        self.dispatch(link, &packet, &claimed, now_s).await;

        if packet.is_broadcast() {
            self.gossip.on_public_packet_seen(&packet, now_s);
        }
        self.relay(link, &packet, &local).await;
    }

    /// Unwrap a fragment, then handle whatever it turns out to be.
    ///
    /// Deliberately not recursive. The fragmenter never nests fragments, so one
    /// level of unwrapping is the whole truth — and expressing it as one level
    /// means a hostile peer cannot make us descend at all.
    async fn dispatch(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
        now_s: u64,
    ) {
        if packet.message_type() != Some(MessageType::Fragment) {
            self.handle_typed(link, packet, claimed).await;
            return;
        }

        let reassembled = self
            .state
            .with(|state| state.fragments.accept(packet, now_s));
        let Ok(Some(bytes)) = reassembled else {
            return;
        };
        let Ok(inner) = codec::decode(&bytes) else {
            return;
        };
        if inner.message_type() == Some(MessageType::Fragment) {
            // A fragment carrying a fragment is not something the fragmenter
            // produces; it is someone trying to make us loop.
            return;
        }
        self.handle_typed(link, &inner, claimed).await;
    }

    async fn handle_typed(&mut self, link: &LinkId, packet: &BitchatPacket, claimed: &PeerId) {
        match packet.message_type() {
            Some(MessageType::Announce) => self.on_announce(link, packet, claimed).await,
            Some(MessageType::Message) => self.on_public_message(packet, claimed),
            Some(MessageType::Leave) => {
                self.state.with(|state| {
                    state.peers.remove(claimed);
                    state.sessions.forget(claimed);
                });
                self.gossip.remove_announcement_for_peer(claimed);
                self.emit_peers();
            }
            Some(MessageType::NoiseHandshake) => self.on_handshake(packet, claimed).await,
            Some(MessageType::NoiseEncrypted) => self.on_encrypted(link, packet, claimed).await,
            Some(MessageType::CourierEnvelope) => self.on_courier_envelope(packet, claimed),
            Some(MessageType::RequestSync) => self.on_request_sync(packet, claimed).await,
            Some(MessageType::FileTransfer) => self.on_file_transfer(packet, claimed),
            Some(MessageType::FileManifest) => self.on_file_manifest(packet, claimed),
            Some(MessageType::FileChunk) => self.on_file_chunk(packet, claimed),
            Some(MessageType::FileChunkRequest) => self.on_file_chunk_request(packet, claimed),
            // Boards, groups, voice and prekey bundles were deferred from P0.
            // Not handling them is right; not *relaying* them would partition
            // every peer that does implement them, so the relay decision in
            // `on_packet` still runs.
            _ => {}
        }
    }

    async fn on_announce(&mut self, link: &LinkId, packet: &BitchatPacket, claimed: &PeerId) {
        // Our own announce, come back to us. The ingress guard lets a
        // self-authored TTL-0 sync response through on purpose — that is how a
        // reinstalled device recovers its own history from a peer's archive — so
        // this is the one place that has to say no to it. Without the check we
        // register *ourselves* as a peer, which inflates the "N nearby" count the
        // whole mesh UI is built around and binds a link to our own identity.
        // bitchat refuses it in its announce preflight, as `.reject(.selfAnnounce)`.
        if claimed == &self.identity.peer_id() {
            return;
        }

        let Ok(announcement) = AnnouncementPacket::decode(&packet.payload) else {
            return;
        };
        let Ok(signing_key) = <[u8; 32]>::try_from(announcement.signing_public_key.as_slice())
        else {
            return;
        };
        let verified = verify_packet(packet, &signing_key);
        if !verified {
            // bitchat drops an unverified announce outright rather than
            // recording the peer unverified (`BLEAnnounceHandler.swift:167`).
            // Recording it would be worse than useless: an unverified announce
            // carries an attacker-chosen noise key and nickname, so accepting it
            // is exactly the identity-poisoning their handler exists to prevent.
            tracing::debug!(target: "mesh::ble", peer = %claimed, "unverified announce ignored");
            return;
        }

        let now_ms = now_millis();
        let now_s = now_secs();
        let direct = VerifiedDirectAnnounce::from_packet(
            claimed.clone(),
            packet,
            verified,
            DIRECT_ANNOUNCE_TTL,
        );

        let (is_new, link_authenticated, first_sighting, reconnected, bound_peer) =
            self.state.with(|state| {
            // Whether this announce means the peer is *reachable on a link of
            // its own*, which is not the same as "we heard from it".
            //
            // Passing `true` unconditionally, as this did, marks a peer that only
            // reached us by relay as connected — and `reconcile_connectivity` then
            // demotes it after PEER_INACTIVITY_TIMEOUT_MS because no live link is
            // bound to it, so every subsequent announce arrives as a reconnect.
            // With three devices in the room that is visible immediately: the
            // second phone's announces come in over the first phone's link and
            // flap `reconnected=true` forever.
            //
            // bitchat computes it as `hasPeripheralConnection ||
            // hasCentralSubscription || (isDirectAnnounce && !linkBoundToOtherPeer)`
            // (`BLEAnnounceHandler.swift:178`), and notes why the last clause is
            // guarded: the TTL is unsigned, so a replayed announce can look direct
            // on the replayer's link, and that must not shortcut an absent peer
            // into "connected".
            let has_own_link = !state.links.links_to(claimed).is_empty();
            let link_bound_elsewhere = state
                .links
                .peer_for(link)
                .is_some_and(|bound| bound != claimed);
            let connected =
                has_own_link || (direct.is_some() && !link_bound_elsewhere);

            let update = state
                .peers
                .upsert_verified_announce(claimed, &announcement, connected, now_ms);
            // Bind first, always. `rebind_after_verified_direct` refuses an
            // *unbound* link by design — it exists for the contested case, where a
            // link already belongs to someone else and a verified direct announce
            // is the only evidence strong enough to move it. Calling only rebind
            // for a direct announce therefore never bound a fresh inbound link at
            // all, which is the normal first-contact case: the peer registered,
            // no link was bound to it, so `reconcile_connectivity` saw no live
            // presence and demoted it 8s later — and the next announce arrived as
            // a reconnect. bitchat binds a peripheral link to the claimed sender
            // on a direct announce before any signature check
            // (`BLEService.swift:2516`), so binding is not the guarded part.
            //
            // Rebinding still runs afterwards, and is still fully guarded: on a
            // link we just bound it refuses as `SamePeer`, and on a contested one
            // `bind_if_unbound` was the no-op instead.
            state.links.bind_if_unbound(link, claimed);
            if let Some(direct) = direct.as_ref() {
                state
                    .links
                    .rebind_after_verified_direct(link, direct, now_ms);
            }
            let authenticated = state.sessions.has_session(claimed);
            if authenticated {
                state.links.mark_noise_authenticated(link, claimed);
            }
            (
                update.is_new_peer || update.was_disconnected,
                authenticated,
                update.is_new_peer,
                update.was_disconnected,
                state.links.peer_for(link).cloned(),
            )
        });

        self.emit_peers();
        // The two halves are reported separately on purpose. `is_new` is
        // `first_sighting || reconnected`, and conflating them hides the
        // interesting case: a peer that keeps *reconnecting* was demoted for
        // silence between announces, which only happens when no live link is
        // bound to it. That is a binding problem wearing a discovery problem's
        // clothes, and one field cannot tell you which you have.
        tracing::debug!(
            target: "mesh::ble",
            peer = %claimed,
            nickname = %announcement.nickname,
            first_sighting,
            reconnected,
            link = %link,
            bound_to = ?bound_peer,
            "announce accepted"
        );

        // Hand over any mail we are carrying for them.
        //
        // The ingress that releases it is deliberately strict: a *signature
        // verified* direct announce arriving on a link that is Noise
        // authenticated for this peer right now. A session outlives its link, so
        // "we have a session with X" is not enough — a replayed announce could
        // otherwise rebind X's ID to the replayer's link and drain their mail.
        let ingress = AnnounceIngress::new(direct.is_some(), link_authenticated);
        if ingress.releases_mail()
            && let Ok(key) = <[u8; 32]>::try_from(announcement.noise_public_key.as_slice())
        {
            self.hand_over_courier_mail(claimed, &key, now_s, ingress).await;
        }

        if is_new {
            // Push anything retained for them, and offer them mail for others.
            let _ = self.router.send(RouterCommand::PeerAvailable {
                peer: claimed.clone(),
            });
            // A peer we have not synced with yet gets our public history.
            if let Ok(send) = self.gossip.initial_sync_to(claimed, now_s) {
                self.dispatch_sync(send).await;
            }
        }
    }

    fn on_public_message(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        // A `Message` addressed to one recipient is a legacy private message. We
        // do not carry those — private traffic is Noise-encrypted — and showing
        // it as public chat would put a message meant for one person in the room.
        if !packet.is_broadcast() {
            return;
        }
        let Ok(content) = core::str::from_utf8(&packet.payload) else {
            return;
        };
        // An empty payload decodes cleanly and is not a message. `send_room` and
        // `send_dm` both refuse to compose one (`require_content`), and bitchat
        // refuses on its send side too (`MeshCore.kt:415`, `content.isEmpty()`) —
        // but neither of us checks on receive, so an empty broadcast from anywhere
        // became a stored row holding `Text("")`. There is nothing to render in
        // it: the room showed a bubble with a sender and a timestamp and no body,
        // which reads as a message that failed to load rather than one that was
        // never said.
        //
        // Whitespace counts as empty for the same reason. This is the one place
        // the check belongs, because a row, once stored, is what every surface
        // reads from and no later layer can tell it apart from a message whose
        // text genuinely went missing.
        if content.trim().is_empty() {
            tracing::debug!(
                target: "mesh::ble",
                peer = %claimed,
                bytes = packet.payload.len(),
                "an empty public message was dropped"
            );
            return;
        }

        // The sender is whoever the packet header says, authenticated against
        // the signing key their announce established — there is no self-declared
        // sender field to prefer, and preferring one would let anybody publish
        // under anybody's name. An unsigned or unverifiable public message is
        // dropped, matching `BLEPublicMessageHandler.swift:86-95`.
        let sender = claimed.clone();
        // A message we wrote ourselves, recovered from a peer's gossip archive.
        // The ingress guard admits a self-authored TTL-0 sync response precisely
        // so this can happen — it is how a device that lost its database gets its
        // own history back — so it has to be handled rather than dropped.
        //
        // It is authenticated against our own signing key, because we are not in
        // our own peer registry (and must not be: see `on_announce`). It is
        // attributed to us, because it *is* ours; treating it as inbound would put
        // our own words on screen as though a stranger had said them.
        let is_self = sender == self.identity.peer_id();
        // One of our own recent broadcasts, flooded or synced back to us. We
        // displayed it the moment it was composed, so showing it again is a
        // duplicate — and a confusing one, because the two rows disagree: the
        // local copy is labelled `broadcast` while this one has no delivery state
        // and falls through to "sent".
        //
        // `SelfBroadcastTracker` is exactly the "we have already shown this"
        // record. The ingress guard deliberately skips it for a TTL-0 sync
        // response — that is how a device with no database recovers its own
        // history — so this is the layer that has to consult it. A self-authored
        // packet the tracker does *not* know is genuine recovery and still lands.
        if is_self && self.state.with(|state| state.self_broadcasts.contains(packet)) {
            tracing::debug!(
                target: "mesh::ble",
                "our own broadcast came back; already shown"
            );
            return;
        }
        let (signing_key, nickname, verified) = if is_self {
            (
                self.identity.signing_public(),
                self.config.nickname.clone(),
                true,
            )
        } else {
            let known = self.state.with(|state| {
                state.peers.info(&sender).map(|peer| {
                    (
                        peer.signing_public_key.clone(),
                        peer.nickname.clone(),
                        peer.verified_nickname,
                    )
                })
            });
            let Some((Some(key), nickname, verified)) = known else {
                return;
            };
            let Ok(key) = <[u8; 32]>::try_from(key.as_slice()) else {
                return;
            };
            (key, nickname, verified)
        };
        if !verify_packet(packet, &signing_key) {
            tracing::debug!(
                target: "mesh::ble",
                peer = %sender,
                "public message with missing or invalid signature dropped",
            );
            return;
        }

        tracing::info!(
            target: "mesh::ble",
            peer = %sender,
            nickname = %nickname,
            bytes = content.len(),
            is_self,
            "public message received"
        );
        let inbound = InboundMessage {
            // No message ID on the wire, so one is derived from the packet. The
            // dedup key is a function of sender, timestamp, type and payload
            // hash, so every copy that reaches us over a different path — and
            // the same message seen twice — resolves to one ID.
            wire_id: MessageDeduplicator::dedup_key(packet),
            conversation: MeshConversation::Room,
            sender: sender.clone(),
            nickname: nickname.clone(),
            content: content.to_string(),
            timestamp_ms: packet.timestamp,
        };
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: inbound.to_domain(),
            conversation: inbound.conversation,
            sender,
            nickname,
            // `is_local` decides which side of the timeline it lands on and
            // whether it counts as unread. A recovered self-authored message is
            // ours on both counts.
            is_local: is_self,
            sender_verified: verified,
        })));
    }

    /// A file, image or voice note broadcast into the mesh room.
    ///
    /// Every gate below is [`on_public_message`](Self::on_public_message)'s, and
    /// that is deliberate rather than copied by accident: the two carry the same
    /// authentication problem. A broadcast's `sender_id` is a header field the
    /// sender chose, so registry membership is not proof of identity — only a
    /// signature that verifies against the key an announce established is.
    /// bitchat reaches the same conclusion in the same words and for a concrete
    /// reason: without it "a peer that observed a public voice burst could spoof
    /// a broadcast `voice_<burstID>.m4a` note under the talker's ID and overwrite
    /// the signature-verified live bubble with attacker audio"
    /// (`BLEFileTransferHandler.swift`, `resolveSenderNickname`).
    ///
    /// What is *not* ported is bitchat's MIME allowlist and magic-byte check
    /// (`BLEIncomingFileValidator`). Those exist to protect a decoder, and
    /// nothing here decodes: the bytes are encrypted and stored, never parsed.
    /// Whatever eventually renders them will need that check, and this is the
    /// layer to put it in.
    fn on_file_transfer(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        // Directed file transfers are refused, where bitchat delivers one as a
        // private message (`BLEFileTransferPolicy.deliveryPlan`). A directed
        // `FileTransfer` is a file in the clear addressed to one peer, and this
        // client's private traffic is Noise-encrypted without exception —
        // accepting it would put an unencrypted file in a DM and teach the UI
        // that a DM can arrive unencrypted. `on_public_message` refuses the
        // legacy directed `Message` for exactly this reason. Relaying is
        // untouched: a peer that does implement directed files must not be
        // partitioned by our refusal.
        if !packet.is_broadcast() {
            return;
        }
        let Ok(file) = BitchatFilePacket::decode(&packet.payload) else {
            return;
        };

        let sender = claimed.clone();
        // A file we sent ourselves, recovered from a peer's gossip archive. The
        // ingress guard admits a self-authored TTL-0 sync response on purpose, so
        // this reaches us and has to be handled rather than dropped.
        let is_self = sender == self.identity.peer_id();
        // One of our own broadcasts flooded or synced straight back. We stored
        // and showed it when it was composed, so this copy is a duplicate — and
        // a worse one than a duplicate text message, because storing it again
        // would write a *second encrypted copy of the bytes* to disk under a new
        // attachment id. bitchat suppresses the same case as
        // `BLEFileTransferPolicy.isSelfEcho`.
        if is_self && self.state.with(|state| state.self_broadcasts.contains(packet)) {
            tracing::debug!(
                target: "mesh::ble",
                "our own file came back; already stored"
            );
            return;
        }
        let (signing_key, nickname, verified) = if is_self {
            // Verified against our own key, because we are deliberately not in
            // our own peer registry — see `on_announce`.
            (
                self.identity.signing_public(),
                self.config.nickname.clone(),
                true,
            )
        } else {
            let known = self.state.with(|state| {
                state.peers.info(&sender).map(|peer| {
                    (
                        peer.signing_public_key.clone(),
                        peer.nickname.clone(),
                        peer.verified_nickname,
                    )
                })
            });
            let Some((Some(key), nickname, verified)) = known else {
                // No announce from this peer, or one that carried no signing
                // key: there is nothing to check the signature against, so the
                // file is unattributable. bitchat drops it here too, as
                // "file transfer from unverified or unknown peer".
                tracing::debug!(
                    target: "mesh::ble",
                    peer = %sender,
                    "file transfer from an unknown or unverified peer dropped"
                );
                return;
            };
            let Ok(key) = <[u8; 32]>::try_from(key.as_slice()) else {
                return;
            };
            (key, nickname, verified)
        };
        if !verify_packet(packet, &signing_key) {
            tracing::debug!(
                target: "mesh::ble",
                peer = %sender,
                "file transfer with missing or invalid signature dropped",
            );
            return;
        }

        // No message ID on the wire, so it is derived from the packet — the same
        // derivation a public message uses, which is what collapses the copies
        // arriving by different paths onto one row.
        let wire_id = MessageDeduplicator::dedup_key(packet);
        // Already have it. The dedup set in `on_packet` catches the flood, but the
        // gossip archive exists to hand public packets back *after* that window
        // has closed — and a file cannot be stored idempotently the way a text
        // message can, because its bytes go to disk before the row that would
        // have been ignored. See `MeshTimeline::already_stored`.
        match self.events.already_stored(&wire_id) {
            Ok(true) => {
                tracing::debug!(
                    target: "mesh::ble",
                    peer = %sender,
                    "a file already on disk arrived again; not stored twice"
                );
                return;
            }
            Ok(false) => {}
            Err(error) => {
                // Store anyway rather than dropping the file over a read failure:
                // a duplicate copy is recoverable, a discarded one is not.
                tracing::warn!(
                    target: "mesh::ble",
                    %error,
                    "could not check whether this file is already stored"
                );
            }
        }

        // Stored before the event exists, because the row the event carries names
        // the attachment by id. A failure here is loud: the packet is off the air,
        // its dedup entry is already recorded, and nothing retransmits — so this
        // is a file the user will never be offered again.
        let attachment = match self.events.store_file(
            file.file_name.as_deref(),
            file.mime_type.as_deref(),
            &file.content,
        ) {
            Ok(attachment) => attachment,
            Err(error) => {
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    peer = %sender,
                    bytes = file.content.len(),
                    "an inbound mesh file could not be stored; it is lost"
                );
                self.degraded(format!("a file from {nickname} could not be saved: {error}"));
                return;
            }
        };

        tracing::info!(
            target: "mesh::ble",
            peer = %sender,
            nickname = %nickname,
            bytes = file.content.len(),
            name = ?file.file_name,
            mime = ?file.mime_type,
            is_self,
            "file transfer received"
        );
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: mapping::inbound_file_message(
                &MeshConversation::Room,
                &wire_id,
                &sender,
                attachment,
                packet.timestamp,
            ),
            conversation: MeshConversation::Room,
            sender,
            nickname,
            is_local: is_self,
            sender_verified: verified,
        })));
    }

    // ------------------------------------------------- chunked file transfer
    //
    // Three handlers, one authentication gate, and one rule that shapes all of
    // them: **a manifest signed once does not authenticate the chunks that
    // follow.** Skipping per-chunk verification would turn a transfer into an
    // unauthenticated write channel into a file the receiver is going to store
    // and show under the manifest sender's name. So every packet below goes
    // through [`authenticate_stream_packet`](Self::authenticate_stream_packet),
    // every time, and the spool separately checks that the verified sender is the
    // *same* peer whose manifest opened the file.

    /// Every gate P8 established, applied to one chunked-transfer packet.
    ///
    /// Deliberately a separate helper rather than a refactor of
    /// [`on_file_transfer`](Self::on_file_transfer): that path works, is tested
    /// against real bitchat, and has one gate this one does not — see the
    /// `is_self` case below.
    fn authenticate_stream_packet(
        &self,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) -> Option<(PeerId, String, bool)> {
        // Broadcast only, exactly as `on_file_transfer`: a directed cleartext
        // file has no business in a client whose every DM is Noise-sealed.
        if !packet.is_broadcast() {
            return None;
        }

        // Our own packet, coming back. Refused outright, where the single-packet
        // path has to *handle* a self-authored copy because the gossip archive
        // replays public `FileTransfer` packets and that is how a reinstalled
        // device recovers its own history.
        //
        // These three types have no sync bit at all (`SyncTypeFlags::bit_index`
        // returns `None` for each), so they are never replayed out of an archive
        // and a self-authored copy can only be our own flood echoing back. There
        // is nothing to recover and nothing to attribute: the local copy was
        // stored when the transfer started.
        if claimed == &self.identity.peer_id() {
            return None;
        }

        let (key, nickname, verified) = self.state.with(|state| {
            state.peers.info(claimed).map(|peer| {
                (
                    peer.signing_public_key.clone(),
                    peer.nickname.clone(),
                    peer.verified_nickname,
                )
            })
        })?;
        // No announce from this peer, or one with no signing key: there is
        // nothing to check a signature against, so the packet is unattributable.
        let key = <[u8; 32]>::try_from(key?.as_slice()).ok()?;
        if !verify_packet(packet, &key) {
            tracing::debug!(
                target: "mesh::ble",
                peer = %claimed,
                msg_type = packet.msg_type,
                "chunked transfer packet with missing or invalid signature dropped",
            );
            return None;
        }
        Some((claimed.clone(), nickname, verified))
    }

    /// A manifest: the start of a chunked transfer, or a repeat of one.
    fn on_file_manifest(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(manifest) = FileManifest::decode(&packet.payload) else {
            return;
        };
        let Some((sender, nickname, _verified)) =
            self.authenticate_stream_packet(packet, claimed)
        else {
            return;
        };

        // The row identity, fixed now and persisted with the bitmap. Derived from
        // the manifest packet the same way a text message's is, so the copies of
        // the *manifest* arriving by different paths collapse onto one row — and
        // so a transfer resumed after a restart lands on that same row rather
        // than a second one.
        let wire_id = MessageDeduplicator::dedup_key(packet);
        let transfer = hex_encode(&manifest.transfer_id);

        match self.files_in.begin(
            manifest,
            sender.clone(),
            wire_id,
            packet.timestamp,
            now_secs(),
        ) {
            Ok(ManifestOutcome::Started) => {
                tracing::info!(
                    target: "mesh::ble",
                    peer = %sender,
                    nickname = %nickname,
                    %transfer,
                    "a chunked file transfer started"
                );
            }
            Ok(ManifestOutcome::Resumed { present, total }) => {
                tracing::info!(
                    target: "mesh::ble",
                    peer = %sender,
                    %transfer,
                    present,
                    total,
                    "resuming a chunked file transfer"
                );
            }
            // The common case: the manifest repeats on a cadence so a peer that
            // joins mid-transfer can pick it up.
            Ok(ManifestOutcome::Unchanged { .. }) => {}
            Err(error) => {
                tracing::warn!(
                    target: "mesh::ble",
                    %error,
                    peer = %sender,
                    %transfer,
                    "a chunked file transfer could not be started"
                );
            }
        }
    }

    /// One chunk, verified in its own right and written straight to disk.
    fn on_file_chunk(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(chunk) = FileChunk::decode(&packet.payload) else {
            return;
        };
        let Some((sender, nickname, _verified)) =
            self.authenticate_stream_packet(packet, claimed)
        else {
            return;
        };

        let outcome = self.files_in.accept_chunk(
            &chunk.transfer_id,
            chunk.index,
            &chunk.bytes,
            &sender,
            now_secs(),
        );
        let transfer = hex_encode(&chunk.transfer_id);
        match outcome {
            Ok(ChunkOutcome::Stored { present, total }) => {
                tracing::debug!(
                    target: "mesh::ble",
                    peer = %sender,
                    %transfer,
                    index = chunk.index,
                    present,
                    total,
                    "chunk stored"
                );
            }
            Ok(ChunkOutcome::Duplicate) => {}
            Ok(ChunkOutcome::Complete) => self.complete_transfer(&chunk.transfer_id, &nickname),
            // A chunk with no manifest is normal, not hostile: chunks and
            // manifests are separate broadcasts and the manifest repeats.
            Ok(ChunkOutcome::Unknown) => {
                tracing::debug!(
                    target: "mesh::ble",
                    peer = %sender,
                    %transfer,
                    "a chunk arrived before its manifest; waiting for one"
                );
            }
            Ok(ChunkOutcome::Refused(reason)) => {
                tracing::warn!(
                    target: "mesh::ble",
                    peer = %sender,
                    %transfer,
                    index = chunk.index,
                    reason,
                    "chunk refused"
                );
            }
            Err(error) => {
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    peer = %sender,
                    %transfer,
                    "a chunk could not be written; the transfer will stall"
                );
            }
        }
    }

    /// Verify, encrypt into place, and surface the row.
    ///
    /// Ordering here is the whole security property of the receive path:
    ///
    /// 1. **Digest first.** Per-chunk signatures say who sent each chunk; only
    ///    the whole-file digest says the assembly is the file the manifest
    ///    described. It is checked before the encryptor opens the file, because
    ///    after that the bytes are inside the attachments directory.
    /// 2. **`already_stored` next**, for the same reason the single-packet path
    ///    checks it: a file's bytes reach disk *before* the row that names them,
    ///    so storing first and letting an idempotent row write be ignored would
    ///    leave an encrypted copy nothing references.
    /// 3. **Then encrypt into place** with the streaming call the CDN upload path
    ///    uses, and only then the event, because the event names the attachment.
    fn complete_transfer(&mut self, transfer_id: &TransferId, nickname: &str) {
        let transfer_hex = hex_encode(transfer_id);
        if let Err(error) = self.files_in.verify(transfer_id) {
            // Terminal. `verify` has already discarded the partial file and its
            // bitmap: a mismatch means some chunk that passed its packet
            // signature still produced the wrong file, so there is no chunk to
            // re-request and asking again would loop forever on the same bytes.
            tracing::error!(
                target: "mesh::ble",
                %error,
                transfer = %transfer_hex,
                "a completed chunked transfer did not match its digest; discarded, not retried"
            );
            self.degraded(format!(
                "a file from {nickname} arrived corrupted and was discarded"
            ));
            return;
        }

        let part = self.files_in.part_path(transfer_id);
        let Some(transfer) = self.files_in.take(transfer_id) else {
            return;
        };

        match self.events.already_stored(&transfer.wire_id) {
            Ok(true) => {
                tracing::debug!(
                    target: "mesh::ble",
                    transfer = %transfer_hex,
                    "a chunked file already on disk completed again; not stored twice"
                );
                self.files_in.discard(transfer_id);
                return;
            }
            Ok(false) => {}
            Err(error) => {
                // Store anyway: a duplicate copy is recoverable, and this file
                // cost minutes of radio that nothing will spend again.
                tracing::warn!(
                    target: "mesh::ble",
                    %error,
                    "could not check whether this chunked file is already stored"
                );
            }
        }

        let attachment = match self.events.store_file_streamed(
            transfer.manifest.file_name.as_deref(),
            transfer.manifest.mime_type.as_deref(),
            &part,
        ) {
            Ok(attachment) => attachment,
            Err(error) => {
                // The partial file is deliberately left on disk. Everything else
                // about this transfer is gone from the air, and re-requesting is
                // not possible — the bytes are all here, only the encryption or
                // the row write failed, and a retry after the user frees disk
                // space is the one recovery left.
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    transfer = %transfer_hex,
                    bytes = transfer.manifest.total_size,
                    "a completed chunked transfer could not be stored"
                );
                self.degraded(format!(
                    "a file from {nickname} could not be saved: {error}"
                ));
                return;
            }
        };
        self.files_in.discard(transfer_id);

        tracing::info!(
            target: "mesh::ble",
            peer = %transfer.sender,
            nickname = %nickname,
            transfer = %transfer_hex,
            bytes = transfer.manifest.total_size,
            chunks = transfer.manifest.chunk_count,
            name = ?transfer.manifest.file_name,
            "a chunked file transfer completed"
        );

        let verified = self
            .state
            .with(|state| state.peers.info(&transfer.sender).is_some_and(|p| p.verified_nickname));
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: mapping::inbound_file_message(
                &MeshConversation::Room,
                &transfer.wire_id,
                &transfer.sender,
                attachment,
                // The manifest's timestamp, not now: the row belongs where the
                // sender put it, and a transfer that took ten minutes would
                // otherwise sort ten minutes after everything said during it.
                transfer.timestamp_ms,
            ),
            conversation: MeshConversation::Room,
            sender: transfer.sender,
            nickname: nickname.to_string(),
            is_local: false,
            sender_verified: verified,
        })));
    }

    /// A receiver asking us to send chunks again.
    ///
    /// Authenticated like everything else here, and for a reason beyond
    /// attribution: serving a request costs *our* airtime, so an unauthenticated
    /// request would be an amplification lever for anyone in range.
    fn on_file_chunk_request(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(request) = FileChunkRequest::decode(&packet.payload) else {
            return;
        };
        let Some((sender, _nickname, _verified)) =
            self.authenticate_stream_packet(packet, claimed)
        else {
            return;
        };
        // Only a transfer we are actually sending. A request for anything else is
        // a peer asking us to serve a file we do not have — which, once chunked
        // transfers ride the gossip archive, becomes a real feature; today it is
        // simply not ours to answer.
        let Some(transfer) = self.files_out.get_mut(&request.transfer_id) else {
            return;
        };
        transfer.note_requested(&request.indices);
        tracing::info!(
            target: "mesh::ble",
            peer = %sender,
            transfer = %hex_encode(&request.transfer_id),
            chunks = request.indices.len(),
            "retransmitting chunks a receiver asked for"
        );
    }

    async fn on_handshake(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let local = self.identity.peer_id();
        let identity = Arc::clone(&self.identity);
        let step = self.state.with(|state| {
            state
                .sessions
                .on_handshake(claimed, &local, &identity, &packet.payload)
        });

        let step = match step {
            Ok(step) => step,
            Err(error) => {
                tracing::debug!(target: "mesh::ble", peer = %claimed, %error, "handshake failed");
                return;
            }
        };

        if let Some(reply) = step.reply {
            let packet = self.build_packet(
                MessageType::NoiseHandshake,
                Some(claimed),
                reply,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            );
            self.transmit(&packet, Some(claimed), None).await;
        }

        if step.established.is_some() {
            // A session is the only signal strong enough for the router to send
            // without retaining, so this is the moment retained mail can go.
            let _ = self.router.send(RouterCommand::PeerAvailable {
                peer: claimed.clone(),
            });
            self.flush_pending_dms(claimed).await;
            self.emit_peers();
        }
    }

    async fn on_encrypted(&mut self, link: &LinkId, packet: &BitchatPacket, claimed: &PeerId) {
        // Only accept encrypted traffic addressed to us; a directed packet for
        // somebody else is relayed, not opened.
        if !self.addressed_to_us(packet) {
            return;
        }
        let opened = self
            .state
            .with(|state| state.sessions.decrypt(claimed, &packet.payload));
        let plaintext = match opened {
            Ok(plaintext) => plaintext,
            Err(NoiseError::WrongState(_)) => {
                // No session. Start one rather than dropping the peer silently:
                // this is how a first message from a peer we have never spoken
                // to gets answered.
                self.start_handshake(claimed).await;
                return;
            }
            Err(error) => {
                tracing::debug!(target: "mesh::ble", peer = %claimed, %error, "decrypt failed");
                return;
            }
        };

        // The link carried traffic only this peer's session key could produce.
        self.state
            .with(|state| state.links.mark_noise_authenticated(link, claimed));

        let Some((payload_type, body)) = decode_noise_payload(&plaintext) else {
            return;
        };
        match payload_type {
            NoisePayloadType::PrivateMessage => {
                let Ok(message) = PrivateMessagePacket::decode(body) else {
                    return;
                };
                self.emit_dm(claimed, &message.message_id, &message.content, packet.timestamp);
                self.send_ack(claimed, &message.message_id, NoisePayloadType::Delivered)
                    .await;
            }
            NoisePayloadType::Delivered => self.forward_ack(claimed, body, AckKind::Delivered),
            NoisePayloadType::ReadReceipt => self.forward_ack(claimed, body, AckKind::Read),
            // Groups, voice, and verification are deferred; dropping the payload
            // is the correct handling for a type this build does not implement.
            _ => {}
        }
    }

    fn forward_ack(&self, peer: &PeerId, body: &[u8], kind: AckKind) {
        // The ack body is the message ID as raw UTF-8, not a TLV.
        let Ok(message_id) = String::from_utf8(body.to_vec()) else {
            return;
        };
        let _ = self.router.send(RouterCommand::Acked {
            message_id,
            peer: peer.clone(),
            kind,
        });
    }

    fn on_courier_envelope(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(envelope) = CourierEnvelope::decode(&packet.payload) else {
            return;
        };

        // Is it ours? Only the seal can answer that — the recipient tag rotates
        // and is a hint, not proof.
        if let Ok((plaintext, sender_key)) = open_courier(&envelope.ciphertext, &self.identity) {
            if let Some((NoisePayloadType::PrivateMessage, body)) = decode_noise_payload(&plaintext)
                && let Ok(message) = PrivateMessagePacket::decode(body)
            {
                let sender = PeerId::from_noise_key(&sender_key);
                self.emit_dm(&sender, &message.message_id, &message.content, packet.timestamp);
            }
            return;
        }

        // Not ours: carry it. Verified strangers and mutual favourites draw from
        // separate quotas, which is what stops strangers crowding out favourites.
        let snapshot = self.state.with(|state| {
            state
                .peers
                .info(claimed)
                .map(|record| (record.noise_public_key.clone(), record.verified_nickname))
        });
        let Some((Some(depositor_key), verified)) = snapshot else {
            return;
        };
        if !verified {
            // An unverified peer may not deposit mail: without a checked
            // signature the pool is a free amplifier.
            return;
        }
        if let Err(error) = self.couriers.deposit(
            &envelope,
            &depositor_key,
            CourierTier::Verified,
            now_secs(),
        ) {
            tracing::debug!(target: "mesh::ble", %error, "courier deposit refused");
        }
    }

    async fn on_request_sync(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(request) = RequestSyncPacket::decode(&packet.payload) else {
            return;
        };
        let sends = self
            .gossip
            .handle_request_sync(claimed, &request, now_secs());
        for send in sends {
            self.dispatch_sync(send).await;
        }
    }

    // ---------------------------------------------------------------- outbound

    async fn on_command(&mut self, command: BleCommand) {
        match command {
            BleCommand::Stop => {}
            BleCommand::Announce => self.announce().await,
            BleCommand::Broadcast { packet, sign } => {
                let packet = self.maybe_sign(packet, sign);
                self.transmit(&packet, None, None).await;
            }
            BleCommand::Directed { peer, packet, sign } => {
                let packet = self.maybe_sign(packet, sign);
                self.transmit(&packet, Some(&peer), None).await;
            }
            BleCommand::PrivateMessage {
                peer,
                message_id,
                content,
            } => self.send_private(&peer, &message_id, &content).await,
            BleCommand::Courier { peer, envelope } => {
                let Ok(payload) = envelope.encode() else {
                    return;
                };
                let packet = self.build_packet(
                    MessageType::CourierEnvelope,
                    Some(&peer),
                    payload,
                    chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
                );
                self.transmit(&packet, Some(&peer), None).await;
            }
            BleCommand::Ack {
                peer,
                message_id,
                payload_type,
            } => self.send_ack(&peer, &message_id, payload_type).await,
            BleCommand::PublicMessage {
                message_id,
                content,
            } => self.broadcast_public(&message_id, &content).await,
            BleCommand::FileTransfer { message_id, file } => {
                self.broadcast_file(&message_id, &file).await
            }
            BleCommand::StreamedFile {
                message_id,
                manifest,
                path,
            } => self.start_streamed_file(&message_id, manifest, path).await,
        }
    }

    async fn send_private(&mut self, peer: &PeerId, message_id: &str, content: &str) {
        let Ok(tlv) = PrivateMessagePacket::new(message_id.to_string(), content.to_string()).encode()
        else {
            return;
        };
        let payload = encode_noise_payload(NoisePayloadType::PrivateMessage, &tlv);

        let sealed = self
            .state
            .with(|state| state.sessions.encrypt(peer, &payload));
        match sealed {
            Ok(ciphertext) => {
                let packet = self.build_packet(
                    MessageType::NoiseEncrypted,
                    Some(peer),
                    ciphertext,
                    chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
                );
                self.transmit(&packet, Some(peer), None).await;
            }
            Err(_) => {
                // No session yet. Hold the message and start a handshake: the
                // router has already retained a copy, so nothing is lost if the
                // handshake never completes.
                self.state.with(|state| {
                    state
                        .pending_dms
                        .entry(peer.clone())
                        .or_default()
                        .push(super::state::PendingDm {
                            message_id: message_id.to_string(),
                            content: content.to_string(),
                            queued_at_ms: now_millis(),
                        });
                });
                self.start_handshake(peer).await;
            }
        }
    }

    async fn start_handshake(&mut self, peer: &PeerId) {
        let identity = Arc::clone(&self.identity);
        let opening = self
            .state
            .with(|state| state.sessions.initiate(peer, &identity));
        let Ok(opening) = opening else {
            return;
        };
        let packet = self.build_packet(
            MessageType::NoiseHandshake,
            Some(peer),
            opening,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        self.transmit(&packet, Some(peer), None).await;
    }

    async fn flush_pending_dms(&mut self, peer: &PeerId) {
        let pending = self
            .state
            .with(|state| state.pending_dms.remove(peer).unwrap_or_default());
        for message in pending {
            self.send_private(peer, &message.message_id, &message.content)
                .await;
        }
    }

    async fn send_ack(
        &mut self,
        peer: &PeerId,
        message_id: &str,
        payload_type: NoisePayloadType,
    ) {
        let payload = encode_noise_payload(payload_type, message_id.as_bytes());
        let sealed = self
            .state
            .with(|state| state.sessions.encrypt(peer, &payload));
        let Ok(ciphertext) = sealed else {
            return;
        };
        let packet = self.build_packet(
            MessageType::NoiseEncrypted,
            Some(peer),
            ciphertext,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        self.transmit(&packet, Some(peer), None).await;
    }

    /// Broadcast a public message into the mesh room.
    pub async fn broadcast_public(&mut self, message_id: &str, content: &str) {
        let now_ms = now_millis();
        let nickname = self.config.nickname.clone();
        // The payload of a public message is the message text and nothing else
        // (`BLEService.swift:504`: `payload: Data(content.utf8)`). There is no
        // room on the wire for a message ID, a timestamp or a sender name:
        // receivers take the sender from the packet header, the time from
        // `packet.timestamp`, and the name from the announce registry. bitchat's
        // `BitchatMessage.toBinaryPayload()` encodes all of those, and is dead
        // code outside its own tests.
        let packet = self.build_packet(
            MessageType::Message,
            None,
            content.as_bytes().to_vec(),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        // Signed, because the receiver requires it: an unsigned public message
        // is dropped before its payload is even decoded
        // (`BLEPublicMessageHandler.swift:91`).
        let packet = self.maybe_sign(packet, true);
        // Remember it so the copy that floods back to us is recognised as our
        // own rather than displayed twice.
        self.state
            .with(|state| state.self_broadcasts.record(message_id, &packet, now_ms));
        self.transmit(&packet, None, None).await;

        let local = self.identity.peer_id();
        let message = outbound_message(
            &MeshConversation::Room,
            message_id,
            &local,
            content,
            now_ms,
        );
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message,
            conversation: MeshConversation::Room,
            sender: local,
            nickname,
            is_local: true,
            sender_verified: true,
        })));
    }

    /// Put a file on the air for the mesh room.
    ///
    /// The counterpart of [`broadcast_public`](Self::broadcast_public), and
    /// deliberately shaped the same way: build, sign, remember as our own,
    /// transmit, then mirror locally — because the mesh has no server echo, so
    /// the local copy *is* the message.
    pub async fn broadcast_file(&mut self, message_id: &str, file: &BitchatFilePacket) {
        let now_ms = now_millis();
        let nickname = self.config.nickname.clone();
        let payload = match file.encode() {
            Ok(payload) => payload,
            Err(error) => {
                // Only reachable above `MAX_PAYLOAD_BYTES`, which the service
                // already refused, so this is a bug rather than a user error —
                // and returning quietly would leave the user's row holding a
                // delivery state for a packet that never existed.
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    bytes = file.content.len(),
                    "a file transfer could not be encoded; nothing was sent"
                );
                return;
            }
        };
        let packet = self.build_packet(
            MessageType::FileTransfer,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        // Signed, because the receiver requires it: `on_file_transfer` drops an
        // unsigned broadcast before it looks at the content, exactly as the
        // public-message path does.
        let packet = self.maybe_sign(packet, true);
        // Remember it, so the copy that floods back is recognised as ours rather
        // than stored a second time under a new attachment id.
        self.state
            .with(|state| state.self_broadcasts.record(message_id, &packet, now_ms));
        self.transmit(&packet, None, None).await;

        // Our own copy, stored the same way an inbound one is. Without it the row
        // would name an attachment that does not exist, and the sender would be
        // the only person on the mesh who cannot open what they just sent.
        let attachment = match self.events.store_file(
            file.file_name.as_deref(),
            file.mime_type.as_deref(),
            &file.content,
        ) {
            Ok(attachment) => attachment,
            Err(error) => {
                // The packet is already away, so the file did reach the room.
                // Only our own record of it is missing, and saying so is better
                // than a row pointing at nothing.
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    "a sent mesh file could not be stored locally"
                );
                self.degraded(format!(
                    "the file was sent, but this device could not keep a copy: {error}"
                ));
                return;
            }
        };

        let local = self.identity.peer_id();
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: mapping::outbound_file_message(
                &MeshConversation::Room,
                message_id,
                &local,
                attachment,
                now_ms,
            ),
            conversation: MeshConversation::Room,
            sender: local,
            nickname,
            is_local: true,
            sender_verified: true,
        })));
    }

    /// Begin a chunked transfer: manifest on the air, local copy stored, chunks
    /// paced out by the tick.
    ///
    /// Shaped like [`broadcast_file`](Self::broadcast_file) — build, sign,
    /// remember as our own, transmit, mirror locally — with one difference that
    /// is the whole of P9 on the send side: **the file is never read whole.** The
    /// local mirror goes through the streaming encryptor straight from the
    /// source path, and the chunks are read one window at a time from
    /// [`on_tick`](Self::on_tick).
    pub async fn start_streamed_file(
        &mut self,
        message_id: &str,
        manifest: FileManifest,
        path: std::path::PathBuf,
    ) {
        let now_ms = now_millis();
        let now_s = now_secs();
        let nickname = self.config.nickname.clone();
        let transfer_id = manifest.transfer_id;

        // Our own copy first, and a failure here aborts the send.
        //
        // The opposite of `broadcast_file`, which puts the packet on the air and
        // then stores — because there it is one packet and by the time storing
        // fails the file has already reached the room. Here the transfer takes
        // minutes, so failing before the first byte costs nothing and avoids a
        // sender streaming out a file it cannot itself open.
        let attachment = match self.events.store_file_streamed(
            manifest.file_name.as_deref(),
            manifest.mime_type.as_deref(),
            &path,
        ) {
            Ok(attachment) => attachment,
            Err(error) => {
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    path = %path.display(),
                    "a file could not be stored locally; nothing was streamed"
                );
                self.degraded(format!("that file could not be read or saved: {error}"));
                return;
            }
        };

        let transfer = OutboundTransfer::new(manifest, path, message_id.to_string(), now_s);
        let manifest_packet = self.build_manifest_packet(&transfer.manifest);
        // Remembered so the echo is recognised as ours. The chunks are recorded
        // the same way as they go out — see `pump_streamed_files`.
        self.state
            .with(|state| state.self_broadcasts.record(message_id, &manifest_packet, now_ms));
        self.transmit(&manifest_packet, None, None).await;

        tracing::info!(
            target: "mesh::ble",
            transfer = %hex_encode(&transfer_id),
            bytes = transfer.manifest.total_size,
            chunks = transfer.manifest.chunk_count,
            "streaming a file into the mesh room"
        );
        self.files_out.insert(transfer_id, transfer);

        let local = self.identity.peer_id();
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: mapping::outbound_file_message(
                &MeshConversation::Room,
                message_id,
                &local,
                attachment,
                now_ms,
            ),
            conversation: MeshConversation::Room,
            sender: local,
            nickname,
            is_local: true,
            sender_verified: true,
        })));
    }

    fn build_manifest_packet(&self, manifest: &FileManifest) -> BitchatPacket {
        let payload = manifest.encode().unwrap_or_default();
        let packet = self.build_packet(
            MessageType::FileManifest,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        // Signed, because the receiver requires it: an unsigned manifest is
        // dropped before its payload is looked at, exactly as an unsigned file
        // transfer is.
        self.maybe_sign(packet, true)
    }

    /// Put the next chunk of each live transfer on the air, and repeat manifests.
    ///
    /// Called from the tick, one chunk at a time, rather than looping over the
    /// whole file: the loop is also the only thing servicing link events, and a
    /// 16 MiB transfer emitted in one pass would leave the mesh deaf for minutes.
    /// `now_ms` is a parameter rather than a call to the clock inside, because
    /// chunk pacing is measured in milliseconds and a rate that reads its own
    /// clock cannot be tested without sleeping for real.
    async fn pump_streamed_files(&mut self, now_s: u64, now_ms: u64) {
        if self.files_out.is_empty() {
            return;
        }

        // Backpressure the loop can see. A chunk is the lowest-priority thing in
        // the write buffer, so generating chunks into a full buffer mostly evicts
        // earlier chunks of the same transfer.
        let queued = self.state.with(|state| state.writes.total_queued_bytes());
        let congested = queued > CHUNK_PACING_QUEUE_BYTES;

        let ids: Vec<TransferId> = self.files_out.keys().copied().collect();
        for id in ids {
            // A manifest again, whether or not the queue is busy: it is a few
            // hundred bytes, and a peer that arrived mid-transfer cannot use a
            // single chunk without one.
            let due = self
                .files_out
                .get(&id)
                .is_some_and(|t| now_s.saturating_sub(t.last_manifest_secs) >= MANIFEST_REPEAT_SECS);
            if due {
                let manifest = {
                    let Some(transfer) = self.files_out.get_mut(&id) else {
                        continue;
                    };
                    transfer.last_manifest_secs = now_s;
                    transfer.manifest.clone()
                };
                let packet = self.build_manifest_packet(&manifest);
                self.state.with(|state| {
                    state
                        .self_broadcasts
                        .record("mesh-manifest-repeat", &packet, now_millis())
                });
                self.transmit(&packet, None, None).await;
            }

            if congested {
                continue;
            }

            // Rate pacing, on top of the queue check above. The queue check only
            // fires where the radio reports congestion; this one holds in the
            // direction where it never does.
            if self
                .files_out
                .get(&id)
                .is_some_and(|transfer| now_ms < transfer.next_chunk_ms)
            {
                continue;
            }

            for _ in 0..CHUNKS_PER_TICK {
                let Some(transfer) = self.files_out.get_mut(&id) else {
                    break;
                };
                let Some(index) = transfer.next_chunk() else {
                    break;
                };
                let bytes = match transfer.read_chunk(index) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        // The user's file moved or became unreadable mid-transfer.
                        // Nothing recovers from that, and continuing would stream
                        // a file that no longer exists.
                        tracing::error!(
                            target: "mesh::ble",
                            %error,
                            transfer = %hex_encode(&id),
                            index,
                            "a chunk could not be read; abandoning the transfer"
                        );
                        self.files_out.remove(&id);
                        self.degraded("a file being sent became unreadable".to_string());
                        break;
                    }
                };
                let Ok(payload) = FileChunk::new(id, index, bytes).and_then(|c| c.encode()) else {
                    continue;
                };
                // Hold the next chunk back for as long as this one needs on the
                // air. Stamped from the encoded length, so a short final chunk
                // waits proportionally less, and set before the write rather than
                // after so a slow write does not extend the interval twice.
                if let Some(transfer) = self.files_out.get_mut(&id) {
                    let air_ms = (payload.len() as u64)
                        .saturating_mul(1_000)
                        .saturating_div(AIR_BYTES_PER_SEC.max(1));
                    transfer.next_chunk_ms = now_ms.saturating_add(air_ms);
                }
                let packet = self.build_packet(
                    MessageType::FileChunk,
                    None,
                    payload,
                    chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
                );
                // Signed individually. This is the point the design is emphatic
                // about: the manifest's signature says nothing about the chunks,
                // and an unsigned chunk is refused by the receiver.
                let packet = self.maybe_sign(packet, true);
                self.state.with(|state| {
                    state
                        .self_broadcasts
                        .record("mesh-file-chunk", &packet, now_millis())
                });
                self.transmit(&packet, None, None).await;
            }
        }

        // A drained transfer is kept, not dropped, for as long as a receiver
        // might still ask: there is no acknowledgement, so "drained" only means
        // we have nothing left to offer unprompted.
        let expired: Vec<TransferId> = self
            .files_out
            .iter()
            .filter(|(_, transfer)| {
                transfer.is_drained()
                    && now_s.saturating_sub(transfer.started_secs) > TRANSFER_ABANDON_SECS
            })
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            tracing::debug!(
                target: "mesh::ble",
                transfer = %hex_encode(&id),
                "closing a finished outbound transfer; no more retransmit requests will be served"
            );
            self.files_out.remove(&id);
        }
    }

    /// Ask for the chunks of a stalled transfer.
    ///
    /// The receiver-driven half of retransmission, and the direct analogue of
    /// `GossipSyncManager::request_missing_fragments` one layer down — including
    /// the truncation: [`FileChunkRequest`] names at most
    /// [`MAX_REQUESTED_CHUNKS`], so a receiver missing hundreds asks for the
    /// earliest now and the rest next round rather than emitting a request that
    /// would have to be fragmented to fit.
    async fn request_missing_chunks(&mut self, now_s: u64) {
        let stalled =
            self.files_in
                .stalled(now_s, CHUNK_REQUEST_QUIET_SECS, MAX_REQUESTED_CHUNKS);
        for (transfer_id, missing) in stalled {
            let Ok(request) = FileChunkRequest::new(transfer_id, missing) else {
                continue;
            };
            let Ok(payload) = request.encode() else {
                continue;
            };
            let packet = self.build_packet(
                MessageType::FileChunkRequest,
                None,
                payload,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            );
            // Signed: the sender refuses an unauthenticated request, because
            // serving one costs airtime and would otherwise be an amplification
            // lever.
            let packet = self.maybe_sign(packet, true);
            self.state.with(|state| {
                state
                    .self_broadcasts
                    .record("mesh-chunk-request", &packet, now_millis())
            });
            tracing::info!(
                target: "mesh::ble",
                transfer = %hex_encode(&transfer_id),
                chunks = request.indices.len(),
                "asking for chunks that never arrived"
            );
            self.transmit(&packet, None, None).await;
        }
    }

    async fn announce(&mut self) {
        let now_ms = now_millis();
        let nickname = self.config.nickname.clone();
        let mut capabilities = PeerCapabilities::from_bits(0);
        if self.config.bridge_geohash.is_some() {
            capabilities.insert(PeerCapabilities::BRIDGE);
        }
        let announcement = AnnouncementPacket::new(
            nickname.clone(),
            self.identity.static_public().to_vec(),
            self.identity.signing_public().to_vec(),
            None,
            Some(capabilities),
            self.config.bridge_geohash.clone(),
        );
        let payload = match announcement.encode() {
            Ok(payload) => payload,
            Err(error) => {
                // Silent before. An announce that cannot be encoded means this
                // device is invisible to every peer forever, which is the single
                // worst failure the mesh has and deserves to be loud.
                tracing::error!(
                    target: "mesh::ble",
                    %error,
                    nickname = %nickname,
                    "the announce could not be encoded; this device is invisible"
                );
                return;
            }
        };

        let mut packet = self.build_packet(
            MessageType::Announce,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        packet.timestamp = now_ms;
        // Signed over the packet preimage, not over `canonicalAnnounceBytes`.
        // bitchat has both, but `buildAnnounceSignature` has no callers outside
        // its own tests — the wire uses `noiseService.signPacket(packet)`
        // (`BLEService.swift:1685`), and an announce signed the other way is
        // dropped by every real peer before it reaches their registry.
        let packet = self.maybe_sign(packet, true);

        self.state.with(|state| state.last_announce_ms = now_ms);
        self.transmit(&packet, None, None).await;
    }

    /// Sign a packet over its own preimage, when the caller asked for it.
    fn maybe_sign(&self, mut packet: BitchatPacket, sign: bool) -> BitchatPacket {
        if sign && let Ok(signature) = sign_packet(&self.identity, &packet) {
            packet.signature = Some(signature);
        }
        packet
    }

    fn build_packet(
        &self,
        msg_type: MessageType,
        recipient: Option<&PeerId>,
        payload: Vec<u8>,
        ttl: u8,
    ) -> BitchatPacket {
        let sender_id = self
            .identity
            .peer_id()
            .to_routing_bytes()
            .unwrap_or([0u8; SENDER_ID_LEN]);
        // Broadcast is encoded by *omitting* the recipient, which is what
        // bitchat sends (`recipientID: nil`). The explicit all-`0xFF` form is
        // also accepted by them, but the omitted form is what their own packets
        // carry, so sending it keeps our traffic byte-comparable with theirs.
        let recipient_id = recipient.and_then(PeerId::to_routing_bytes);
        BitchatPacket::new(
            msg_type,
            sender_id,
            recipient_id,
            now_millis(),
            payload,
            ttl,
        )
    }

    /// Plan and write one packet.
    async fn transmit(
        &self,
        packet: &BitchatPacket,
        directed_peer: Option<&PeerId>,
        ingress_link: Option<&LinkId>,
    ) {
        let padding = fragment::pads_ble_frame(packet.msg_type);
        let Ok(encoded) = codec::encode(packet, padding) else {
            return;
        };

        let (plan, frames) = {
            let links = self.state.link_candidates();
            let request = OutboundPlanRequest::new(packet, encoded.len(), &links)
                .with_ingress_link(ingress_link)
                .with_directed_only_peer(directed_peer)
                .with_direct_announce_ttl(DIRECT_ANNOUNCE_TTL);
            let plan = planner::plan(&request);

            let frames = match plan.fragment_chunk_size {
                Some(chunk) => {
                    let mut id = [0u8; 8];
                    id.copy_from_slice(&packet.packet_id()[..8]);
                    match fragment::plan_fragments(packet, chunk, id) {
                        Ok(fragments) => fragments
                            .iter()
                            .filter_map(|f| codec::encode(f, fragment::pads_ble_frame(f.msg_type)).ok())
                            .collect(),
                        Err(_) => Vec::new(),
                    }
                }
                None => vec![encoded],
            };
            (plan, frames)
        };

        if plan.should_spool_directed
            && let Some(peer) = plan.directed_peer.clone()
        {
            // Directed, with no link to go down. Holding it lets an ordinary
            // link gap heal instead of dropping the packet.
            self.state.with(|state| {
                state.spool.enqueue(
                    packet.clone(),
                    peer,
                    planner::message_id(packet),
                    now_secs(),
                );
            });
            return;
        }

        if plan.selected.is_empty() {
            // Not an error — a broadcast with no links yet is normal on a quiet
            // mesh. Logged because the alternative is indistinguishable from a
            // packet that went out and was ignored, and telling those two apart
            // is most of what bringing up a second device consists of.
            tracing::debug!(
                target: "mesh::ble",
                msg_type = packet.msg_type,
                links = self.state.link_candidates().len(),
                "nothing to write to; packet not sent"
            );
            return;
        }
        tracing::debug!(
            target: "mesh::ble",
            msg_type = packet.msg_type,
            links = plan.selected.len(),
            frames = frames.len(),
            "writing"
        );

        let priority = planner::priority(packet);
        let write_priority = chatter_mesh_link::backpressure::WritePriority {
            level: priority.level,
            suborder: priority.suborder,
        };

        for link in plan.selected.iter() {
            for frame in &frames {
                match self.radio.send(link, frame).await {
                    Ok(()) => {}
                    Err(error) if should_buffer(&error) => {
                        // Routine: BLE controller queues fill, and there is no
                        // retransmission underneath us, so discarding here loses
                        // the packet outright.
                        let admitted = self.state.with(|state| {
                            state.writes.enqueue(
                                link,
                                frame.clone(),
                                write_priority,
                                PENDING_WRITE_BUFFER_CAP_BYTES,
                            )
                        });
                        // The buffer is byte-capped, so enqueue can refuse. Dropping
                        // the outcome would turn a refusal into a silent packet loss,
                        // which is exactly what #[must_use] on WriteEnqueue is for.
                        if !matches!(admitted, WriteEnqueue::Enqueued { .. }) {
                            tracing::warn!(
                                link = %link,
                                "mesh: outbound write buffer full, packet dropped"
                            );
                        }
                        self.state.with(|_state| {
                        });
                    }
                    Err(error) => {
                        tracing::debug!(target: "mesh::ble", link = %link, %error, "write failed");
                    }
                }
            }
        }
    }

    async fn relay(&self, ingress: &LinkId, packet: &BitchatPacket, local: &PeerId) {
        let degree = self.state.with(|state| state.peers.connected_count());
        let sender_is_self = PeerId::from_bytes(&packet.sender_id) == *local;
        let recipient_is_self = self.addressed_to_us(packet);
        let input = RelayInput::from_packet(
            packet,
            sender_is_self,
            recipient_is_self,
            degree,
            chatter_mesh::relay::HIGH_DEGREE_THRESHOLD,
        );
        let mut jitter = |low: u32, high: u32| -> u32 {
            if high <= low {
                return low;
            }
            low + (now_millis() as u32) % (high - low)
        };
        let decision = relay::decide(&input, &mut jitter);
        if !decision.should_relay {
            return;
        }

        let mut forwarded = packet.clone();
        forwarded.ttl = decision.new_ttl;
        // The delay is collision avoidance on a shared channel, so it has to be
        // a real wait rather than a hint. Spawned so the loop keeps servicing the
        // radio while it elapses.
        let radio = Arc::clone(&self.radio);
        let state = Arc::clone(&self.state);
        let ingress = ingress.clone();
        let delay = u64::from(decision.delay_ms);
        tokio::spawn(async move {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            transmit_forwarded(&radio, &state, &forwarded, &ingress).await;
        });
    }

    // -------------------------------------------------------------------- tick

    async fn on_tick(&mut self) {
        let now_ms = now_millis();
        let now_s = now_secs();

        // Scan duty cycle. Scanning is the expensive half of BLE, so it runs
        // intermittently while advertising stays up continuously.
        let configured_duty = self.config.scan_duty_cycle;
        let scan = self.state.with(|state| {
            let connected = state.peers.connected_count();
            let recent_traffic = now_ms.saturating_sub(state.last_traffic_ms)
                < duty::RECENT_TRAFFIC_FORCE_SCAN_MS;
            let duty_enabled = state.duty_enabled && configured_duty;
            let command = state.duty.update(
                duty_enabled,
                state.app_is_active,
                connected,
                recent_traffic,
                now_ms,
            );
            match command {
                ScanCommand::Unchanged => state.duty.poll(now_ms),
                other => other,
            }
        });
        match scan {
            ScanCommand::StartScanning => {
                let _ = self.radio.set_scanning(true).await;
            }
            ScanCommand::StopScanning => {
                let _ = self.radio.set_scanning(false).await;
            }
            ScanCommand::Unchanged => {}
        }

        // Drain the connection queue: discoveries that were queued rather than
        // dialled immediately.
        let next = self.state.with(|state| {
            let connected_or_connecting =
                state.links.count_role(LinkRole::Central) + state.connecting.len();
            if connected_or_connecting >= MAX_CENTRAL_LINKS {
                return QueueDecision::Idle;
            }
            let connecting = state.connecting.clone();
            state.scheduler.next_candidate(
                connected_or_connecting,
                |address| connecting.contains(address),
                now_ms,
            )
        });
        if let QueueDecision::Connect(candidate) = next {
            self.dial(&candidate.address.clone()).await;
        }

        // Announce cadence, cleanup, and the directed spool.
        let plan = self.state.with(|state| {
            let connected = state.peers.connected_count();
            let elapsed = now_ms.saturating_sub(state.last_announce_ms);
            let recent_traffic =
                now_ms.saturating_sub(state.last_traffic_ms) < duty::MAINTENANCE_RECENT_TRAFFIC_MS;
            let cycle = state.maintenance_cycle;
            let plan = duty::maintenance_plan(
                cycle,
                connected,
                state.peers.is_empty(),
                elapsed,
                recent_traffic,
                duty::no_jitter,
            );
            state.maintenance_cycle = if plan.should_reset_counter {
                0
            } else {
                cycle.saturating_add(1)
            };
            plan
        });

        if plan.should_send_announce {
            self.announce().await;
        }
        if plan.should_ensure_advertising {
            // Nobody can see us. Advertising is the first suspect, and a restart
            // is the only lever the trait offers.
            let _ = self.radio.start().await;
        }
        if plan.should_run_cleanup {
            self.cleanup(now_ms, now_s);
        }
        if plan.should_flush_directed_spool {
            self.flush_directed_spool(now_s).await;
        }

        self.flush_buffered_writes().await;

        // Chunked file transfers, which nothing else can schedule either: the
        // send side is paced by this tick rather than by a loop over the file, and
        // the receive side has no other way to notice that a transfer went quiet.
        self.pump_streamed_files(now_s, now_millis()).await;
        self.request_missing_chunks(now_s).await;

        // Gossip sync, which nothing else can schedule.
        let connected: Vec<PeerId> = self
            .state
            .with(|state| state.peers.connected_peer_ids());
        match self.gossip.maintenance(&connected, now_s) {
            Ok(sends) => {
                for send in sends {
                    self.dispatch_sync(send).await;
                }
            }
            Err(error) => self.degraded(format!("gossip sync maintenance failed: {error}")),
        }
        if let Err(error) = self.gossip.persist_if_dirty(now_s) {
            self.degraded(format!("gossip archive could not be persisted: {error}"));
        }

        let _ = self.router.send(RouterCommand::Maintenance);
    }

    fn cleanup(&mut self, now_ms: u64, now_s: u64) {
        self.state.with(|state| {
            state.dedup.cleanup(now_s);
            for expired in state.fragments.evict_expired(now_s) {
                tracing::warn!(
                    target: "mesh::ble",
                    peer = %hex_encode(&expired.sender),
                    received = expired.received,
                    expected = expired.expected,
                    is_broadcast = expired.is_broadcast,
                    lifetime_secs = chatter_mesh::fragment::FRAGMENT_LIFETIME_SECS,
                    "an incomplete reassembly expired; a packet was sent but never \
                     finished arriving"
                );
            }
            state
                .ingress
                .prune(now_ms, chatter_mesh_link::registry::INGRESS_RECORD_LIFETIME_MS);
            state.self_broadcasts.prune(
                now_ms.saturating_sub(chatter_mesh_link::guard::SELF_BROADCAST_RETENTION_MILLIS),
            );
            state.scheduler.prune_backoff(now_ms);
            state
                .spool
                .prune_expired(now_s, DIRECTED_SPOOL_WINDOW_SECS);
        });
        if let Err(error) = self.couriers.prune_expired(now_s) {
            tracing::debug!(target: "mesh::ble", %error, "courier pool prune failed");
        }
        // Transfers that have every chunk but were never stored: the process ended
        // between the last chunk and the store, or the store itself failed. Every
        // byte is already on disk and nothing retransmits, so this periodic retry
        // is the only recovery. On the cleanup cadence rather than the tick, so a
        // persistent failure retries occasionally instead of twice a second.
        for id in self.files_in.completed() {
            let nickname = self
                .files_in
                .get(&id)
                .and_then(|transfer| {
                    self.state
                        .with(|state| state.peers.nickname(&transfer.sender, false).map(str::to_string))
                })
                .unwrap_or_else(|| "a peer".to_string());
            self.complete_transfer(&id, &nickname);
        }
        // A sender that walked out of range mid-transfer would otherwise leave its
        // partial file in the spool forever. The window is deliberately long — the
        // point of persisting the bitmap is surviving outages that last minutes —
        // so this bounds disk, not patience.
        self.files_in.abandon_stale(now_s, TRANSFER_ABANDON_SECS);
        self.reconcile_peers();
    }

    async fn flush_directed_spool(&mut self, now_s: u64) {
        let spooled = self.state.with(|state| {
            state
                .spool
                .drain_unexpired(now_s, DIRECTED_SPOOL_WINDOW_SECS)
        });
        for packet in spooled {
            self.transmit(&packet.packet, Some(&packet.recipient), None)
                .await;
        }
    }

    async fn flush_buffered_writes(&self) {
        let pending = self.state.with(|state| {
            let links: Vec<LinkId> = state.writes.link_ids().cloned().collect();
            links
                .into_iter()
                .map(|link| {
                    let writes = state.writes.take_all(&link);
                    (link, writes)
                })
                .collect::<Vec<_>>()
        });

        for (link, writes) in pending {
            let mut requeue = Vec::new();
            let mut blocked = false;
            for write in writes {
                if blocked {
                    requeue.push(write);
                    continue;
                }
                match self.radio.send(&link, &write.bytes).await {
                    Ok(()) => {}
                    Err(error) if should_buffer(&error) => {
                        // Still full. Stop and put the rest back in order — a
                        // reordered fragment stream is worse than a delayed one.
                        blocked = true;
                        requeue.push(write);
                    }
                    Err(_) => {}
                }
            }
            if !requeue.is_empty() {
                self.state
                    .with(|state| state.writes.prepend(&link, requeue));
            }
        }
    }

    // ----------------------------------------------------------------- helpers

    async fn hand_over_courier_mail(
        &mut self,
        peer: &PeerId,
        recipient_key: &[u8; 32],
        now_s: u64,
        ingress: AnnounceIngress,
    ) {
        let envelopes = self
            .couriers
            .take_envelopes(recipient_key, now_s, ingress)
            .unwrap_or_default();
        for envelope in envelopes {
            let Ok(payload) = envelope.encode() else {
                continue;
            };
            let packet = self.build_packet(
                MessageType::CourierEnvelope,
                Some(peer),
                payload,
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            );
            self.transmit(&packet, Some(peer), None).await;
        }
    }

    async fn dispatch_sync(&self, send: chatter_mesh_store::sync::SyncSend) {
        let packet = self.maybe_sign(send.packet, send.sign_before_send);
        self.transmit(&packet, send.to.as_ref(), None).await;
    }

    fn emit_dm(&self, peer: &PeerId, message_id: &str, content: &str, timestamp_ms: u64) {
        let nickname = self
            .state
            .with(|state| state.peers.nickname(peer, false).map(str::to_string))
            .unwrap_or_else(|| peer.as_str().to_string());
        let verified = self
            .state
            .with(|state| state.peers.info(peer).is_some_and(|p| p.verified_nickname));
        let inbound = InboundMessage {
            wire_id: message_id.to_string(),
            conversation: MeshConversation::Dm(peer.clone()),
            sender: peer.clone(),
            nickname: nickname.clone(),
            content: content.to_string(),
            timestamp_ms,
        };
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message: inbound.to_domain(),
            conversation: inbound.conversation,
            sender: peer.clone(),
            nickname,
            is_local: false,
            sender_verified: verified,
        })));
    }

    fn addressed_to_us(&self, packet: &BitchatPacket) -> bool {
        let local = self.identity.peer_id().to_routing_bytes();
        match (packet.recipient_id, local) {
            (Some(recipient), Some(local)) => recipient == local,
            _ => false,
        }
    }

    fn reconcile_peers(&mut self) {
        let now = now_millis();
        let removed = self.state.with(|state| {
            let presence = state.links.presence_by_peer();
            let changes = state.peers.reconcile_connectivity(now, &presence);
            for peer in &changes.disconnected {
                state.sessions.forget(peer);
            }
            changes
        });
        for removed in removed.removed {
            self.gossip.remove_announcement_for_peer(&removed.peer);
        }
        self.emit_peers();
    }

    fn emit_peers(&self) {
        self.events.send(MeshEvent::Peers(self.state.peers()));
    }

    fn emit_status(&self) {
        let status = self
            .state
            .status(self.nostr_state.connected_relay_count());
        self.events.send(MeshEvent::Status(status));
    }

    fn degraded(&self, message: String) {
        tracing::debug!(target: "mesh::ble", "{message}");
        self.events.send(MeshEvent::Degraded(message));
    }
}

/// Write a relayed packet, with the ingress link excluded.
///
/// A free function so the relay path can be spawned: it needs the radio and the
/// shared state and nothing else from the runtime.
async fn transmit_forwarded(
    radio: &Arc<dyn MeshLink>,
    state: &Arc<MeshState>,
    packet: &BitchatPacket,
    ingress: &LinkId,
) {
    let padding = fragment::pads_ble_frame(packet.msg_type);
    let Ok(encoded) = codec::encode(packet, padding) else {
        return;
    };
    let links = state.with(|inner| link_candidates_locked(inner));
    let request = OutboundPlanRequest::new(packet, encoded.len(), &links)
        // Split horizon: never send a packet back down the link it arrived on.
        .with_ingress_link(Some(ingress))
        .with_direct_announce_ttl(DIRECT_ANNOUNCE_TTL);
    let plan = planner::plan(&request);

    let frames: Vec<Vec<u8>> = match plan.fragment_chunk_size {
        Some(chunk) => {
            let mut id = [0u8; 8];
            id.copy_from_slice(&packet.packet_id()[..8]);
            match fragment::plan_fragments(packet, chunk, id) {
                Ok(fragments) => fragments
                    .iter()
                    .filter_map(|f| codec::encode(f, fragment::pads_ble_frame(f.msg_type)).ok())
                    .collect(),
                Err(_) => return,
            }
        }
        None => vec![encoded],
    };

    let priority = planner::priority(packet);
    let write_priority = chatter_mesh_link::backpressure::WritePriority {
        level: priority.level,
        suborder: priority.suborder,
    };

    for link in plan.selected.iter() {
        for frame in &frames {
            match radio.send(link, frame).await {
                Ok(()) => {}
                Err(error) if should_buffer(&error) => {
                    let admitted = state.with(|inner| {
                        inner.writes.enqueue(
                            link,
                            frame.clone(),
                            write_priority,
                            PENDING_WRITE_BUFFER_CAP_BYTES,
                        )
                    });
                    if !matches!(admitted, WriteEnqueue::Enqueued { .. }) {
                        tracing::warn!(
                            link = %link,
                            "mesh: outbound write buffer full, packet dropped"
                        );
                    }
                    state.with(|_inner| {
                    });
                }
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::mesh::events::MeshDelivery;
    use crate::services::mesh::mock::MockLink;
    use chatter_mesh_store::MemoryStorage;
    use chatter_mesh_store::storage::Storage;
    use rand_core_06::OsRng;

    fn runtime() -> (BleRuntime, Arc<MockLink>, Arc<MeshState>) {
        let (runtime, radio, state, _timeline) = runtime_parts();
        (runtime, radio, state)
    }

    /// The same runtime, with the timeline it stores through.
    ///
    /// Only the file tests need it: a file has to be readable back out of the
    /// store to prove anything, because unlike a text message the row is only a
    /// pointer to bytes that landed somewhere else.
    fn runtime_parts() -> (
        BleRuntime,
        Arc<MockLink>,
        Arc<MeshState>,
        Arc<crate::services::mesh::timeline::MeshTimeline>,
    ) {
        let radio = MockLink::shared();
        let identity = Arc::new(MeshIdentity::generate(&mut OsRng));
        let state = Arc::new(MeshState::new(identity.peer_id()));
        let nostr_state = Arc::new(NostrState::new());
        let timeline = crate::services::mesh::timeline::temp_timeline();
        let events = MeshEvents::new(64, Arc::clone(&timeline));
        let storage = SharedStorage::new(Arc::new(MemoryStorage::new()) as Arc<dyn Storage>);
        let router = Arc::new(RouterHandle::spawn(super::super::router::RouterDeps {
            state: Arc::clone(&state),
            nostr_state: Arc::clone(&nostr_state),
            identity: Arc::clone(&identity),
            storage: Arc::new(MemoryStorage::new()),
            sealer: Arc::new(NoopSealer),
            favorites: Arc::new(super::super::transport::MeshFavorites::new()),
            ble: tokio::sync::mpsc::unbounded_channel().0,
            nostr: tokio::sync::mpsc::unbounded_channel().0,
            events: events.clone(),
            bridge_enabled: false,
        }));
        let runtime = BleRuntime::new(
            Arc::clone(&radio) as Arc<dyn MeshLink>,
            Arc::clone(&state),
            nostr_state,
            identity,
            Arc::new(MeshConfig::default()),
            events,
            router,
            storage,
        );
        (runtime, radio, state, timeline)
    }

    /// A peer in the registry with a known signing key, as a verified announce
    /// would have left it. Returns the identity that has to sign for it.
    fn register_verified_peer(
        state: &MeshState,
        peer: &PeerId,
        nickname: &str,
    ) -> MeshIdentity {
        let identity = MeshIdentity::generate(&mut OsRng);
        let announcement = AnnouncementPacket::new(
            nickname.to_string(),
            identity.static_public().to_vec(),
            identity.signing_public().to_vec(),
            None,
            None,
            None,
        );
        state.with(|inner| {
            inner
                .peers
                .upsert_verified_announce(peer, &announcement, true, now_millis());
        });
        identity
    }

    /// One file transfer on the wire, optionally signed.
    fn file_packet(
        sender: &PeerId,
        identity: &MeshIdentity,
        file: &BitchatFilePacket,
        recipient: Option<&PeerId>,
        sign: bool,
    ) -> BitchatPacket {
        let mut packet = BitchatPacket::new(
            MessageType::FileTransfer,
            sender.to_routing_bytes().expect("routing bytes"),
            recipient.and_then(PeerId::to_routing_bytes),
            now_millis(),
            file.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        if sign {
            packet.signature = Some(sign_packet(identity, &packet).expect("signs"));
        }
        packet
    }

    fn sample_file() -> BitchatFilePacket {
        BitchatFilePacket::new(
            Some("note.txt".to_string()),
            None,
            Some("text/plain".to_string()),
            b"the contents".to_vec(),
        )
    }

    struct NoopSealer;

    impl chatter_mesh_store::outbox::Sealer for NoopSealer {
        fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, chatter_mesh_store::StoreError> {
            Ok(plaintext.to_vec())
        }

        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, chatter_mesh_store::StoreError> {
            Ok(sealed.to_vec())
        }
    }

    #[tokio::test]
    async fn a_peer_heard_only_by_relay_is_not_marked_connected() {
        // Found with three devices in one room: the second phone's announces
        // arrived over the *first* phone's link, we marked it connected anyway,
        // and `reconcile_connectivity` demoted it 8s later for having no link of
        // its own — so it flapped `reconnected=true` on every announce forever.
        //
        // "We heard from it" is not "it is reachable on a link of its own".
        let (mut runtime, _radio, state) = runtime();
        let relay = PeerId::from_str("aaaaaaaaaaaaaaaa");
        let distant = PeerId::from_str("bbbbbbbbbbbbbbbb");

        // One link, owned by the relay.
        let link = LinkId::new("/mock/relay");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
            inner.links.bind_if_unbound(&link, &relay);
        });

        // A signed announce authored by the distant peer, arriving on that link.
        let identity = MeshIdentity::generate(&mut OsRng);
        let announcement = AnnouncementPacket::new(
            "distant".to_string(),
            identity.static_public().to_vec(),
            identity.signing_public().to_vec(),
            None,
            None,
            None,
        );
        let mut packet = BitchatPacket::new(
            MessageType::Announce,
            distant.to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            announcement.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        packet.signature = Some(
            chatter_mesh_noise::sign::sign_packet(&identity, &packet).expect("signs"),
        );

        runtime.on_announce(&link, &packet, &distant).await;

        assert!(
            state.with(|inner| inner.peers.info(&distant).is_some()),
            "a relayed peer is still a peer we know about"
        );
        assert!(
            !state.with(|inner| inner.peers.info(&distant).expect("known").connected),
            "but not one we can reach on a link of its own"
        );
    }

    #[tokio::test]
    async fn an_empty_public_message_is_not_stored_as_a_blank_row() {
        // An empty payload is valid UTF-8 and decodes without complaint, so it used
        // to become an ordinary row holding `Text("")`. The room then showed a
        // bubble with a sender and a timestamp and no body — indistinguishable from
        // a message whose text failed to load, and impossible to tell apart later
        // because the row is all any surface has.
        //
        // Neither side composes one: `send_room` refuses via `require_content`, and
        // bitchat refuses on its send path too (`MeshCore.kt:415`). Neither checks
        // on receive, which is the gap.
        let (mut runtime, _radio, state) = runtime();
        let sender = PeerId::from_str("eeeeeeeeeeeeeeee");
        let identity = register_verified_peer(&state, &sender, "quiet");
        let mut events = runtime.events.subscribe();

        for payload in [b"".to_vec(), b"   ".to_vec(), b"\n\t".to_vec()] {
            let packet = signed(&sender, &identity, MessageType::Message, payload, true);
            runtime.on_public_message(&packet, &sender);
        }

        assert!(
            events.try_recv().is_err(),
            "an empty broadcast is not a message and must not reach the timeline"
        );

        // The guard is on emptiness alone, not on the sender: a real message from
        // the same peer still arrives.
        let packet = signed(
            &sender,
            &identity,
            MessageType::Message,
            b"still here".to_vec(),
            true,
        );
        runtime.on_public_message(&packet, &sender);
        match events.try_recv() {
            Ok(MeshEvent::Message(message)) => assert_eq!(
                super::super::mapping::text_of(&message.message),
                Some("still here")
            ),
            other => panic!("expected the real message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn our_own_broadcast_coming_straight_back_is_not_shown_twice() {
        // Reported from the phone: every send appeared twice, once labelled
        // "broadcast" and once "sent". The second row is our own packet returning
        // — flooded or synced back — carrying a different derived wire ID, so
        // nothing collapsed the two, and with no delivery state it fell through to
        // the one-to-one word "sent" beside its own correct copy.
        let (mut runtime, radio, state) = runtime();
        let local = runtime.identity.peer_id();
        let link_out = LinkId::new("/mock/out");
        state.with(|inner| {
            inner.links.link_up(link_out.clone(), LinkRole::Central);
            inner.links.set_writable(&link_out, true);
            inner.max_write.insert(link_out.clone(), 512);
        });
        let mut events = runtime.events.subscribe();

        runtime.broadcast_public("MSG-1", "said once").await;
        let packet = codec::decode(&radio.writes()[0].1).expect("decodable");
        // The local mirror: the copy the user is already looking at.
        assert!(matches!(events.try_recv(), Ok(MeshEvent::Message(_))));

        runtime.on_public_message(&packet, &local);

        assert!(
            events.try_recv().is_err(),
            "our own broadcast returning must not produce a second row"
        );
    }

    #[tokio::test]
    async fn our_own_announce_coming_back_does_not_register_us_as_a_peer() {
        // The ingress guard admits a self-authored TTL-0 sync response on purpose,
        // so our own announce really does reach `on_announce`. If it registers, we
        // become our own peer: "N nearby" counts us, and a link binds to our own
        // identity. bitchat refuses it as `.reject(.selfAnnounce)`.
        let (mut runtime, _radio, state) = runtime();
        let local = runtime.identity.peer_id();
        let link_out = LinkId::new("/mock/out");
        state.with(|inner| {
            inner.links.link_up(link_out.clone(), LinkRole::Central);
            inner.links.set_writable(&link_out, true);
            inner.max_write.insert(link_out.clone(), 512);
        });

        // Build the announce exactly as we would send it, then feed it back.
        runtime.announce().await;
        let sent = _radio.writes();
        let packet = codec::decode(&sent[0].1).expect("decodable");
        assert_eq!(
            PeerId::from_bytes(&packet.sender_id),
            local,
            "the fixture must really be our own announce"
        );

        let link = LinkId::new("/mock/self");
        runtime.on_announce(&link, &packet, &local).await;

        assert!(
            state.with(|inner| inner.peers.info(&local).is_none()),
            "we must never appear in our own peer registry"
        );
        assert!(
            state.with(|inner| inner.links.peer_for(&link).is_none()),
            "and no link may be bound to our own identity"
        );
    }

    #[tokio::test]
    async fn a_message_of_ours_recovered_from_a_peer_is_attributed_to_us() {
        // Gossip sync hands back what we wrote — that is how a device with no
        // database gets its own history again. It must land as ours: verified
        // against our own key (we are deliberately not in our own registry) and
        // marked local, or our own words appear on screen as a stranger's.
        let (mut runtime, _radio, _state) = runtime();
        let local = runtime.identity.peer_id();
        let link_out = LinkId::new("/mock/out");
        _state.with(|inner| {
            inner.links.link_up(link_out.clone(), LinkRole::Central);
            inner.links.set_writable(&link_out, true);
            inner.max_write.insert(link_out.clone(), 512);
        });
        let mut events = runtime.events.subscribe();

        // Built directly rather than through `broadcast_public`, because that
        // records the packet in the self-broadcast tracker — which is precisely
        // what marks a message as "already shown" and suppresses it. Recovery is
        // the opposite case: a packet we authored in some *earlier* session, whose
        // tracker entry died with the process, arriving from a peer's archive.
        let packet = runtime.maybe_sign(
            runtime.build_packet(
                MessageType::Message,
                None,
                b"our own words".to_vec(),
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            ),
            true,
        );
        assert!(
            !_state.with(|inner| inner.self_broadcasts.contains(&packet)),
            "the fixture must model a packet this session never sent"
        );

        runtime.on_public_message(&packet, &local);

        match events.try_recv() {
            Ok(MeshEvent::Message(message)) => {
                assert!(message.is_local, "a recovered self message is ours");
                assert_eq!(message.sender, local);
                assert_eq!(
                    super::super::mapping::text_of(&message.message),
                    Some("our own words")
                );
            }
            other => panic!("expected the recovered message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_announce_is_signed_and_carries_both_public_keys() {
        let (mut runtime, radio, state) = runtime();
        // One writable link, so the announce has somewhere to go.
        let link = LinkId::new("/mock/one");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });

        runtime.announce().await;

        let writes = radio.writes();
        assert_eq!(writes.len(), 1, "the announce went out on the one link");
        let packet = codec::decode(&writes[0].1).expect("decodable");
        assert_eq!(packet.message_type(), Some(MessageType::Announce));

        let announcement =
            AnnouncementPacket::decode(&packet.payload).expect("announce payload decodes");
        assert_eq!(
            announcement.noise_public_key,
            runtime.identity.static_public().to_vec()
        );
        assert_eq!(
            announcement.signing_public_key,
            runtime.identity.signing_public().to_vec()
        );

        assert!(packet.signature.is_some(), "announces are signed");
        let signing_key = <[u8; 32]>::try_from(announcement.signing_public_key.as_slice())
            .expect("32 bytes");
        assert!(
            verify_packet(&packet, &signing_key),
            "the signature must verify against the advertised key, over the \
             packet preimage — a peer verifying `canonicalAnnounceBytes` \
             instead never enters anyone's registry"
        );
        assert!(
            packet.recipient_id.is_none(),
            "an announce is addressed to everyone by omitting the recipient"
        );
    }

    #[tokio::test]
    async fn nothing_is_written_when_no_link_is_writable() {
        let (mut runtime, radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        // Bound and up, but not writable.
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.max_write.insert(link.clone(), 512);
        });
        runtime.announce().await;
        assert_eq!(radio.write_count(), 0);
    }

    #[tokio::test]
    async fn a_public_message_is_mirrored_locally_and_recorded_as_our_own() {
        let (mut runtime, radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });
        let mut events = runtime.events.subscribe();

        runtime.broadcast_public("MSG-1", "hello room").await;

        let writes = radio.writes();
        assert_eq!(writes.len(), 1);
        let packet = codec::decode(&writes[0].1).expect("decodable");
        assert_eq!(packet.message_type(), Some(MessageType::Message));
        assert!(packet.is_broadcast());
        // The three things a real bitchat checks before it will show this, and
        // the three the port originally got wrong: the payload is the message
        // text and nothing else, the packet is signed, and "everyone" is encoded
        // by omitting the recipient.
        assert_eq!(packet.payload, b"hello room");
        assert!(packet.recipient_id.is_none());
        let signing_key = runtime.identity.signing_public();
        assert!(
            verify_packet(&packet, &signing_key),
            "an unsigned public message is dropped before its payload is read"
        );

        // The echo of our own broadcast must be recognisable.
        assert!(state.with(|inner| inner.self_broadcasts.contains(&packet)));
        assert_eq!(
            state.with(|inner| inner.self_broadcasts.message_id(&packet).map(str::to_string)),
            Some("MSG-1".to_string())
        );

        match events.try_recv() {
            Ok(MeshEvent::Message(message)) => {
                assert!(message.is_local);
                assert_eq!(message.conversation, MeshConversation::Room);
                assert_eq!(
                    message.message.conversation_type,
                    crate::domain::message::ConversationType::Mesh
                );
            }
            other => panic!("expected a mirrored message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_write_rejection_buffers_rather_than_dropping_the_packet() {
        let (mut runtime, radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });
        radio.set_reject_writes(true);

        runtime.broadcast_public("MSG-1", "hello room").await;

        assert_eq!(radio.write_count(), 0, "the controller refused the write");
        assert_eq!(
            state.with(|inner| inner.writes.queued_count(&link)),
            1,
            "a rejected write is retryable and must not be discarded"
        );

        // Once the controller drains, the buffered write goes out.
        radio.set_reject_writes(false);
        runtime.flush_buffered_writes().await;
        assert_eq!(radio.write_count(), 1);
        assert_eq!(state.with(|inner| inner.writes.queued_count(&link)), 0);
    }

    #[tokio::test]
    async fn a_connected_link_is_treated_as_writable_immediately() {
        let (mut runtime, _radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        runtime
            .on_link_event(LinkEvent::Connected {
                link: link.clone(),
                role: LinkRole::Peripheral,
                max_write: 244,
            })
            .await;
        assert!(state.with(|inner| inner.links.is_writable(&link)));
        assert_eq!(state.with(|inner| inner.max_write.get(&link).copied()), Some(244));
    }

    #[tokio::test]
    async fn a_disconnect_drops_every_piece_of_link_state() {
        let (mut runtime, _radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        runtime
            .on_link_event(LinkEvent::Connected {
                link: link.clone(),
                role: LinkRole::Central,
                max_write: 244,
            })
            .await;
        runtime
            .on_link_event(LinkEvent::Disconnected { link: link.clone() })
            .await;
        assert!(!state.with(|inner| inner.links.contains(&link)));
        assert!(state.with(|inner| inner.max_write.get(&link).is_none()));
        assert!(state.link_candidates().is_empty());
    }

    #[tokio::test]
    async fn losing_the_adapter_clears_every_link() {
        let (mut runtime, _radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        runtime
            .on_link_event(LinkEvent::Connected {
                link,
                role: LinkRole::Central,
                max_write: 244,
            })
            .await;
        runtime
            .on_link_event(LinkEvent::AdapterPowerChanged { powered: false })
            .await;
        assert!(state.with(|inner| inner.links.is_empty()));
        assert!(!state.with(|inner| inner.adapter_powered));
        assert!(state.link_candidates().is_empty());
    }

    #[tokio::test]
    async fn a_private_message_with_no_session_is_held_and_a_handshake_starts() {
        let (mut runtime, radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.bind_if_unbound(&link, &peer);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });

        runtime.send_private(&peer, "MSG-1", "secret").await;

        // The message is held, not sent in the clear.
        let held = state.with(|inner| inner.pending_dms.get(&peer).map(Vec::len));
        assert_eq!(held, Some(1));
        let writes = radio.writes();
        assert_eq!(writes.len(), 1, "only the handshake went out");
        let packet = codec::decode(&writes[0].1).expect("decodable");
        assert_eq!(packet.message_type(), Some(MessageType::NoiseHandshake));
        assert!(
            !writes[0].1.windows(6).any(|w| w == b"secret"),
            "plaintext must never reach the wire"
        );
    }

    #[tokio::test]
    async fn a_file_from_a_verified_peer_is_stored_and_reachable_from_its_row() {
        // A file's row is only a pointer: unlike a text message, "it arrived"
        // and "its bytes are somewhere the user can open" are two claims, and
        // the second is the one that makes the feature exist.
        let (mut runtime, _radio, state, timeline) = runtime_parts();
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");
        let mut events = runtime.events.subscribe();

        let file = sample_file();
        let packet = file_packet(&sender, &identity, &file, None, true);
        runtime.on_file_transfer(&packet, &sender);

        let attachment = match events.try_recv() {
            Ok(MeshEvent::Message(message)) => {
                assert!(!message.is_local, "someone else's file is not ours");
                assert_eq!(message.conversation, MeshConversation::Room);
                assert!(
                    message.sender_verified,
                    "the signature verified, so the sender is verified"
                );
                assert_eq!(
                    mapping::text_of(&message.message),
                    None,
                    "a file has no text body; `Mixed` with no text is the point"
                );
                let attachments = mapping::attachments_of(&message.message);
                assert_eq!(attachments.len(), 1, "one file, one attachment");
                attachments[0]
            }
            other => panic!("expected the file, got {other:?}"),
        };

        // The row survived the store, still naming the same attachment — the
        // event and the durable copy must not disagree about which bytes the
        // message is.
        let history = timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(mapping::attachments_of(&history[0]), [attachment]);

        // And the bytes are really on disk, where the existing attachment read
        // path looks for them.
        let path = crate::services::attachment::attachment_file_path(&attachment);
        assert!(path.exists(), "the content has to be at {}", path.display());
        assert_ne!(
            std::fs::read(&path).expect("readable"),
            file.content,
            "a mesh file lands encrypted, like every other attachment — the \
             bytes sit outside SQLCipher, so storing them in the clear would \
             leave mesh files the one thing in this app lying open on disk"
        );
    }

    #[tokio::test]
    async fn the_same_file_arriving_again_does_not_store_a_second_copy() {
        // The dedup set in `on_packet` catches the flood, but the gossip archive
        // exists to hand public packets back after that window closes. A text
        // message re-arriving is free — `record` is idempotent on the message ID.
        // A file is not: its bytes go to disk *before* the row, so storing first
        // and letting the row be ignored leaves an encrypted megabyte nothing
        // references, and shows the user an attachment the stored row never had.
        let (mut runtime, _radio, state, timeline) = runtime_parts();
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");
        let packet = file_packet(&sender, &identity, &sample_file(), None, true);

        runtime.on_file_transfer(&packet, &sender);
        let first = timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history");
        assert_eq!(first.len(), 1);
        let stored = mapping::attachments_of(&first[0]).to_vec();

        // The same packet, arriving by another path with the dedup entry gone.
        let mut events = runtime.events.subscribe();
        runtime.on_file_transfer(&packet, &sender);

        assert!(
            events.try_recv().is_err(),
            "the second arrival must produce no row and no event"
        );
        let after = timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history");
        assert_eq!(after.len(), 1);
        assert_eq!(
            mapping::attachments_of(&after[0]),
            stored.as_slice(),
            "still pointing at the copy that was stored the first time"
        );
    }

    #[tokio::test]
    async fn an_unsigned_file_transfer_is_dropped() {
        // The security gate, and the reason this handler exists rather than a
        // `_ => {}`. A broadcast's `sender_id` is a header field the sender
        // picked, so without a signature that verifies against the key their
        // announce established, anybody in range publishes files under anybody's
        // name. bitchat names the concrete attack: a peer that overheard a public
        // voice burst spoofing a broadcast note under the talker's ID to
        // overwrite their signature-verified bubble with attacker audio.
        let (mut runtime, _radio, state, timeline) = runtime_parts();
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");
        let mut events = runtime.events.subscribe();

        let packet = file_packet(&sender, &identity, &sample_file(), None, false);
        runtime.on_file_transfer(&packet, &sender);

        assert!(
            events.try_recv().is_err(),
            "an unsigned file must not reach the room"
        );
        assert!(
            timeline
                .history(&MeshConversation::Room, None, 10)
                .expect("history")
                .is_empty(),
            "and nothing about it may reach disk"
        );
    }

    #[tokio::test]
    async fn a_file_from_a_peer_that_never_announced_is_dropped() {
        // A signature is only worth something against a key we obtained
        // independently. With no announce there is nothing to check against, so
        // a self-signed packet would otherwise authenticate itself.
        let (mut runtime, _radio, _state, timeline) = runtime_parts();
        let stranger = PeerId::from_str("cccccccccccccccc");
        let identity = MeshIdentity::generate(&mut OsRng);

        let packet = file_packet(&stranger, &identity, &sample_file(), None, true);
        let mut events = runtime.events.subscribe();
        runtime.on_file_transfer(&packet, &stranger);

        assert!(
            events.try_recv().is_err(),
            "a validly-signed file from an unknown key is still unattributable"
        );
        assert!(
            timeline
                .history(&MeshConversation::Room, None, 10)
                .expect("history")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn our_own_file_coming_straight_back_is_not_stored_twice() {
        // Worse than the duplicate row a returning text message produces: a
        // second pass through `store_file` writes a *second encrypted copy of
        // the bytes* under a new attachment id, so every echo of a 1 MiB file
        // would cost another megabyte of disk.
        let (mut runtime, radio, state, timeline) = runtime_parts();
        let local = runtime.identity.peer_id();
        let link = LinkId::new("/mock/out");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });
        let mut events = runtime.events.subscribe();

        runtime.broadcast_file("MSG-F1", &sample_file()).await;
        let packet = codec::decode(&radio.writes()[0].1).expect("decodable");
        assert_eq!(packet.message_type(), Some(MessageType::FileTransfer));
        // The local mirror: the row the sender is already looking at.
        let sent = match events.try_recv() {
            Ok(MeshEvent::Message(message)) => {
                assert!(message.is_local);
                assert_eq!(
                    message.message.delivery_status,
                    MeshDelivery::Broadcast.as_str(),
                    "nobody acknowledges a room file, so \"sent\" would be a \
                     one-to-one word for something with no recipient"
                );
                mapping::attachments_of(&message.message).to_vec()
            }
            other => panic!("expected the local mirror, got {other:?}"),
        };
        assert_eq!(sent.len(), 1);

        runtime.on_file_transfer(&packet, &local);

        assert!(
            events.try_recv().is_err(),
            "our own file returning must not produce a second row"
        );
        let history = timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history");
        assert_eq!(history.len(), 1, "one row");
        assert_eq!(
            mapping::attachments_of(&history[0]),
            sent.as_slice(),
            "and one copy of the bytes, still the first one"
        );
    }

    #[tokio::test]
    async fn a_file_addressed_to_us_alone_is_not_accepted_in_the_clear() {
        // bitchat delivers a directed `FileTransfer` as a private message
        // (`BLEFileTransferPolicy.deliveryPlan`). This client will not: the
        // packet is unencrypted, and every mesh DM here is Noise-sealed, so
        // showing it in a conversation would teach the UI that private traffic
        // can arrive in the clear. `on_public_message` refuses the legacy
        // directed `Message` for the same reason.
        let (mut runtime, _radio, state, timeline) = runtime_parts();
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");
        let local = runtime.identity.peer_id();
        let mut events = runtime.events.subscribe();

        let packet = file_packet(&sender, &identity, &sample_file(), Some(&local), true);
        runtime.on_file_transfer(&packet, &sender);

        assert!(
            events.try_recv().is_err(),
            "a cleartext file addressed to one person is not a mesh DM"
        );
        assert!(
            timeline
                .history(&MeshConversation::Room, None, 10)
                .expect("history")
                .is_empty(),
            "and it is certainly not a room message"
        );
    }

    #[tokio::test]
    async fn a_sent_file_is_signed_and_broadcast_with_the_payload_intact() {
        let (mut runtime, radio, state, _timeline) = runtime_parts();
        let link = LinkId::new("/mock/one");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });

        let file = sample_file();
        runtime.broadcast_file("MSG-F1", &file).await;

        let writes = radio.writes();
        assert_eq!(writes.len(), 1);
        let packet = codec::decode(&writes[0].1).expect("decodable");
        assert_eq!(packet.message_type(), Some(MessageType::FileTransfer));
        assert!(
            packet.recipient_id.is_none(),
            "\"everyone\" is encoded by omitting the recipient"
        );
        assert!(
            verify_packet(&packet, &runtime.identity.signing_public()),
            "an unsigned file transfer is dropped by the receiver before its \
             payload is read"
        );
        let decoded = BitchatFilePacket::decode(&packet.payload).expect("payload decodes");
        assert_eq!(decoded.content, file.content);
        assert_eq!(decoded.file_name.as_deref(), Some("note.txt"));
        assert_eq!(
            decoded.file_size,
            Some(file.content.len() as u64),
            "the declared size has to match the bytes, or the receiver rejects it"
        );

        // The echo of our own file must be recognisable, or it comes back and is
        // stored a second time.
        assert!(state.with(|inner| inner.self_broadcasts.contains(&packet)));
    }

    // ---------------------------------------------- chunked file transfer (P9)

    use chatter_mesh::tlv::file_stream::MAX_SINGLE_PACKET_FILE_BYTES;

    /// Chunk size for the runtime tests: the smallest a manifest may declare, so
    /// a handful of kilobytes exercises several chunks without a megabyte of IO.
    const TEST_CHUNK: u32 = 1024;

    /// Milliseconds to advance between pumps so chunk pacing never gates a test.
    ///
    /// `AIR_BYTES_PER_SEC` turns a chunk's encoded length into a hold-off, and a
    /// `TEST_CHUNK` chunk is well under a second of air time. Rounded up to a
    /// second so the figure stays correct if either constant moves.
    const CHUNK_AIR_MS_FOR_TESTS: u64 = 1_000;

    /// Point one runtime's receive spool at a directory of its own.
    ///
    /// The attachments base is a process-wide `OnceLock`, so every test in the
    /// process would otherwise share one spool directory and one test's partial
    /// transfer could be restored into another's runtime.
    fn isolate_spool(runtime: &mut BleRuntime) -> std::path::PathBuf {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().to_path_buf();
        Box::leak(Box::new(dir));
        runtime.files_in = FileStreamSpool::new(path.clone());
        path
    }

    /// Position-dependent filler: a chunk written at the wrong offset changes the
    /// digest, which a run of one byte would hide.
    fn body(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn test_manifest(id: [u8; 8], bytes: &[u8]) -> FileManifest {
        use sha2::{Digest, Sha256};
        FileManifest::new(
            id,
            Some("clip.bin".into()),
            Some("application/octet-stream".into()),
            bytes.len() as u64,
            TEST_CHUNK,
            Sha256::digest(bytes).into(),
        )
        .expect("valid manifest")
    }

    fn signed(
        sender: &PeerId,
        identity: &MeshIdentity,
        msg_type: MessageType,
        payload: Vec<u8>,
        sign: bool,
    ) -> BitchatPacket {
        let mut packet = BitchatPacket::new(
            msg_type,
            sender.to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        if sign {
            packet.signature = Some(sign_packet(identity, &packet).expect("signs"));
        }
        packet
    }

    fn manifest_packet(
        sender: &PeerId,
        identity: &MeshIdentity,
        manifest: &FileManifest,
        sign: bool,
    ) -> BitchatPacket {
        signed(
            sender,
            identity,
            MessageType::FileManifest,
            manifest.encode().expect("encodes"),
            sign,
        )
    }

    fn chunk_packet(
        sender: &PeerId,
        identity: &MeshIdentity,
        manifest: &FileManifest,
        bytes: &[u8],
        index: u32,
        sign: bool,
    ) -> BitchatPacket {
        let (offset, len) = manifest.chunk_span(index).expect("in range");
        let slice = bytes[offset as usize..offset as usize + len].to_vec();
        signed(
            sender,
            identity,
            MessageType::FileChunk,
            FileChunk::new(manifest.transfer_id, index, slice)
                .expect("valid chunk")
                .encode()
                .expect("encodes"),
            sign,
        )
    }

    /// A writable link, so `transmit` produces writes instead of logging that
    /// there was nowhere to send.
    ///
    /// `max_write` is a parameter because it decides whether one packet becomes
    /// one write. Real links negotiate ~514, so a real 32 KiB chunk is ~70
    /// fragments — that split is the fragmenter's business and is tested there.
    /// The send tests below pass a value above the chunk size so that "one chunk
    /// per tick" can be counted in writes, which is the property they are
    /// actually about.
    fn writable_link(state: &MeshState, name: &str, max_write: usize) -> LinkId {
        let link = LinkId::new(name);
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), max_write);
        });
        link
    }

    #[tokio::test]
    async fn a_chunked_transfer_round_trips_into_a_row_with_its_bytes_on_disk() {
        // The whole of P9 in one test. A payload this size cannot arrive as one
        // packet at all — `FRAGMENT_LIFETIME_SECS` is measured from the first
        // fragment, so a single-packet send of it is evicted at 30 s however far
        // it got — and it arrives here in chunks, each verified in its own right,
        // written straight to disk, digest-checked, and encrypted into place.
        let (mut runtime, _radio, state, timeline) = runtime_parts();
        let spool = isolate_spool(&mut runtime);
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");
        let mut events = runtime.events.subscribe();

        let bytes = body(2_500);
        let manifest = test_manifest([0x10; 8], &bytes);
        assert_eq!(manifest.chunk_count, 3);

        runtime.on_file_manifest(&manifest_packet(&sender, &identity, &manifest, true), &sender);
        assert!(
            events.try_recv().is_err(),
            "a manifest is not a message; nothing may surface until the bytes are \
             here and verified"
        );
        for index in 0..3 {
            runtime.on_file_chunk(
                &chunk_packet(&sender, &identity, &manifest, &bytes, index, true),
                &sender,
            );
        }

        let attachment = match events.try_recv() {
            Ok(MeshEvent::Message(message)) => {
                assert!(!message.is_local);
                assert_eq!(message.conversation, MeshConversation::Room);
                assert!(message.sender_verified);
                let attachments = mapping::attachments_of(&message.message);
                assert_eq!(attachments.len(), 1);
                attachments[0]
            }
            other => panic!("expected the completed file, got {other:?}"),
        };

        let history = timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(mapping::attachments_of(&history[0]), [attachment]);

        let path = crate::services::attachment::attachment_file_path(&attachment);
        assert!(path.exists(), "the bytes have to be at {}", path.display());
        let stored = std::fs::read(&path).expect("readable");
        assert_ne!(
            stored, bytes,
            "a mesh file lands encrypted, streamed through the same call the CDN \
             upload path uses"
        );

        // The spool is empty and its plaintext is gone: a partial file is
        // unencrypted by construction, so leaving one behind after the transfer
        // finished would be the one thing in this app lying open on disk.
        assert!(runtime.files_in.is_empty());
        assert_eq!(
            std::fs::read_dir(&spool).expect("spool").count(),
            0,
            "no .part or .state may survive a completed transfer"
        );
    }

    #[tokio::test]
    async fn a_missing_chunk_is_re_requested_and_the_transfer_then_completes() {
        // Retransmission is *receiver*-requested, and it has to be: a broadcast
        // is not acknowledged, so the sender cannot know what landed. Without
        // this, one dropped chunk out of thousands means a file that never
        // arrives and nobody can say why.
        let (mut runtime, radio, state, timeline) = runtime_parts();
        isolate_spool(&mut runtime);
        writable_link(&state, "/mock/req", 512);
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");

        let bytes = body(3_000);
        let manifest = test_manifest([0x11; 8], &bytes);
        runtime.on_file_manifest(&manifest_packet(&sender, &identity, &manifest, true), &sender);
        for index in [0u32, 2] {
            runtime.on_file_chunk(
                &chunk_packet(&sender, &identity, &manifest, &bytes, index, true),
                &sender,
            );
        }

        // Nothing is asked for while chunks are still plausibly in flight.
        let before = radio.write_count();
        runtime.request_missing_chunks(now_secs()).await;
        assert_eq!(
            radio.write_count(),
            before,
            "a transfer that just made progress is not stalled"
        );

        // Once it has gone quiet, the receiver names exactly what it is missing.
        runtime
            .request_missing_chunks(now_secs() + CHUNK_REQUEST_QUIET_SECS)
            .await;
        let writes = radio.writes();
        let packet = codec::decode(&writes[before].1).expect("decodable");
        assert_eq!(packet.message_type(), Some(MessageType::FileChunkRequest));
        assert!(
            verify_packet(&packet, &runtime.identity.signing_public()),
            "an unsigned request is refused by the sender, because serving one \
             costs airtime and would otherwise be an amplification lever"
        );
        let request = FileChunkRequest::decode(&packet.payload).expect("decodes");
        assert_eq!(request.transfer_id, manifest.transfer_id);
        assert_eq!(request.indices, vec![1], "only the chunk that never came");

        // And the answer completes the file rather than restarting it.
        runtime.on_file_chunk(
            &chunk_packet(&sender, &identity, &manifest, &bytes, 1, true),
            &sender,
        );
        assert_eq!(
            timeline
                .history(&MeshConversation::Room, None, 10)
                .expect("history")
                .len(),
            1,
            "the recovered chunk finished the transfer"
        );
    }

    #[tokio::test]
    async fn an_unsigned_chunk_is_refused() {
        // The gate the design is most emphatic about. A manifest signed once says
        // nothing about the chunks that follow it, so accepting an unsigned chunk
        // would turn a transfer into an unauthenticated write channel into a file
        // that is about to be stored and shown under the manifest sender's name.
        let (mut runtime, _radio, state, timeline) = runtime_parts();
        isolate_spool(&mut runtime);
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");
        let mut events = runtime.events.subscribe();

        let bytes = body(2_048);
        let manifest = test_manifest([0x12; 8], &bytes);
        runtime.on_file_manifest(&manifest_packet(&sender, &identity, &manifest, true), &sender);

        // Chunk 0 unsigned, chunk 1 properly signed.
        runtime.on_file_chunk(
            &chunk_packet(&sender, &identity, &manifest, &bytes, 0, false),
            &sender,
        );
        assert_eq!(
            runtime
                .files_in
                .get(&manifest.transfer_id)
                .expect("still open")
                .received
                .present(),
            0,
            "an unsigned chunk contributes nothing"
        );

        runtime.on_file_chunk(
            &chunk_packet(&sender, &identity, &manifest, &bytes, 1, true),
            &sender,
        );
        assert_eq!(
            runtime
                .files_in
                .get(&manifest.transfer_id)
                .expect("still open")
                .received
                .present(),
            1,
            "and a signed one does, so the refusal is the signature and not the shape"
        );

        // The transfer is incomplete, so nothing surfaced and nothing was stored.
        assert!(events.try_recv().is_err());
        assert!(
            timeline
                .history(&MeshConversation::Room, None, 10)
                .expect("history")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_unsigned_manifest_never_opens_a_transfer() {
        // The same reasoning one packet earlier: a manifest's sender is a header
        // field the sender chose, so without a signature verifying against the
        // key their announce established, anyone in range opens a file under
        // anyone's name — and every chunk after it inherits that attribution.
        let (mut runtime, _radio, state, _timeline) = runtime_parts();
        isolate_spool(&mut runtime);
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");

        let bytes = body(2_048);
        let manifest = test_manifest([0x13; 8], &bytes);
        runtime.on_file_manifest(&manifest_packet(&sender, &identity, &manifest, false), &sender);
        assert!(runtime.files_in.is_empty());

        // Nor does a validly signed manifest from a peer that never announced:
        // there is no key to check it against, so it authenticates itself.
        let stranger = PeerId::from_str("cccccccccccccccc");
        let own = MeshIdentity::generate(&mut OsRng);
        runtime.on_file_manifest(&manifest_packet(&stranger, &own, &manifest, true), &stranger);
        assert!(runtime.files_in.is_empty());
    }

    #[tokio::test]
    async fn a_chunk_from_someone_other_than_the_manifests_sender_is_refused() {
        // Both peers are verified and both signatures are valid — the packet-level
        // gates all pass. What must not pass is a *different* verified peer
        // contributing bytes to a file somebody else opened.
        let (mut runtime, _radio, state, _timeline) = runtime_parts();
        isolate_spool(&mut runtime);
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");
        let other = PeerId::from_str("dddddddddddddddd");
        let other_identity = register_verified_peer(&state, &other, "someone else");

        let bytes = body(2_048);
        let manifest = test_manifest([0x14; 8], &bytes);
        runtime.on_file_manifest(&manifest_packet(&sender, &identity, &manifest, true), &sender);
        runtime.on_file_chunk(
            &chunk_packet(&other, &other_identity, &manifest, &bytes, 0, true),
            &other,
        );
        assert_eq!(
            runtime
                .files_in
                .get(&manifest.transfer_id)
                .expect("still open")
                .received
                .present(),
            0
        );
    }

    #[tokio::test]
    async fn a_digest_mismatch_discards_the_transfer_and_asks_for_nothing_more() {
        // A mismatch means some chunk that passed its packet signature still
        // produced the wrong file. There is no chunk to blame, so re-requesting
        // would fetch the same bytes and fail identically — forever, and
        // indistinguishably from a slow transfer.
        let (mut runtime, radio, state, timeline) = runtime_parts();
        let spool = isolate_spool(&mut runtime);
        writable_link(&state, "/mock/bad", 512);
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");
        let mut events = runtime.events.subscribe();

        let bytes = body(2_500);
        let mut manifest = test_manifest([0x15; 8], &bytes);
        // A manifest whose digest does not describe the bytes that follow it. The
        // manifest is still correctly signed, so nothing before the digest check
        // can catch this.
        manifest.sha256 = [0xAB; 32];
        runtime.on_file_manifest(&manifest_packet(&sender, &identity, &manifest, true), &sender);
        for index in 0..3 {
            runtime.on_file_chunk(
                &chunk_packet(&sender, &identity, &manifest, &bytes, index, true),
                &sender,
            );
        }

        // Discarded: no row, no attachment, no bytes.
        assert!(
            timeline
                .history(&MeshConversation::Room, None, 10)
                .expect("history")
                .is_empty(),
            "a file that failed its digest must not reach the timeline"
        );
        assert!(runtime.files_in.is_empty());
        assert_eq!(std::fs::read_dir(&spool).expect("spool").count(), 0);

        // Not retried: the tick asks for nothing, so there is no loop.
        let before = radio.write_count();
        runtime
            .request_missing_chunks(now_secs() + CHUNK_REQUEST_QUIET_SECS * 10)
            .await;
        assert_eq!(
            radio.write_count(),
            before,
            "a discarded transfer has nothing left to re-request"
        );

        // And the user is told, because minutes of radio produced nothing.
        let mut degraded = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, MeshEvent::Degraded(_)) {
                degraded = true;
            }
        }
        assert!(degraded, "a corrupt file is worth saying out loud");
    }

    #[tokio::test]
    async fn a_restart_resumes_a_transfer_instead_of_starting_it_again() {
        // The reason the bitmap is on disk. In memory, resume survives a link
        // drop and dies with the process — which is exactly when it is most
        // wanted: the app was killed, the file is still half there, and the only
        // record of which half was in a map.
        let (mut first, _radio, state, _timeline) = runtime_parts();
        let spool = isolate_spool(&mut first);
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");

        let bytes = body(5_000);
        let manifest = test_manifest([0x16; 8], &bytes);
        assert_eq!(manifest.chunk_count, 5);
        first.on_file_manifest(&manifest_packet(&sender, &identity, &manifest, true), &sender);
        for index in [0u32, 1, 2] {
            first.on_file_chunk(
                &chunk_packet(&sender, &identity, &manifest, &bytes, index, true),
                &sender,
            );
        }
        drop(first);

        // A new process over the same spool directory.
        let (mut restarted, _radio2, state2, timeline2) = runtime_parts();
        restarted.files_in = FileStreamSpool::new(spool);
        assert_eq!(restarted.files_in.restore(), 1, "the partial transfer is back");
        let identity = register_verified_peer(&state2, &sender, "wanderer");
        assert_eq!(
            restarted
                .files_in
                .get(&manifest.transfer_id)
                .expect("resumed")
                .received
                .present(),
            3,
            "three chunks are already here and must not be asked for again"
        );

        // The sender's next manifest resumes rather than truncating.
        restarted.on_file_manifest(
            &manifest_packet(&sender, &identity, &manifest, true),
            &sender,
        );
        assert_eq!(
            restarted
                .files_in
                .get(&manifest.transfer_id)
                .expect("still resumed")
                .received
                .present(),
            3
        );

        for index in [3u32, 4] {
            restarted.on_file_chunk(
                &chunk_packet(&sender, &identity, &manifest, &bytes, index, true),
                &sender,
            );
        }
        assert_eq!(
            timeline2
                .history(&MeshConversation::Room, None, 10)
                .expect("history")
                .len(),
            1,
            "two chunks finished a transfer that had already survived a restart"
        );
    }

    #[tokio::test]
    async fn a_transfer_that_completed_but_was_never_stored_is_finished_on_the_next_sweep() {
        // The one case where every byte is on disk and nothing will retransmit
        // them: the process ended between the last chunk and the store, or the
        // store failed. Losing the file then would waste minutes of radio that
        // nothing spends again.
        let (mut first, _radio, state, _timeline) = runtime_parts();
        let spool = isolate_spool(&mut first);
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");

        let bytes = body(2_048);
        let manifest = test_manifest([0x1C; 8], &bytes);
        first.on_file_manifest(&manifest_packet(&sender, &identity, &manifest, true), &sender);
        // Only the first chunk goes through the runtime; the second is written
        // straight into the spool so the completion handler never runs, which is
        // what a kill between the two looks like on disk.
        first.on_file_chunk(
            &chunk_packet(&sender, &identity, &manifest, &bytes, 0, true),
            &sender,
        );
        assert_eq!(
            first
                .files_in
                .accept_chunk(&manifest.transfer_id, 1, &bytes[1024..], &sender, now_secs())
                .expect("accepted"),
            ChunkOutcome::Complete
        );
        drop(first);

        let (mut restarted, _radio2, state2, timeline2) = runtime_parts();
        restarted.files_in = FileStreamSpool::new(spool);
        assert_eq!(restarted.files_in.restore(), 1);
        register_verified_peer(&state2, &sender, "wanderer");
        assert_eq!(restarted.files_in.completed(), vec![manifest.transfer_id]);

        restarted.cleanup(now_millis(), now_secs());
        assert_eq!(
            timeline2
                .history(&MeshConversation::Room, None, 10)
                .expect("history")
                .len(),
            1,
            "the sweep stored the file rather than leaving it to expire"
        );
        assert!(restarted.files_in.is_empty());
    }

    #[tokio::test]
    async fn a_chunk_waits_for_the_previous_one_to_clear_the_air() {
        // The pacing that makes a large transfer possible at all. Chunk
        // generation used to be gated only by the loop tick, on the assumption
        // that the outbound write queue would absorb the difference and the radio
        // would set the real rate. That holds only where the radio reports
        // congestion — Android's bridge does. BlueZ sending notifications does
        // not: measured over 36,800 frames, `try_send` never once refused, so
        // every frame was accepted and the excess was dropped below the socket.
        // The receiver saw 2 to 9 fragments of each 70-fragment chunk and never
        // completed one, while asking for them twice a second.
        //
        // So a chunk is held until the previous one has had time on the air.
        let (mut runtime, radio, state, _timeline) = runtime_parts();
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("paced.bin");
        let bytes = body(4_000);
        std::fs::write(&source, &bytes).expect("write");
        writable_link(&state, "/mock/paced", 4096);

        let manifest = test_manifest([0x2C; 8], &bytes);
        runtime
            .start_streamed_file("MSG-PACE", manifest.clone(), source)
            .await;

        let at = 5_000_000u64;
        runtime.pump_streamed_files(now_secs(), at).await;
        let after_first = radio.write_count();
        assert!(after_first > 0, "the first chunk goes out immediately");

        // Same millisecond, and every millisecond up to the interval: nothing
        // more is generated, however often the loop runs.
        for advance in [0, 1, 10] {
            runtime.pump_streamed_files(now_secs(), at + advance).await;
            assert_eq!(
                radio.write_count(),
                after_first,
                "a chunk {advance} ms after the last one would be dropped, not sent"
            );
        }

        // Once the air time has elapsed, the next chunk goes.
        runtime
            .pump_streamed_files(now_secs(), at + CHUNK_AIR_MS_FOR_TESTS)
            .await;
        assert!(
            radio.write_count() > after_first,
            "pacing delays a chunk, it does not cancel it"
        );
    }

    #[tokio::test]
    async fn a_streamed_send_puts_a_manifest_out_first_then_paces_chunks_by_tick() {
        // The send side never reads the file whole, and never emits it in one
        // pass: the loop that emits chunks is the same loop that services link
        // events, so a 16 MiB transfer done in one go would leave the mesh deaf
        // for minutes.
        let (mut runtime, radio, state, timeline) = runtime_parts();
        let sender_dir = tempfile::tempdir().expect("temp dir");
        let source = sender_dir.path().join("outgoing.bin");
        let bytes = body(3_000);
        std::fs::write(&source, &bytes).expect("write");
        writable_link(&state, "/mock/send", 4096);
        let mut events = runtime.events.subscribe();

        let manifest = test_manifest([0x17; 8], &bytes);
        runtime
            .start_streamed_file("MSG-S1", manifest.clone(), source.clone())
            .await;

        // The manifest goes first, signed, as a broadcast.
        let writes = radio.writes();
        assert_eq!(writes.len(), 1, "one packet, and it is not a chunk");
        let packet = codec::decode(&writes[0].1).expect("decodable");
        assert_eq!(packet.message_type(), Some(MessageType::FileManifest));
        assert!(packet.recipient_id.is_none());
        assert!(verify_packet(&packet, &runtime.identity.signing_public()));
        assert_eq!(
            FileManifest::decode(&packet.payload).expect("decodes"),
            manifest
        );
        assert!(
            state.with(|inner| inner.self_broadcasts.contains(&packet)),
            "the echo of our own manifest has to be recognisable"
        );

        // The sender's own copy exists immediately: without it the row names an
        // attachment that does not exist and the sender is the one person on the
        // mesh who cannot open what they just sent.
        match events.try_recv() {
            Ok(MeshEvent::Message(message)) => {
                assert!(message.is_local);
                assert_eq!(mapping::attachments_of(&message.message).len(), 1);
            }
            other => panic!("expected the local mirror, got {other:?}"),
        }
        assert_eq!(
            timeline
                .history(&MeshConversation::Room, None, 10)
                .expect("history")
                .len(),
            1
        );

        // Then one chunk at a time, in order, each signed in its own right —
        // and each only once the previous one has had time on the air. The clock
        // is advanced explicitly rather than slept through: the interval is the
        // contract, so the test states it instead of waiting for it.
        let mut seen = Vec::new();
        let mut rebuilt = Vec::new();
        let mut now_ms = 1_000_000u64;
        for _ in 0..manifest.chunk_count {
            let before = radio.write_count();
            runtime.pump_streamed_files(now_secs(), now_ms).await;
            now_ms += CHUNK_AIR_MS_FOR_TESTS;
            let writes = radio.writes();
            assert_eq!(
                writes.len(),
                before + 1,
                "one chunk per pacing interval, so the loop keeps servicing the radio"
            );
            let packet = codec::decode(&writes[before].1).expect("decodable");
            assert_eq!(packet.message_type(), Some(MessageType::FileChunk));
            assert!(
                verify_packet(&packet, &runtime.identity.signing_public()),
                "the receiver refuses an unsigned chunk"
            );
            let chunk = FileChunk::decode(&packet.payload).expect("decodes");
            assert_eq!(chunk.transfer_id, manifest.transfer_id);
            seen.push(chunk.index);
            rebuilt.extend_from_slice(&chunk.bytes);
        }
        assert_eq!(seen, (0..manifest.chunk_count).collect::<Vec<_>>());
        assert_eq!(rebuilt, bytes, "the chunks tile the file exactly");

        // Drained: nothing more goes out unprompted, because there is no
        // acknowledgement to wait for.
        let before = radio.write_count();
        runtime.pump_streamed_files(now_secs(), u64::MAX).await;
        assert_eq!(radio.write_count(), before);
    }

    #[tokio::test]
    async fn a_chunk_request_from_a_receiver_is_served_before_the_first_pass() {
        // The other half of receiver-driven retransmission. The peer that asked is
        // *blocked* on that chunk; the first pass is merely unfinished.
        let (mut runtime, radio, state, _timeline) = runtime_parts();
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("outgoing2.bin");
        let bytes = body(4_000);
        std::fs::write(&source, &bytes).expect("write");
        writable_link(&state, "/mock/serve", 4096);
        let asker = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let asker_identity = register_verified_peer(&state, &asker, "wanderer");

        let manifest = test_manifest([0x18; 8], &bytes);
        runtime
            .start_streamed_file("MSG-S2", manifest.clone(), source)
            .await;
        // Two passes, with the clock moved on between them so the second is not
        // held back by the first chunk's pacing interval.
        runtime.pump_streamed_files(now_secs(), 1_000_000).await;
        runtime
            .pump_streamed_files(now_secs(), 1_000_000 + CHUNK_AIR_MS_FOR_TESTS)
            .await;

        // A signed request for chunk 0, which has already been sent once.
        let request = FileChunkRequest::new(manifest.transfer_id, vec![0]).expect("valid");
        let packet = signed(
            &asker,
            &asker_identity,
            MessageType::FileChunkRequest,
            request.encode().expect("encodes"),
            true,
        );
        runtime.on_file_chunk_request(&packet, &asker);

        let before = radio.write_count();
        // Named rather than indexed. Tests install no tracing subscriber, so a
        // transfer that never started logs nothing anywhere, and indexing
        // straight into the write log turns that into an out-of-bounds panic
        // that says only "len is 0" — which is every possible cause at once.
        assert!(
            before > 0,
            "precondition: the manifest and the first pass should already be on \
             the air after two pumps, but nothing was written (transfer still \
             live: {})",
            !runtime.files_out.is_empty()
        );
        runtime.pump_streamed_files(now_secs(), u64::MAX).await;
        let writes = radio.writes();
        let served = writes
            .get(before)
            .unwrap_or_else(|| panic!("the re-requested chunk should have been written"));
        let sent = FileChunk::decode(
            &codec::decode(&served.1).expect("decodable").payload,
        )
        .expect("decodes");
        assert_eq!(
            sent.index, 0,
            "the re-request jumps the queue ahead of the unfinished first pass"
        );

        // An unsigned request is ignored, so a stranger cannot make us spend
        // airtime re-sending a file.
        let unsigned = signed(
            &asker,
            &asker_identity,
            MessageType::FileChunkRequest,
            request.encode().expect("encodes"),
            false,
        );
        runtime.on_file_chunk_request(&unsigned, &asker);
        let before = radio.write_count();
        // Named rather than indexed. Tests install no tracing subscriber, so a
        // transfer that never started logs nothing anywhere, and indexing
        // straight into the write log turns that into an out-of-bounds panic
        // that says only "len is 0" — which is every possible cause at once.
        assert!(
            before > 0,
            "precondition: the manifest and the first pass should already be on \
             the air after two pumps, but nothing was written (transfer still \
             live: {})",
            !runtime.files_out.is_empty()
        );
        runtime.pump_streamed_files(now_secs(), u64::MAX).await;
        let writes = radio.writes();
        let served = writes
            .get(before)
            .unwrap_or_else(|| panic!("the re-requested chunk should have been written"));
        let sent = FileChunk::decode(
            &codec::decode(&served.1).expect("decodable").payload,
        )
        .expect("decodes");
        assert_eq!(
            sent.index, 2,
            "the pass carried on; the unsigned request bought nothing"
        );
    }

    #[tokio::test]
    async fn our_own_manifest_and_chunks_coming_back_are_ignored() {
        // These types have no gossip-sync bit, so a self-authored copy can only be
        // our own flood echoing back — there is nothing to recover and the local
        // copy was stored when the transfer started. Accepting one would spool our
        // own file into a second attachment.
        let (mut runtime, _radio, state, timeline) = runtime_parts();
        isolate_spool(&mut runtime);
        writable_link(&state, "/mock/echo", 4096);
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("mine.bin");
        let bytes = body(2_048);
        std::fs::write(&source, &bytes).expect("write");

        let manifest = test_manifest([0x19; 8], &bytes);
        runtime
            .start_streamed_file("MSG-S3", manifest.clone(), source)
            .await;
        let rows_before = timeline
            .history(&MeshConversation::Room, None, 10)
            .expect("history")
            .len();

        let local = runtime.identity.peer_id();
        let own_manifest = manifest_packet(&local, &runtime.identity.clone(), &manifest, true);
        runtime.on_file_manifest(&own_manifest, &local);
        assert!(
            runtime.files_in.is_empty(),
            "our own manifest must not open a receive spool for a file we are sending"
        );

        let own_chunk = chunk_packet(&local, &runtime.identity.clone(), &manifest, &bytes, 0, true);
        runtime.on_file_chunk(&own_chunk, &local);
        assert!(runtime.files_in.is_empty());
        assert_eq!(
            timeline
                .history(&MeshConversation::Room, None, 10)
                .expect("history")
                .len(),
            rows_before,
            "and no second row for a file we already have"
        );
    }

    #[tokio::test]
    async fn a_chunked_transfer_is_refused_when_it_is_addressed_to_us_alone() {
        // Every gate P8 established still applies. A directed cleartext file has
        // no business in a client whose every DM is Noise-sealed, and accepting
        // one would teach the UI that private traffic can arrive in the clear.
        let (mut runtime, _radio, state, _timeline) = runtime_parts();
        isolate_spool(&mut runtime);
        let sender = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &sender, "wanderer");
        let local = runtime.identity.peer_id();

        let bytes = body(2_048);
        let manifest = test_manifest([0x1A; 8], &bytes);
        let mut packet = BitchatPacket::new(
            MessageType::FileManifest,
            sender.to_routing_bytes().expect("routing bytes"),
            local.to_routing_bytes(),
            now_millis(),
            manifest.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        packet.signature = Some(sign_packet(&identity, &packet).expect("signs"));
        runtime.on_file_manifest(&packet, &sender);
        assert!(runtime.files_in.is_empty());
    }

    #[tokio::test]
    async fn a_small_file_still_rides_one_packet_and_a_large_one_streams() {
        // The interop guarantee. Real bitchat speaks `FileTransfer` and knows
        // nothing about 0x2A–0x2C, so everything small must keep going out the way
        // P8 sent it. Chunking is an extension for the sizes that path cannot
        // carry, selected by size — not a replacement.
        let (mut runtime, radio, state, _timeline) = runtime_parts();
        writable_link(&state, "/mock/fork", 4096);

        runtime.broadcast_file("MSG-SMALL", &sample_file()).await;
        let writes = radio.writes();
        assert_eq!(
            codec::decode(&writes[0].1).expect("decodable").message_type(),
            Some(MessageType::FileTransfer),
            "a small file is still a single bitchat file packet"
        );

        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("big.bin");
        let bytes = body(MAX_SINGLE_PACKET_FILE_BYTES + 1);
        std::fs::write(&source, &bytes).expect("write");
        let before = radio.write_count();
        runtime
            .start_streamed_file("MSG-BIG", test_manifest([0x1B; 8], &bytes), source)
            .await;
        let writes = radio.writes();
        assert_eq!(
            codec::decode(&writes[before].1)
                .expect("decodable")
                .message_type(),
            Some(MessageType::FileManifest),
            "and anything above one packet's worth announces itself instead"
        );
    }
}
