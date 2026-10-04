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

use std::cmp::Reverse;
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
use chatter_mesh_link::announce::{
    AnnounceDistrust, AnnounceLinkContext, AnnouncePreflightConfig, evaluate_announce,
};
use chatter_mesh_link::backpressure::{
    InboundAppend, InboundAppendMetadata, InboundChunk, InboundLimits,
    PENDING_WRITE_BUFFER_CAP_BYTES, should_buffer, WriteEnqueue,
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
use chatter_mesh_store::storage::{Storage, StorageKey};
use chatter_mesh_store::outbox::Sealer;
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
use super::mapping::{self, InboundMessage, MeshConversation, outbound_message};
use super::router::{RouterCommand, RouterHandle};
use super::state::{LinkStateInner, MeshState, NostrState, link_candidates_locked, now_millis, now_secs};
use super::timeline::MeshEvents;
use super::transport::BleCommand;

/// How often the loop wakes to run the parts that nothing else can schedule.
const TICK: Duration = Duration::from_millis(500);

/// How long between frames on one link — the transmit rate, in one number.
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
const WRITE_PACING_DELAY: Duration = Duration::from_millis(20);

/// How often to report achieved frame throughput while there is traffic.
const THROUGHPUT_REPORT_INTERVAL_MS: u64 = 10_000;

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
    storage: Arc<dyn Storage>,
    sealer: Arc<dyn Sealer>,
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
    couriers: CourierStore<Arc<dyn Storage>>,
    guard: IngressGuard,
    /// Chunked transfers being received. Owned by the loop rather than by
    /// [`MeshState`], because every method on it touches disk and `MeshState` is
    /// behind a `parking_lot::Mutex` that must never be held across IO.
    files_in: FileStreamSpool,
    /// Chunked transfers being sent, keyed by transfer id. Each holds a path and
    /// a cursor, never the file's bytes.
    files_out: std::collections::BTreeMap<TransferId, OutboundTransfer>,
    /// Frames actually put on the air since the last throughput report.
    ///
    /// The one number this feature turns on and nothing was measuring. Chunk
    /// pacing allows one frame per `WRITE_PACING_DELAY`, so 50 a second — but the
    /// drain runs from a `select!` arm that competes with radio events and the
    /// tick, so what it *achieves* is a different question. Derived from the
    /// receiver, a 2 MB transfer came to about 13 frames a second, and whether
    /// that is the link's ceiling or the loop's was unanswerable from the logs.
    frames_written: std::sync::atomic::AtomicU64,
    /// When the last throughput report went out.
    last_throughput_report_ms: u64,
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
        storage: Arc<dyn Storage>,
        sealer: Arc<dyn Sealer>,
    ) -> Self {
        let now = now_secs();
        let archive = GossipMessageArchive::new(Arc::new(storage.clone()));
        let gossip = GossipSyncManager::new(
            &identity.peer_id(),
            SyncConfig::default(),
            Some(archive),
            now,
        );
        let couriers = CourierStore::new(storage.clone());
        Self {
            storage,
            sealer,
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
            frames_written: std::sync::atomic::AtomicU64::new(0),
            last_throughput_report_ms: 0,
        }
    }

    fn restore_pins(&self) -> Result<(), String> {
        if let Some(sealed) = self.storage.load(StorageKey::PeerPins).map_err(|e| e.to_string())? {
            let bytes = self.sealer.open(&sealed).map_err(|e| e.to_string())?;
            self.state.with(|state| state.peers.restore_pins(&bytes))
                .map_err(|_| "malformed peer pins".to_string())?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn save_pins(&self) -> Result<(), String> {
        let bytes = self.state.with(|state| state.peers.pins_to_bytes());
        let sealed = self.sealer.seal(&bytes).map_err(|e| e.to_string())?;
        self.storage.save(StorageKey::PeerPins, &sealed).map_err(|e| e.to_string())
    }

    /// Start the radio and run until the command channel or the radio closes.
    ///
    /// The receiver is a parameter rather than a field so that the `select!`
    /// branch reading it does not borrow the whole runtime, which every handler
    /// needs mutably.
    pub async fn run(mut self, mut commands: UnboundedReceiver<BleCommand>) {
        if let Err(error) = self.restore_pins() {
            self.degraded(format!("Peer identity pins could not be restored: {error}"));
            return;
        }
        // Before `start`, because an advertiser already running will not pick it
        // up. This is what lets peers recognise us across a private address
        // rotation instead of spending a connection to find out who we are.
        if let Some(peer) = self.identity.peer_id().to_routing_bytes() {
            self.radio.set_local_peer(peer);
        }
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
                _ = tokio::time::sleep(WRITE_PACING_DELAY), if backlogged => {
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
            LinkEvent::Discovered { address, rssi, peer } => {
                self.on_discovered(&address, rssi, peer).await
            }
            LinkEvent::Connected {
                link,
                role,
                max_write,
            } => self.on_connected(link, role, max_write).await,
            LinkEvent::Disconnected { link } => self.on_disconnected(&link).await,
            LinkEvent::Data { link, bytes } => self.on_data(&link, bytes).await,
            LinkEvent::PeripheralRoleEnded => {
                // Not fatal and not recoverable from here: outbound links still
                // work, so the mesh is half alive, and that is precisely the state
                // worth saying out loud. A peer that dials us now gets nowhere and
                // has no way to know why.
                tracing::warn!(
                    target: "mesh::ble",
                    "this device can no longer be connected to; inbound links have \
                     stopped being served, and only outbound ones will form now"
                );
                self.degraded(
                    "Other devices can no longer connect to this one over Bluetooth. \
                     Restarting the app should restore it."
                        .to_string(),
                );
            }
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

    /// A device advertising the mesh service came into range.
    ///
    /// `peer` is the identity from the advert's service data, when it carried
    /// one. Recognising it here is the whole point: a private address rotates,
    /// and without the id the only way to learn that an unfamiliar address is a
    /// peer we already hold is to connect to it and read its announce. That
    /// connection is not free — it displaces the ACL we already had, and the
    /// disconnect makes the peer's advertiser restart under yet another address.
    /// Measured before this existed: nineteen addresses dialled in four minutes,
    /// every one of them the same phone.
    ///
    /// `None` means the scan response has not been seen yet, not that the device
    /// is a stranger, so it falls through to the ordinary scheduler path.
    ///
    /// Deliberately counts links of *either* role, unlike
    /// [`redundant_central_links_locked`], which prunes only central ones. The two
    /// asymmetries point the same way: a peer already reachable needs no second
    /// path, so an inbound link is reason enough not to dial out — but a link the
    /// peer dialled to us cost us no slot and is not ours to hang up. Measured
    /// consequence: where both devices are running, the phone usually dials first
    /// and the desktop then never dials at all, which is what a 25 minute run on a
    /// single peripheral link with zero warnings looks like.
    ///
    /// This does nothing at a cold start, when there are no links to recognise a
    /// peer by. Over five restarts, two produced a second link before the first
    /// announce arrived; the prune resolved each within seconds and no frame was
    /// lost. That residual is the price of identity arriving with the first
    /// announce rather than before it.
    async fn on_discovered(&mut self, address: &str, rssi: i16, peer: Option<[u8; 8]>) {
        let now = now_millis();

        if let Some(peer) = peer {
            let peer = PeerId::from_bytes(&peer);
            let held = self.state.with(|state| {
                state
                    .links
                    .links()
                    .any(|(_, link)| link.peer.as_ref() == Some(&peer))
            });
            if held {
                tracing::trace!(
                    target: "mesh::ble",
                    %peer,
                    %address,
                    "another address for a peer we already hold; not dialling"
                );
                return;
            }
        }

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
                // A timeout is worth saying out loud; an outright failure is not.
                // A peer that walked out of range is ordinary churn and would
                // drown the log, but twenty seconds spent half-connected is the
                // symptom of the unexplained link death, and the error now names
                // the stage that hung. Debug is invisible on Android, which is
                // where the question gets asked.
                if timed_out {
                    tracing::warn!(target: "mesh::ble", %address, %error, "connect timed out");
                } else {
                    tracing::debug!(target: "mesh::ble", %address, %error, "connect failed");
                }
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

        // Both discard paths report the numbers that tell them apart, and both at
        // warn. The metadata was already being assembled and then thrown away with
        // `..`, which is how a peer whose frames never land looks identical to a
        // peer that never sent any. `received=56 expected=70` is what made the
        // fragment-expiry bug findable; the same applies here.
        match appended {
            InboundAppend::Decoded { packet, metadata } => {
                report_stall_discard(link, &metadata);
                self.on_packet(link, *packet).await
            }
            InboundAppend::Waiting { metadata } => report_stall_discard(link, &metadata),
            InboundAppend::Oversized { metadata } => {
                tracing::warn!(
                    target: "mesh::ble",
                    link = %link,
                    needed_bytes = metadata.accumulated_bytes,
                    cap_bytes = InboundLimits::default().cap_bytes,
                    appended_bytes = metadata.appended_bytes,
                    packet_type = ?metadata.packet_type,
                    "inbound frame over the cap; the link's buffer was dropped"
                );
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

        let retainable = self.dispatch(link, &packet, &claimed, now_s).await;

        // Refused identity-control packets must not be propagated to peers.
        if !retainable { return; }
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
    ///
    /// Returns whether the packet may be retained and re-gossiped. Only an
    /// announce or unverified Leave says no — see [`Self::handle_typed`].
    async fn dispatch(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
        now_s: u64,
    ) -> bool {
        if packet.message_type() != Some(MessageType::Fragment) {
            return self.handle_typed(link, packet, claimed).await;
        }

        let reassembled = self
            .state
            .with(|state| state.fragments.accept(packet, now_s));
        let Ok(Some(bytes)) = reassembled else {
            return true;
        };
        let Ok(inner) = codec::decode(&bytes) else {
            return true;
        };
        if inner.message_type() == Some(MessageType::Fragment) {
            // A fragment carrying a fragment is not something the fragmenter
            // produces; it is someone trying to make us loop.
            return true;
        }
        self.handle_typed(link, &inner, claimed).await
    }

    /// Handle one decoded packet, and say whether the gossip store may keep it.
    ///
    /// The flag exists for announces alone. An announce we refused is an identity
    /// forgery, and retaining it would make this device re-serve that forgery to
    /// every peer that syncs with us; worse, `on_public_packet_seen` reads a
    /// *stale* announce as "this peer left" and purges the store's state for the
    /// claimed sender, so an unauthenticated old announce is a remote wipe of a
    /// real peer's gossip state. Genuine staleness is still collected by
    /// `cleanup_stale_announcements_if_needed` on the maintenance tick, which
    /// needs no packet from anyone to do its job.
    async fn handle_typed(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) -> bool {
        match packet.message_type() {
            Some(MessageType::Announce) => return self.on_announce(link, packet, claimed).await,
            Some(MessageType::Message) => self.on_public_message(packet, claimed),
            Some(MessageType::Leave) => {
                let verified = self.state.with(|state| {
                    state.peers.pinned(claimed)
                        .and_then(|pin| <[u8; 32]>::try_from(pin.signing_public_key.as_slice()).ok())
                        .is_some_and(|key| verify_packet(packet, &key))
                });
                if !verified { return false; }
                self.state.with(|state| {
                    state.peers.remove(claimed);
                    state.sessions.forget(claimed);
                });
                self.gossip.remove_announcement_for_peer(claimed);
                self.emit_peers();
            }
            Some(MessageType::NoiseHandshake) => self.on_handshake(link, packet, claimed).await,
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
        true
    }

    /// Returns whether the announce was accepted, which is also the answer to
    /// "may the gossip store keep this packet".
    async fn on_announce(
        &mut self,
        link: &LinkId,
        packet: &BitchatPacket,
        claimed: &PeerId,
    ) -> bool {
        let local = self.identity.peer_id();
        let now_ms = now_millis();
        let now_s = now_secs();

        // What the trust decision needs from our own tables, read before it runs.
        //
        // `presence` and `peer_for` are facts about *our* connection state and so
        // are not forgeable; the recorded Noise key is what a key substitution
        // would be trying to overwrite, which is why the policy wants it before
        // it looks at the signature.
        let (presence, link_bound_elsewhere, existing_noise_key, existing_signing_key) =
            self.state.with(|state| {
                let record = state.peers.info(claimed);
                let pin = state.peers.pinned(claimed);
                (
                    state.links.presence(claimed),
                    state
                        .links
                        .peer_for(link)
                        .is_some_and(|bound| bound != claimed),
                    pin.map(|p| p.noise_public_key.clone()).or_else(|| record.and_then(|r| r.noise_public_key.clone())),
                    pin.map(|p| p.signing_public_key.clone()).or_else(|| record.and_then(|r| r.signing_public_key.clone())),
                )
            });

        // The whole inbound announce policy, in one call: decode, derive the peer
        // ID from the announced Noise key and refuse a sender that does not match
        // it, refuse our own announce coming back, refuse a stale one, refuse a
        // swapped key, and only then spend a signature verification.
        //
        // The derivation check is the load-bearing one and it was missing. A
        // packet signature verified against the signing key the announcement
        // itself carries proves only that whoever wrote the announce holds that
        // key — which every attacker does. The mesh ID *is*
        // `SHA256(noise static key)[..8]` (`MeshIdentity::peer_id`), so binding
        // the sender ID to the key is free and is what stops a device in radio
        // range announcing any identity it likes.
        let context = AnnounceLinkContext {
            direct_announce_ttl: DIRECT_ANNOUNCE_TTL,
            has_peripheral_link: presence.as_peripheral,
            has_central_link: presence.as_central,
            link_bound_to_other_peer: link_bound_elsewhere,
            existing_noise_public_key: existing_noise_key.as_deref(),
            // §98: the signing key is pinned the same way. The registry has
            // recorded it since the first announce; nothing compared it, so
            // "Verified" meant only "signed by the key this packet carries",
            // which every attacker in radio range also holds.
            existing_signing_public_key: existing_signing_key.as_deref(),
        };
        let accepted = match evaluate_announce(
            packet,
            claimed,
            &local,
            now_ms,
            &context,
            &AnnouncePreflightConfig::default(),
            |packet, signing_key| {
                <[u8; 32]>::try_from(signing_key).is_ok_and(|key| verify_packet(packet, &key))
            },
        ) {
            Ok(accepted) => accepted,
            Err(rejection) => {
                tracing::debug!(
                    target: "mesh::ble",
                    peer = %claimed,
                    ?rejection,
                    "announce rejected before the trust check"
                );
                return false;
            }
        };

        // bitchat drops an unverified announce outright rather than recording the
        // peer unverified (`BLEAnnounceHandler.swift:167`, `MessageHandler.kt:259`).
        // Recording it would be worse than useless: an unverified announce carries
        // an attacker-chosen noise key and nickname, so accepting it is exactly the
        // identity-poisoning their handler exists to prevent.
        if !accepted.should_upsert_peer() {
            if matches!(accepted.trust.distrust(), Some(AnnounceDistrust::KeyMismatch | AnnounceDistrust::SigningKeyMismatch)) {
                self.events.send(MeshEvent::IdentityKeyChanged { peer: claimed.clone() });
            }
            tracing::debug!(
                target: "mesh::ble",
                peer = %claimed,
                distrust = ?accepted.trust.distrust(),
                "unverified announce ignored"
            );
            return false;
        }

        if !self.state.with(|state| state.peers.can_pin(claimed)) {
            self.degraded("Peer identity storage is full; new peers are refused.".to_string());
            return false;
        }
        let announcement = accepted.announcement.clone();
        // Commit the pin before exposing the peer as trusted. A failed write
        // leaves no accepted identity that would silently lose its pin on restart.
        let persisted = self.state.with(|state| -> Result<(), String> {
            if state.peers.pinned(claimed).is_some() { return Ok(()); }
            let mut candidate = state.peers.clone();
            candidate.upsert_verified_announce(claimed, &announcement, false, now_ms);
            let sealed = self.sealer.seal(&candidate.pins_to_bytes()).map_err(|e| e.to_string())?;
            self.storage.save(StorageKey::PeerPins, &sealed).map_err(|e| e.to_string())
        });
        if let Err(error) = persisted {
            self.degraded(format!("Peer identity pin could not be saved: {error}"));
            return false;
        }
        let direct = VerifiedDirectAnnounce::from_packet(
            claimed.clone(),
            packet,
            accepted.is_verified(),
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
            let connected = accepted.connected_flag().unwrap_or(false);

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
            // Per-link proof only: a Noise handshake completed, or traffic
            // decrypted, on *this* link from this peer. A peer-level session is
            // not enough — it outlives its link, and a replayed announce can
            // rebind an attacker's link to the victim's ID.
            let authenticated = state.links.is_noise_authenticated(link, claimed);
            (
                update.is_new_peer || update.was_disconnected,
                authenticated,
                update.is_new_peer,
                update.was_disconnected,
                state.links.peer_for(link).cloned(),
            )
        });

        // Hang up any duplicate ACL to this peer. The announce is the only thing
        // that reveals identity — the address rotated out from under us — so this
        // is the first moment the duplicate can be recognised as one.
        let redundant = self.state.with(|state| {
            let doomed = redundant_central_links_locked(state, claimed);
            // Stop the address being dialled straight back. Closing the link on its
            // own is a loop: the peer is still advertising that address, discovery
            // reports it as a device we do not hold, and we dial it again — measured
            // as a prune at 03:43:34 followed by a redial of the same address 8s
            // later. The address, not the peer, is what discovery is keyed on.
            for link in &doomed {
                if let Some(address) = state.address_of.get(link).cloned() {
                    state.scheduler.note_duplicate_peer(&address, now_ms);
                }
            }
            doomed
        });
        for link in redundant {
            tracing::info!(
                target: "mesh::ble",
                peer = %claimed,
                %link,
                "closing a duplicate link to a peer we already hold"
            );
            let _ = self.radio.disconnect(&link).await;
        }

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
        if let Ok(key) = <[u8; 32]>::try_from(announcement.noise_public_key.as_slice()) {
            if ingress.releases_mail() {
                self.hand_over_courier_mail(claimed, &key, now_s, ingress).await;
            } else if direct.is_some()
                && self.couriers.carries_for(&key, now_s)
                && !self.state.with(|state| state.sessions.is_handshaking(claimed))
            {
                // We carry mail for a peer that is here, direct, but has not
                // proved its key on this link. A handshake is that proof; once
                // it completes, the link is marked and its next announce
                // releases the mail.
                self.start_handshake(claimed).await;
            }
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

        true
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
            Ok(ManifestOutcome::AlreadyComplete) => {
                tracing::debug!(
                    target: "mesh::ble",
                    peer = %sender,
                    "a manifest arrived for a transfer already finished; ignored"
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
        // Both refusals below were silent, and a chunk refused silently is
        // indistinguishable from one that never arrived — which is precisely the
        // case that needs telling apart when a transfer stops progressing while
        // the sender insists it is serving chunks. Warn rather than debug: the
        // Android build logs at info, so debug here would still say nothing on the
        // device where it matters most.
        let Ok(chunk) = FileChunk::decode(&packet.payload) else {
            tracing::warn!(
                target: "mesh::ble",
                peer = %claimed,
                bytes = packet.payload.len(),
                "a file chunk arrived that could not be decoded"
            );
            return;
        };
        let Some((sender, nickname, _verified)) =
            self.authenticate_stream_packet(packet, claimed)
        else {
            tracing::warn!(
                target: "mesh::ble",
                peer = %claimed,
                transfer = %hex_encode(&chunk.transfer_id),
                index = chunk.index,
                "a file chunk was refused: unsigned, or from a peer with no \
                 announce to check it against"
            );
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
                self.files_in.finish(transfer_id);
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
        self.files_in.finish(transfer_id);

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
        let accepted = transfer.note_requested(&request.indices);
        // `asked` and `queued` separately, because they can differ: an index the
        // manifest does not cover is dropped here, and logging only the request
        // size would report a retransmission that never happened.
        tracing::info!(
            target: "mesh::ble",
            peer = %sender,
            transfer = %hex_encode(&request.transfer_id),
            asked = request.indices.len(),
            queued = accepted,
            "retransmitting chunks a receiver asked for"
        );
    }

    async fn on_handshake(&mut self, link: &LinkId, packet: &BitchatPacket, claimed: &PeerId) {
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
                // Warn: no session means no direct message with this peer will
                // ever open, and nothing retries on a schedule. Silence here reads
                // as "they never wrote", which is the opposite of what happened.
                tracing::warn!(target: "mesh::ble", peer = %claimed, %error, "handshake failed");
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
            // The handshake authenticated `claimed`'s static key (the session
            // manager refuses one that does not derive the ID), and it finished
            // on this link: that is the per-link proof courier handover needs.
            self.state
                .with(|state| state.links.mark_noise_authenticated(link, claimed));
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
                // Warn: this was a message, and over the mesh there is no server
                // copy to fetch again. The plaintext is gone.
                tracing::warn!(target: "mesh::ble", peer = %claimed, %error, "decrypt failed");
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

        // Not ours: carry it, if the depositor is signature-verified.
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
            // signature the pool is a free amplifier. Said at debug rather than
            // warn only because an attacker controls how often it happens — but
            // said, because a silent return here is indistinguishable from mail
            // that was never sent.
            tracing::debug!(
                target: "mesh::ble",
                peer = %claimed,
                "courier deposit from an unverified peer refused"
            );
            return;
        }
        if let Err(error) = self.couriers.deposit(
            &envelope,
            &depositor_key,
            now_secs(),
        ) {
            // Warn: the depositor believes we are carrying this for them, and we
            // are not. Nothing tells them otherwise.
            tracing::warn!(target: "mesh::ble", %error, "courier deposit refused");
        }
    }

    async fn on_request_sync(&mut self, packet: &BitchatPacket, claimed: &PeerId) {
        let Ok(request) = RequestSyncPacket::decode(&packet.payload) else {
            return;
        };

        // A sync response can replay the entire store, and the filter does not
        // constrain that: a GCS filter that is empty or undecodable matches
        // nothing, so `handle_request_sync` reads it as "the requester holds none
        // of it" and answers with everything. Content is not the exposure —
        // public messages are already public and private ones are sealed — but
        // announces and prekey bundles are *keyed by their owner's identity*, and
        // handing the set of them to a stranger hands over the list of everyone
        // this device has met, plus the material to start a session with each.
        //
        // So a request is only answered for a peer that has proved an identity to
        // us: a verified announce, which since it must derive the sender ID from
        // the Noise key it carries is a real binding rather than a claim. Honest
        // peers announce on connect before they ask, so this costs a real device
        // nothing.
        let known = self.state.with(|state| {
            state
                .peers
                .info(claimed)
                .is_some_and(|record| record.verified_nickname)
        });
        if !known {
            tracing::debug!(
                target: "mesh::ble",
                peer = %claimed,
                "sync request from a peer with no verified announce ignored"
            );
            return;
        }

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
                self.rekey_if_due(peer).await;
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

    /// Renegotiate a session that has spent 90% of its nonce budget, before
    /// encryption hard-fails at the limit. The old session keeps carrying
    /// traffic until the new handshake completes and replaces it; the peer
    /// treats our opening message as a restart and does the same.
    ///
    /// Only the message-count half of bitchat's `needsRenegotiation()` is
    /// checked; its 24-hour idle half is not implemented here.
    async fn rekey_if_due(&mut self, peer: &PeerId) {
        if self.state.with(|state| state.sessions.needs_rekey(peer)) {
            tracing::info!(target: "mesh::ble", %peer, "session near its nonce budget; rekeying");
            self.start_handshake(peer).await;
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
        self.rekey_if_due(peer).await;
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
        // Before `files_out.insert`, so a manifest that will not encode leaves no
        // transfer behind to stream chunks nobody can place.
        let Some(manifest_packet) = self.build_manifest_packet(&transfer.manifest) else {
            return;
        };
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

    /// The manifest as a signed packet, or `None` if it cannot be encoded.
    ///
    /// `unwrap_or_default` here put an *empty* payload on the air under a
    /// `FileManifest` type. The receiver drops what it cannot decode, so the
    /// sender would go on to stream every chunk of a transfer the far side had
    /// never been told about — a stall with no error on either device.
    ///
    /// `encode` fails on `validate` (a size or chunk size out of range) or a TLV
    /// field beyond 64 KiB. Both should be unreachable, since `mesh_send_file`
    /// applies the cap before a manifest is ever built. Unreachable is the reason
    /// to return the error rather than paper over it: if it does happen, the
    /// invented value hides the cause exactly where nothing else records it.
    fn build_manifest_packet(&self, manifest: &FileManifest) -> Option<BitchatPacket> {
        let payload = match manifest.encode() {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(
                    target: "mesh::ble",
                    transfer = %hex_encode(&manifest.transfer_id),
                    %error,
                    "file manifest could not be encoded; not sent"
                );
                return None;
            }
        };
        let packet = self.build_packet(
            MessageType::FileManifest,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        // Signed, because the receiver requires it: an unsigned manifest is
        // dropped before its payload is looked at, exactly as an unsigned file
        // transfer is.
        Some(self.maybe_sign(packet, true))
    }

    /// Put the next chunk of each live transfer on the air, and repeat manifests.
    ///
    /// Called from the tick, one chunk at a time, rather than looping over the
    /// whole file: the loop is also the only thing servicing link events, and a
    /// 16 MiB transfer emitted in one pass would leave the mesh deaf for minutes.
    async fn pump_streamed_files(&mut self, now_s: u64) {
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
                let Some(packet) = self.build_manifest_packet(&manifest) else {
                    continue;
                };
                self.state.with(|state| {
                    state
                        .self_broadcasts
                        .record("mesh-manifest-repeat", &packet, now_millis())
                });
                self.transmit(&packet, None, None).await;
            }

            if congested {
                tracing::debug!(
                    target: "mesh::ble",
                    transfer = %hex_encode(&id),
                    queued,
                    cap = CHUNK_PACING_QUEUE_BYTES,
                    "chunk turn skipped: the write queue is backed up"
                );
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
        let last_ms = self.state.with(|state| state.last_announce_ms);
        if !duty::announce_floor_passed(last_ms, now_ms) {
            return;
        }
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

    /// Every *other* link bound to the peer a packet arrived on.
    fn sibling_links_of(&self, ingress_link: Option<&LinkId>) -> Vec<LinkId> {
        let Some(ingress) = ingress_link else {
            return Vec::new();
        };
        self.state.with(|state| sibling_links_locked(state, ingress))
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

        // Split horizon has to be by *peer*, not only by link. Two devices can hold
        // two links to each other at once — each having connected outward to the
        // other — and excluding just the ingress link then leaves the sibling link
        // to the same peer selected, so a relay sends the packet straight back to
        // where it came from.
        //
        // That is not theoretical. On a two-node mesh it turned every broadcast
        // into an echo: the sender logged 250 expired reassemblies of *its own*
        // chunks against 14 real ones, roughly seventeen thousand fragments of its
        // own traffic coming back and occupying the air the transfer needed.
        //
        // `excluded_links` exists for exactly this — its own documentation says
        // "every other link to the ingress peer" — and nothing had ever populated
        // it.
        let siblings = self.sibling_links_of(ingress_link);

        let (plan, frames) = {
            let links = self.state.link_candidates();
            let request = OutboundPlanRequest::new(packet, encoded.len(), &links)
                .with_ingress_link(ingress_link)
                .with_excluded_links(&siblings)
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

        // Queued, never written straight out — and this is the whole of the
        // pacing now. `flush_buffered_writes` takes one frame per link per
        // `WRITE_PACING_DELAY`, which is the rate the radio actually carries.
        //
        // Writing them back to back here is what broke large transfers. BlueZ
        // accepts frames far faster than the link can carry them and discards the
        // excess below the socket, silently, so nothing on either side reported a
        // loss: the receiver simply saw a fraction of each chunk. Android's bridge
        // does push back, which is why the same code worked in one direction and
        // not the other. Spacing every frame on this side is what makes both
        // directions behave, and it needs no estimate of the link's speed.
        //
        // bitchat reaches the same place from the other end — `interFragmentDelayMs
        // = 20` between fragments in `FragmentingPacketSender`. It can sleep inside
        // its send loop because that loop is a coroutine per transfer; this one is
        // the single event loop that also has to service link events, so the wait
        // happens in the drain instead.
        //
        // The cost is that a one-frame packet waits up to one interval too. That is
        // the price of not having to decide per packet which ones are large.
        for link in plan.selected.iter() {
            for frame in &frames {
                let admitted = self.state.with(|state| {
                    state.writes.enqueue(
                        link,
                        frame.clone(),
                        write_priority,
                        PENDING_WRITE_BUFFER_CAP_BYTES,
                    )
                });
                // Byte-capped, so this can refuse. A refusal is a dropped packet
                // and has to be said out loud — which is what `#[must_use]` on
                // `WriteEnqueue` is for.
                if !matches!(admitted, WriteEnqueue::Enqueued { .. }) {
                    tracing::warn!(
                        target: "mesh::ble",
                        link = %link,
                        msg_type = packet.msg_type,
                        "the outbound queue is full; a frame was dropped"
                    );
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
        let state = Arc::clone(&self.state);
        let ingress = ingress.clone();
        let delay = u64::from(decision.delay_ms);
        tokio::spawn(async move {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            transmit_forwarded(&state, &forwarded, &ingress).await;
        });
    }

    // -------------------------------------------------------------------- tick

    async fn on_tick(&mut self) {
        let now_ms = now_millis();
        let now_s = now_secs();

        self.report_throughput(now_ms);

        // Every tick, not on the cleanup cadence: the stall window is 250 ms and
        // the tick 500 ms, so this is the coarsest sweep that still enforces it.
        self.reap_stalled_partials(now_ms);

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
        // A refused scan toggle was discarded, and the two directions fail in
        // opposite ways: unable to *start* means we stop discovering peers
        // altogether — the mesh goes blind while looking perfectly healthy —
        // and unable to stop means the duty cycle silently stops saving power.
        // Neither is inferable from any other line.
        match scan {
            ScanCommand::StartScanning => {
                if let Err(error) = self.radio.set_scanning(true).await {
                    tracing::warn!(
                        target: "mesh::ble",
                        %error,
                        "could not start scanning; no new peer will be discovered"
                    );
                }
            }
            ScanCommand::StopScanning => {
                if let Err(error) = self.radio.set_scanning(false).await {
                    tracing::warn!(
                        target: "mesh::ble",
                        %error,
                        "could not stop scanning; the duty cycle is not saving power"
                    );
                }
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
            //
            // Reported through `degraded` rather than swallowed, because this is
            // the *recovery* path: if it fails, the peripheral role stays
            // invisible and the one mechanism meant to fix that has just given up
            // in silence. A user watching an empty peer list has no other way to
            // learn the difference between an empty room and a radio that will
            // not advertise.
            if let Err(error) = self.radio.start().await {
                self.degraded(format!(
                    "nobody can see us and the radio would not restart to fix it: {error}"
                ));
            }
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
        self.pump_streamed_files(now_s).await;
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

    /// Drop partial inbound frames that have sat past the stall window.
    ///
    /// `append` only discards a stalled frame when the same link writes again, so
    /// a peer that sends one byte and goes silent — still connected, never
    /// writing — would pin its partial until disconnect. This is the sweep that
    /// reclaims it.
    fn reap_stalled_partials(&mut self, now_ms: u64) {
        let reaped = self
            .state
            .with(|state| state.assembler.reap_stalled(InboundLimits::default(), now_ms));
        for stalled in reaped {
            tracing::warn!(
                target: "mesh::ble",
                link = %stalled.link,
                discarded_bytes = stalled.discarded_bytes,
                idle_ms = stalled.idle_ms,
                "a partial inbound frame stalled past its window and was reclaimed"
            );
        }
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

    /// Say how fast frames are actually reaching the air, while that is in doubt.
    ///
    /// Pacing allows one frame per [`WRITE_PACING_DELAY`] — fifty a second — but
    /// the drain runs from a `select!` arm competing with radio events and the
    /// tick, so the achieved rate is a separate question from the permitted one.
    /// A 2 MB transfer worked out at roughly 13 frames a second when derived from
    /// the receiver, and nothing in either log could say whether that was the
    /// link's ceiling or the loop's.
    ///
    /// Only while something is queued or in flight, so a quiet mesh stays quiet,
    /// and at info because the question usually comes up on a phone, where debug
    /// is not emitted.
    fn report_throughput(&mut self, now_ms: u64) {
        let queued = self.state.with(|state| state.writes.total_queued_bytes());
        let busy = queued > 0 || !self.files_out.is_empty();
        if !busy {
            // Reset the window so the first report after a quiet spell measures
            // the transfer rather than the silence before it.
            self.frames_written
                .store(0, std::sync::atomic::Ordering::Relaxed);
            self.last_throughput_report_ms = now_ms;
            return;
        }
        let elapsed = now_ms.saturating_sub(self.last_throughput_report_ms);
        if elapsed < THROUGHPUT_REPORT_INTERVAL_MS {
            return;
        }
        let frames = self
            .frames_written
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        self.last_throughput_report_ms = now_ms;
        tracing::info!(
            target: "mesh::ble",
            frames,
            per_second = frames * 1_000 / elapsed.max(1),
            ceiling_per_second = 1_000 / WRITE_PACING_DELAY.as_millis().max(1) as u64,
            queued_bytes = queued,
            "frames on the air"
        );
    }

    /// Put everything queued on the air at once, ignoring the pacing interval.
    ///
    /// Tests only. Sending now queues, and the queue drains one frame per link per
    /// [`WRITE_PACING_DELAY`] — so a test that sends and then reads the mock
    /// radio's log sees nothing without this. Draining explicitly is better than
    /// sleeping through the real interval: the tests assert *what* went on the air,
    /// and the pacing has its own test for *when*.
    #[cfg(test)]
    async fn drain_writes(&self) {
        // Bounded rather than `while !is_empty()`: a radio that refuses every
        // write would otherwise spin here forever instead of failing the test.
        for _ in 0..10_000 {
            if self.state.with(|state| state.writes.is_empty()) {
                return;
            }
            self.flush_buffered_writes().await;
        }
    }

    /// Put **one** frame per link on the air.
    ///
    /// Called every [`WRITE_PACING_DELAY`] while anything is queued, so one frame
    /// per link per interval is the transmit rate — deliberately, and this is the
    /// only thing that limits it. It used to drain each link until the radio
    /// refused, which is no limit at all where the radio never refuses: BlueZ
    /// accepted every frame and dropped what the link could not carry.
    ///
    /// One per call rather than a batch because the interval is calibrated per
    /// frame. Draining two would double the rate and put the guesswork back.
    async fn flush_buffered_writes(&self) {
        let next = self.state.with(|state| {
            let links: Vec<LinkId> = state.writes.link_ids().cloned().collect();
            links
                .into_iter()
                .filter_map(|link| {
                    let mut writes = state.writes.take_all(&link);
                    if writes.is_empty() {
                        return None;
                    }
                    let head = writes.remove(0);
                    if !writes.is_empty() {
                        // The tail goes back in order. A reordered fragment
                        // stream is worse than a delayed one.
                        state.writes.prepend(&link, writes);
                    }
                    Some((link, head))
                })
                .collect::<Vec<_>>()
        });

        for (link, write) in next {
            match self.radio.send(&link, &write.bytes).await {
                Ok(()) => {
                    self.frames_written
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(error) if should_buffer(&error) => {
                    // The radio does push back on some platforms. Put it back at
                    // the head and try again on the next interval.
                    self.state
                        .with(|state| state.writes.prepend(&link, vec![write]));
                    tracing::trace!(target: "mesh::ble", %link, %error, "write deferred");
                }
                Err(error) => {
                    // The whole queue for this link goes, not just this frame.
                    //
                    // The error that actually shows up here, now that it is
                    // logged, is "link is no longer connected": the peer is gone
                    // and every remaining frame will fail the same way. Draining
                    // them one per pacing interval would spend 20 ms each to
                    // discover that — twenty seconds for a single queued chunk —
                    // while the interval that matters is the one live links are
                    // waiting for.
                    //
                    // A disconnect event already clears the queue via
                    // `discard_all`; this covers the window where frames were
                    // queued before the event arrived, which is exactly when a
                    // transfer is in flight.
                    let discarded_types = self.state.with(|state| state.writes.discard_all(&link));
                    // Counted per type rather than listed: a stalled transfer
                    // discards dozens of fragments at once and the shape is the
                    // information, not the sequence.
                    let mut by_type: std::collections::BTreeMap<u8, usize> =
                        std::collections::BTreeMap::new();
                    for msg_type in &discarded_types {
                        *by_type.entry(*msg_type).or_default() += 1;
                    }
                    // The type distinguishes a loss that matters from one that does
                    // not, and without it the line cannot: an announce or a sync
                    // request is re-sent on the next interval, while a message or a
                    // file chunk is gone. Byte 1 of the frame is the packet type
                    // (`codec.rs`: version:1 type:1 ttl:1 ...); on a fragment it
                    // reads `Fragment`, which is itself the useful answer, since a
                    // fragmented frame is never periodic chatter.
                    tracing::warn!(
                        target: "mesh::ble",
                        %link,
                        %error,
                        bytes = write.bytes.len(),
                        msg_type = ?write.bytes.get(1),
                        also_discarded = discarded_types.len(),
                        also_discarded_types = ?by_type,
                        "a frame was dropped and the link's queue with it: the radio \
                         refused it and it cannot be retried"
                    );
                }
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

    /// Report something that went wrong without stopping the mesh.
    ///
    /// Warn, not debug. Every message that reaches here is an abnormal condition
    /// already judged worth telling the user about, so it is worth telling a log
    /// reader too — and the Android build emits info and above, which meant the
    /// most detailed explanation of a failure was invisible on the device where
    /// the failure was being reported. "the radio would not start: …" was written
    /// at debug and read by nobody.
    fn degraded(&self, message: String) {
        tracing::warn!(target: "mesh::ble", "{message}");
        self.events.send(MeshEvent::Degraded(message));
    }
}

/// Every *other* link bound to the peer `ingress` is bound to.
///
/// Split horizon has to be by peer, not only by link. Two devices can hold two
/// links to each other at once — each having connected outward to the other — so
/// excluding just the ingress link leaves the sibling link to the same peer
/// selected, and the relay sends the packet straight back where it came from.
///
/// Measured on a two-node mesh before this existed: the sender logged 250 expired
/// reassemblies of its *own* chunks against 14 real ones — roughly seventeen
/// thousand fragments of its own traffic returning to occupy the air its transfer
/// needed. `excluded_links` exists for exactly this and its own documentation says
/// so ("every other link to the ingress peer"); nothing had ever populated it.
///
/// Empty when the ingress link has no peer bound yet: an unbound link cannot be
/// known to lead back to the sender, and excluding on a guess would partition the
/// mesh rather than quiet it.
fn sibling_links_locked(state: &LinkStateInner, ingress: &LinkId) -> Vec<LinkId> {
    let Some(peer) = state
        .links
        .links()
        .find(|(id, _)| *id == ingress)
        .and_then(|(_, link_state)| link_state.peer.clone())
    else {
        return Vec::new();
    };
    state
        .links
        .links()
        .filter(|(id, link_state)| *id != ingress && link_state.peer.as_ref() == Some(&peer))
        .map(|(id, _)| id.clone())
        .collect()
}

/// Report bytes the assembler threw away because the previous partial frame
/// stalled past its window.
///
/// A non-zero `stall_discarded_bytes` on an otherwise *successful* append is the
/// quietest data loss in the inbound path: the frame that follows decodes fine, so
/// nothing looks wrong, while everything the peer sent before the stall is gone.
/// It rides on `Decoded` and `Waiting` alike and was being dropped with `..` at
/// both.
fn report_stall_discard(link: &LinkId, metadata: &InboundAppendMetadata) {
    if metadata.stall_discarded_bytes == 0 {
        return;
    }
    tracing::warn!(
        target: "mesh::ble",
        %link,
        discarded_bytes = metadata.stall_discarded_bytes,
        appended_bytes = metadata.appended_bytes,
        packet_type = ?metadata.packet_type,
        "a stalled partial frame was discarded; reassembly restarted"
    );
}

/// The peer's redundant *central* links — every one except the single keeper.
///
/// A BLE peer advertises under a resolvable private address that rotates every
/// fifteen minutes or so. Nothing in the address survives the rotation, so a
/// scanner sees each one as a device it has never met, dials it, and ends up
/// holding a second live ACL to a phone it was already connected to. The
/// registry allowed for this — `links_to` scans rather than consulting a 1:1
/// reverse map precisely because "one peer can legitimately hold several links
/// at once" — but nothing ever closed the extras.
///
/// Measured on one desktop and one Galaxy S9+ in the same room: eight central
/// links to a single peer in under two minutes, under eight distinct addresses,
/// all binding to `4b51a6a51aa7eb3b`. There are only six central slots, so one
/// phone starved the whole mesh, and the phone's own GATT server saturated —
/// which is where `timed out during finding the mesh service` came from.
///
/// Only central links are pruned. A peripheral link is one the peer dialled to
/// us; we did not spend a slot on it and it is not ours to hang up. Two devices
/// legitimately holding one link each way is the case [`sibling_links_locked`]
/// exists for.
///
/// The keeper is chosen deterministically — writable first, then *newest* —
/// because every link carrying an announce runs this, and two announces arriving
/// together must agree on the survivor or they close each other.
///
/// Newest rather than lowest id, which is what this did first and got wrong. A
/// duplicate exists because the peer rotated its address, so the older link is
/// the one sitting on an address the peer has moved off; keeping it discards the
/// fresher ACL for a dying one. Measured on the cold-start race: link #1
/// announced, #2 was pruned in its favour, and two seconds later #1 was already
/// refusing writes as "no longer connected" — both links gone and a reconnect
/// from nothing.
///
/// Age comes from the sequence [`Shared::open_link`] appends as `#N`, which is
/// monotonic per radio. An id without one sorts oldest, so a malformed id can
/// never win the tie.
///
/// Keeping the newest is also what makes [`ConnectionScheduler::note_duplicate_peer`]
/// safe, which is not obvious from either site. That map blacklists the pruned
/// address for five minutes; because the pruned link is always the older one, the
/// address blacklisted is always one the peer has moved off. Keep the oldest
/// instead and it would blacklist the address the peer is currently reachable on.
fn redundant_central_links_locked(state: &LinkStateInner, peer: &PeerId) -> Vec<LinkId> {
    let mut central: Vec<_> = state
        .links
        .links()
        .filter(|(_, link_state)| {
            link_state.role == LinkRole::Central && link_state.peer.as_ref() == Some(peer)
        })
        .collect();
    if central.len() < 2 {
        return Vec::new();
    }
    central.sort_by_key(|(id, link_state)| (!link_state.writable, Reverse(link_seq(id))));
    central.into_iter().skip(1).map(|(id, _)| id.clone()).collect()
}

/// The `#N` a [`LinkId`] ends with — its creation order on this radio.
///
/// `0` when absent or unparseable, which sorts oldest: an id we cannot read the
/// age of must never be chosen as the survivor over one we can.
fn link_seq(link: &LinkId) -> u64 {
    link.as_str()
        .rsplit_once('#')
        .and_then(|(_, seq)| seq.parse().ok())
        .unwrap_or(0)
}

/// Write a relayed packet, with the ingress link excluded.
///
/// A free function so the relay path can be spawned: it needs the shared state and
/// nothing else from the runtime. It no longer needs the radio — everything goes
/// through the paced queue now, the same as `transmit`.
async fn transmit_forwarded(
    state: &Arc<MeshState>,
    packet: &BitchatPacket,
    ingress: &LinkId,
) {
    let padding = fragment::pads_ble_frame(packet.msg_type);
    let Ok(encoded) = codec::encode(packet, padding) else {
        return;
    };
    let (links, siblings) = state.with(|inner| {
        (
            link_candidates_locked(inner),
            sibling_links_locked(inner, ingress),
        )
    });
    let request = OutboundPlanRequest::new(packet, encoded.len(), &links)
        // Split horizon: never send a packet back down the link it arrived on —
        // nor down any other link to the same peer, which is what `siblings` adds.
        .with_ingress_link(Some(ingress))
        .with_excluded_links(&siblings)
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

    // Queued, exactly as `transmit` does, and for the same reason. This path was
    // still writing frame after frame straight at the radio, so every *relayed*
    // packet bypassed the pacing — and on a mesh most traffic is relayed. A relay
    // blasting a 70-fragment chunk as fast as BlueZ accepts it is precisely the
    // behaviour that lost fragments below the socket in the first place, just
    // moved one hop along.
    for link in plan.selected.iter() {
        for frame in &frames {
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
                    target: "mesh::ble",
                    link = %link,
                    msg_type = packet.msg_type,
                    "the outbound queue is full; a relayed frame was dropped"
                );
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
        let storage = Arc::new(MemoryStorage::new()) as Arc<dyn Storage>;
        let router = Arc::new(RouterHandle::spawn(super::super::router::RouterDeps {
            state: Arc::clone(&state),
            nostr_state: Arc::clone(&nostr_state),
            identity: Arc::clone(&identity),
            storage: Arc::new(MemoryStorage::new()),
            sealer: Arc::new(NoopSealer),
            ble: crate::services::mesh::CommandBus::new().0,
            nostr: crate::services::mesh::CommandBus::new().0,
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
            Arc::new(NoopSealer),
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

    #[tokio::test]
    async fn leave_requires_the_pinned_signature_and_preserves_the_pin() {
        let (mut runtime, _radio, state) = runtime();
        let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &peer, "Alice");
        let attacker = MeshIdentity::generate(&mut OsRng);
        let link = LinkId::new("/mock/leave");
        let mut packet = BitchatPacket::new(
            MessageType::Leave, peer.to_routing_bytes().unwrap(), None,
            now_millis(), Vec::new(), chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        assert!(!runtime.handle_typed(&link, &packet, &peer).await);
        packet.signature = Some(sign_packet(&attacker, &packet).unwrap());
        assert!(!runtime.handle_typed(&link, &packet, &peer).await);
        assert!(state.with(|s| s.peers.info(&peer).is_some()));
        packet.signature = Some(sign_packet(&identity, &packet).unwrap());
        assert!(runtime.handle_typed(&link, &packet, &peer).await);
        assert!(state.with(|s| s.peers.info(&peer).is_none()));
        assert!(state.with(|s| s.peers.pinned(&peer).is_some()));
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

        // One link, owned by the relay.
        let link = LinkId::new("/mock/relay");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
            inner.links.bind_if_unbound(&link, &relay);
        });

        // A signed announce authored by the distant peer, arriving on that link.
        // Its ID has to be the one its own Noise key derives, as a real device's
        // is — an announce that claims any other ID is now refused outright.
        let identity = MeshIdentity::generate(&mut OsRng);
        let distant = identity.peer_id();
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

    /// §98 — the other half of the same attack, once the derivation check closed
    /// the first half.
    ///
    /// A peer's Noise public key travels in the clear in every announce it sends,
    /// and the peer ID derives from it, so an attacker who has simply HEARD a
    /// victim announce can replay that Noise key and the derived ID will match.
    /// Everything then hinges on the signing key — which was pinned by nothing.
    /// The attacker signs with their own key, the signature verifies against the
    /// key the packet supplies, and the victim's identity is taken over in our
    /// registry.
    ///
    /// The registry had recorded the victim's signing key since first contact.
    /// Nothing compared it.
    #[tokio::test]
    async fn a_replayed_noise_key_under_a_new_signing_key_cannot_take_over_a_known_peer() {
        let (mut runtime, _radio, state) = runtime();

        let victim_identity = MeshIdentity::generate(&mut OsRng);
        let victim = victim_identity.peer_id();

        // First contact: the victim announces itself, honestly.
        let honest = AnnouncementPacket::new(
            "victim".to_string(),
            victim_identity.static_public().to_vec(),
            victim_identity.signing_public().to_vec(),
            None,
            None,
            None,
        );
        let mut honest_packet = BitchatPacket::new(
            MessageType::Announce,
            victim.to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            honest.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        honest_packet.signature = Some(sign_packet(&victim_identity, &honest_packet).expect("signs"));

        let victim_link = LinkId::new("/mock/victim");
        state.with(|inner| {
            inner.links.link_up(victim_link.clone(), LinkRole::Central);
            inner.links.set_writable(&victim_link, true);
            inner.max_write.insert(victim_link.clone(), 512);
        });
        assert!(
            runtime.on_announce(&victim_link, &honest_packet, &victim).await,
            "the honest announce establishes the pin"
        );
        let pinned_signing_key = state
            .with(|inner| inner.peers.info(&victim).and_then(|r| r.signing_public_key.clone()))
            .expect("the registry records the signing key on first contact");

        // The attacker replays the victim's Noise key — so the peer ID still
        // derives correctly — under its own signing key, and self-signs.
        let attacker = MeshIdentity::generate(&mut OsRng);
        let forged = AnnouncementPacket::new(
            "victim".to_string(),
            victim_identity.static_public().to_vec(), // heard off the air
            attacker.signing_public().to_vec(),       // the attacker's own
            None,
            None,
            None,
        );
        let mut forged_packet = BitchatPacket::new(
            MessageType::Announce,
            victim.to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            forged.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        forged_packet.signature = Some(sign_packet(&attacker, &forged_packet).expect("signs"));

        let attacker_signing_key =
            <[u8; 32]>::try_from(attacker.signing_public().as_slice()).expect("32 bytes");
        assert!(
            verify_packet(&forged_packet, &attacker_signing_key),
            "the fixture must be a self-signature that really verifies — that is the attack"
        );

        let attacker_link = LinkId::new("/mock/attacker");
        state.with(|inner| {
            inner.links.link_up(attacker_link.clone(), LinkRole::Central);
            inner.links.set_writable(&attacker_link, true);
            inner.max_write.insert(attacker_link.clone(), 512);
        });

        runtime.save_pins().expect("save pins");
        state.with(|inner| {
            inner.peers.clear_pins();
            inner.peers.remove(&victim);
        });
        runtime.restore_pins().expect("restore pins");
        assert!(state.with(|inner| inner.peers.info(&victim).is_none()));
        let mut events = runtime.events.subscribe();
        let accepted = runtime.on_announce(&attacker_link, &forged_packet, &victim).await;
        assert!(matches!(events.try_recv(), Ok(MeshEvent::IdentityKeyChanged { peer }) if peer == victim));

        assert!(!accepted, "the forged announce must be refused");
        assert_eq!(
            state.with(|inner| inner.peers.pinned(&victim).map(|r| r.signing_public_key.clone())),
            Some(pinned_signing_key),
            "the victim's pinned signing key must survive the attempt"
        );
        assert!(
            state.with(|inner| inner.links.peer_for(&attacker_link).is_none()),
            "and the attacker's link must not be bound to the victim's identity"
        );
    }

    #[tokio::test]
    async fn an_announce_claiming_a_peer_id_it_cannot_derive_is_rejected() {
        // The mesh ID *is* SHA256(noise static key)[..8] — that is how our own
        // is built (`MeshIdentity::peer_id`). An announce is therefore checkable
        // without any crypto at all: derive the ID from the key it carries and
        // compare it to the sender ID on the packet.
        //
        // Verifying only the packet signature against the signing key the
        // announcement itself supplies proves nothing about *who* sent it — it
        // proves whoever wrote the announce holds the key they put in it, which
        // every attacker does. Anyone in radio range could announce any peer ID
        // with their own keys and take over that identity in our registry.
        let (mut runtime, _radio, state) = runtime();

        // A real peer, whose ID genuinely derives from its own Noise key.
        let victim_identity = MeshIdentity::generate(&mut OsRng);
        let victim = victim_identity.peer_id();

        // The attacker claims the victim's ID and supplies its own two keys,
        // then signs with the matching signing secret.
        let attacker = MeshIdentity::generate(&mut OsRng);
        let announcement = AnnouncementPacket::new(
            "victim".to_string(),
            attacker.static_public().to_vec(),
            attacker.signing_public().to_vec(),
            None,
            None,
            None,
        );
        let mut packet = BitchatPacket::new(
            MessageType::Announce,
            victim.to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            announcement.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        packet.signature = Some(sign_packet(&attacker, &packet).expect("signs"));

        // The forgery is *signature valid*. That is the point: the signature is
        // the attacker's own, over their own key, and it verifies.
        let signing_key = <[u8; 32]>::try_from(announcement.signing_public_key.as_slice())
            .expect("32 bytes");
        assert!(
            verify_packet(&packet, &signing_key),
            "the fixture must be a self-signature that really verifies"
        );

        let link = LinkId::new("/mock/attacker");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });

        let accepted = runtime.on_announce(&link, &packet, &victim).await;

        assert!(!accepted, "the announce must be refused");
        assert!(
            state.with(|inner| inner.peers.info(&victim).is_none()),
            "an announce whose sender ID is not derivable from the Noise key it \
             carries must not enter the peer registry"
        );
        assert!(
            state.with(|inner| inner.links.peer_for(&link).is_none()),
            "and must not bind the attacker's link to the victim's identity"
        );
    }

    #[tokio::test]
    async fn a_sync_request_from_a_peer_that_never_announced_is_not_answered() {
        // A REQUEST_SYNC carrying an empty or undecodable GCS filter matches
        // nothing, so `handle_request_sync` reads it as "this peer holds none of
        // it" and replies with the whole store. The requester is whatever sender
        // ID the packet claimed, and answering it needs no session, no announce
        // and no binding of any kind — so anyone in radio range could ask once
        // and be handed every announce and prekey bundle we hold, each keyed by
        // its owner's identity. That is a social graph of everyone we have met.
        let (mut runtime, radio, state) = runtime();

        // Something identity-bearing in the store: one real peer's announce.
        let subject = MeshIdentity::generate(&mut OsRng);
        let subject_peer = subject.peer_id();
        let announcement = AnnouncementPacket::new(
            "subject".to_string(),
            subject.static_public().to_vec(),
            subject.signing_public().to_vec(),
            None,
            None,
            None,
        );
        let mut announce = BitchatPacket::new(
            MessageType::Announce,
            subject_peer.to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            announcement.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        announce.signature = Some(sign_packet(&subject, &announce).expect("signs"));
        let subject_link = LinkId::new("/mock/subject");
        state.with(|inner| {
            inner.links.link_up(subject_link.clone(), LinkRole::Central);
            inner.links.set_writable(&subject_link, true);
            inner.max_write.insert(subject_link.clone(), 512);
        });
        runtime.on_packet(&subject_link, announce).await;
        // Flush whatever the announce itself set in motion, so the only writes
        // the assertion can see are the answer to the stranger's request.
        runtime.drain_writes().await;
        assert!(
            runtime.gossip.has_announcement(&subject_peer),
            "the fixture must really have something worth leaking"
        );

        // A stranger we have never heard an announce from asks for everything.
        let stranger = PeerId::from_str("cccccccccccccccc");
        let link = LinkId::new("/mock/stranger");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
            inner.links.bind_if_unbound(&link, &stranger);
        });
        let mut request = RequestSyncPacket::new(19, 4096, Vec::new());
        request.types = Some(u64::MAX);
        let packet = BitchatPacket::new(
            MessageType::RequestSync,
            stranger.to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            request.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );

        let before = radio.write_count();
        runtime.on_request_sync(&packet, &stranger).await;
        runtime.drain_writes().await;

        assert_eq!(
            radio.write_count(),
            before,
            "a peer that has never proved an identity to us gets no sync response"
        );
    }

    #[tokio::test]
    async fn a_sync_request_from_a_peer_that_did_announce_is_still_answered() {
        // The gate must not break sync itself: an honest peer announces on
        // connect and then asks, and that request has to be served or two
        // devices in a room never exchange history at all.
        let (mut runtime, radio, state) = runtime();

        let subject = MeshIdentity::generate(&mut OsRng);
        let subject_peer = subject.peer_id();
        let announcement = AnnouncementPacket::new(
            "subject".to_string(),
            subject.static_public().to_vec(),
            subject.signing_public().to_vec(),
            None,
            None,
            None,
        );
        let mut announce = BitchatPacket::new(
            MessageType::Announce,
            subject_peer.to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            announcement.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        announce.signature = Some(sign_packet(&subject, &announce).expect("signs"));
        let link = LinkId::new("/mock/subject");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });
        runtime.on_packet(&link, announce).await;
        runtime.drain_writes().await;

        let mut request = RequestSyncPacket::new(19, 4096, Vec::new());
        request.types = Some(u64::MAX);
        let packet = BitchatPacket::new(
            MessageType::RequestSync,
            subject_peer.to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            request.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );

        let before = radio.write_count();
        runtime.on_request_sync(&packet, &subject_peer).await;
        runtime.drain_writes().await;

        assert!(
            radio.write_count() > before,
            "an announced peer must still get its diff"
        );
    }

    #[tokio::test]
    async fn a_refused_announce_is_not_kept_or_re_served_by_the_gossip_store() {
        // Refusing a forgery from our own registry is only half the job. The
        // gossip store keeps the latest announce per peer and serves it to
        // everyone who syncs with us, so retaining one we just refused turns this
        // device into a relay for the forgery. And `on_public_packet_seen` reads
        // an announce older than the stale-peer timeout as "this peer left" and
        // purges the store's state for the claimed sender — which would make an
        // old forged announce a remote wipe of a real peer's gossip state.
        let (mut runtime, _radio, state) = runtime();

        let victim_identity = MeshIdentity::generate(&mut OsRng);
        let victim = victim_identity.peer_id();
        let attacker = MeshIdentity::generate(&mut OsRng);
        let announcement = AnnouncementPacket::new(
            "victim".to_string(),
            attacker.static_public().to_vec(),
            attacker.signing_public().to_vec(),
            None,
            None,
            None,
        );
        let mut packet = BitchatPacket::new(
            MessageType::Announce,
            victim.to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            announcement.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        packet.signature = Some(sign_packet(&attacker, &packet).expect("signs"));

        let link = LinkId::new("/mock/attacker");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });

        runtime.on_packet(&link, packet).await;

        assert!(
            !runtime.gossip.has_announcement(&victim),
            "a refused announce must not be retained for re-gossip"
        );
    }

    #[tokio::test]
    async fn an_announce_may_not_swap_the_noise_key_recorded_for_a_peer() {
        // Defence in depth behind the derivation check: the sender ID is only
        // eight bytes of the digest, so a second key hashing into the same ID is
        // 2^64 work rather than impossible. Once we have recorded a Noise key
        // for a peer, an announce carrying a *different* one is a key
        // substitution attempt and is refused before the signature is even
        // checked — a valid signature over a swapped key is exactly what the
        // attack looks like (`BLEAnnounceTrustPolicy`, `MessageHandler.kt:252`).
        let (mut runtime, _radio, state) = runtime();

        let honest = MeshIdentity::generate(&mut OsRng);
        let peer = honest.peer_id();
        let first = AnnouncementPacket::new(
            "honest".to_string(),
            honest.static_public().to_vec(),
            honest.signing_public().to_vec(),
            None,
            None,
            None,
        );
        state.with(|inner| {
            inner
                .peers
                .upsert_verified_announce(&peer, &first, true, now_millis());
        });

        // Same peer ID, attacker's keys, signed by the attacker.
        let attacker = MeshIdentity::generate(&mut OsRng);
        let swapped = AnnouncementPacket::new(
            "honest".to_string(),
            attacker.static_public().to_vec(),
            attacker.signing_public().to_vec(),
            None,
            None,
            None,
        );
        let mut packet = BitchatPacket::new(
            MessageType::Announce,
            peer.to_routing_bytes().expect("routing bytes"),
            None,
            now_millis(),
            swapped.encode().expect("encodes"),
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        packet.signature = Some(sign_packet(&attacker, &packet).expect("signs"));

        let link = LinkId::new("/mock/swap");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });

        runtime.on_announce(&link, &packet, &peer).await;

        assert_eq!(
            state.with(|inner| inner
                .peers
                .info(&peer)
                .expect("still known")
                .noise_public_key
                .clone()),
            Some(honest.static_public().to_vec()),
            "the recorded Noise key must survive an announce that tries to \
             replace it — that key is what courier mail is sealed to"
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

        runtime.drain_writes().await;
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
        let (mut runtime, radio, state) = runtime();
        let local = runtime.identity.peer_id();
        let link_out = LinkId::new("/mock/out");
        state.with(|inner| {
            inner.links.link_up(link_out.clone(), LinkRole::Central);
            inner.links.set_writable(&link_out, true);
            inner.max_write.insert(link_out.clone(), 512);
        });

        // Build the announce exactly as we would send it, then feed it back.
        runtime.announce().await;
        runtime.drain_writes().await;
        let sent = radio.writes();
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

        runtime.drain_writes().await;

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
    async fn announces_from_any_cause_are_floored_at_one_second() {
        let (mut runtime, radio, state) = runtime();
        let link = LinkId::new("/mock/one");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });

        // A connect storm or a repeated "announce now" inside the floor: one goes out.
        runtime.announce().await;
        runtime.announce().await;
        runtime.announce().await;
        runtime.drain_writes().await;
        assert_eq!(radio.writes().len(), 1, "announces inside the floor are dropped");

        // Once the floor has passed, the next one goes out.
        state.with(|inner| {
            inner.last_announce_ms = inner
                .last_announce_ms
                .saturating_sub(chatter_mesh_link::duty::ANNOUNCE_MIN_INTERVAL_MS);
        });
        runtime.announce().await;
        runtime.drain_writes().await;
        assert_eq!(radio.writes().len(), 2);
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
        runtime.drain_writes().await;
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

        runtime.drain_writes().await;

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

        runtime.drain_writes().await;

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

        runtime.drain_writes().await;

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

    #[cfg(feature = "testing")]
    #[tokio::test]
    async fn a_send_past_the_rekey_threshold_opens_one_renegotiation() {
        use chatter_mesh_noise::session::REKEY_MESSAGE_THRESHOLD;

        let (mut runtime, radio, state) = runtime();
        let local_peer = runtime.identity.peer_id();
        let peer_identity = MeshIdentity::generate(&mut OsRng);
        let peer = peer_identity.peer_id();
        let link = LinkId::new("/mock/rekey");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.bind_if_unbound(&link, &peer);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });

        // Establish a session: the peer initiates, we answer.
        let mut remote = super::super::session::SessionManager::new();
        let m1 = remote.initiate(&local_peer, &peer_identity).expect("m1");
        let identity = Arc::clone(&runtime.identity);
        let m2 = state
            .with(|inner| inner.sessions.on_handshake(&peer, &local_peer, &identity, &m1))
            .expect("we read m1")
            .reply
            .expect("m2");
        let m3 = remote
            .on_handshake(&local_peer, &peer, &peer_identity, &m2)
            .expect("peer reads m2")
            .reply
            .expect("m3");
        state
            .with(|inner| inner.sessions.on_handshake(&peer, &local_peer, &identity, &m3))
            .expect("we complete");
        assert!(state.has_session(&peer));

        // Below the threshold a send is just a send.
        runtime.send_private(&peer, "MSG-1", "hello").await;
        runtime.drain_writes().await;
        let types = |radio: &MockLink| -> Vec<Option<MessageType>> {
            radio
                .writes()
                .iter()
                .map(|(_, bytes)| codec::decode(bytes).expect("decodable").message_type())
                .collect()
        };
        assert_eq!(types(&radio), [Some(MessageType::NoiseEncrypted)]);

        // At the threshold the message still goes out on the old session, and
        // one handshake follows it; a second send does not restart it.
        state.with(|inner| {
            inner
                .sessions
                .set_messages_sent_for_testing(&peer, REKEY_MESSAGE_THRESHOLD)
        });
        runtime.send_private(&peer, "MSG-2", "again").await;
        runtime.send_private(&peer, "MSG-3", "and again").await;
        runtime.drain_writes().await;
        assert_eq!(
            types(&radio),
            [
                Some(MessageType::NoiseEncrypted),
                Some(MessageType::NoiseEncrypted),
                Some(MessageType::NoiseHandshake),
                Some(MessageType::NoiseEncrypted),
            ]
        );
        assert!(state.has_session(&peer), "the old session stays until replaced");
        assert!(state.with(|inner| inner.sessions.is_handshaking(&peer)));
    }

    /// Courier mail is released only on a link that proved the recipient's key
    /// itself. A peer-level session used to suffice, which is exactly the state a
    /// replayed announce rebinding an attacker's link produces.
    #[tokio::test]
    async fn courier_mail_waits_for_a_handshake_on_the_announcing_link() {
        use chatter_mesh::tlv::courier_envelope::{epoch_day, recipient_tag};

        let (mut runtime, radio, state) = runtime();
        let local_peer = runtime.identity.peer_id();
        let peer_identity = MeshIdentity::generate(&mut OsRng);
        let peer = peer_identity.peer_id();
        let link = LinkId::new("/mock/courier");
        state.with(|inner| {
            inner.links.link_up(link.clone(), LinkRole::Central);
            inner.links.set_writable(&link, true);
            inner.max_write.insert(link.clone(), 512);
        });

        // We carry one envelope for the peer.
        let now_s = now_millis() / 1000;
        let recipient = peer_identity.static_public();
        let envelope = CourierEnvelope::new(
            recipient_tag(&recipient, epoch_day(now_s)),
            (now_s + 3600) * 1000,
            vec![0x5A; 96],
            1,
            None,
        );
        runtime
            .couriers
            .deposit(&envelope, &[0xA1; 32], now_s)
            .expect("deposit");

        // A peer-level session exists, but it was not made on this link.
        let mut remote = super::super::session::SessionManager::new();
        let identity = Arc::clone(&runtime.identity);
        let m1 = remote.initiate(&local_peer, &peer_identity).expect("m1");
        let m2 = state
            .with(|inner| inner.sessions.on_handshake(&peer, &local_peer, &identity, &m1))
            .expect("m1 read")
            .reply
            .expect("m2");
        let m3 = remote
            .on_handshake(&local_peer, &peer, &peer_identity, &m2)
            .expect("m2 read")
            .reply
            .expect("m3");
        state
            .with(|inner| inner.sessions.on_handshake(&peer, &local_peer, &identity, &m3))
            .expect("established");

        let announce = |timestamp: u64| {
            let announcement = AnnouncementPacket::new(
                "recipient".to_string(),
                peer_identity.static_public().to_vec(),
                peer_identity.signing_public().to_vec(),
                None,
                None,
                None,
            );
            let mut packet = BitchatPacket::new(
                MessageType::Announce,
                peer.to_routing_bytes().expect("routing bytes"),
                None,
                timestamp,
                announcement.encode().expect("encodes"),
                chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
            );
            packet.signature = Some(sign_packet(&peer_identity, &packet).expect("signs"));
            packet
        };
        let sent = |radio: &MockLink, kind: MessageType| -> Vec<BitchatPacket> {
            radio
                .writes()
                .iter()
                .map(|(_, bytes)| codec::decode(bytes).expect("decodable"))
                .filter(|packet| packet.message_type() == Some(kind))
                .collect()
        };

        // A direct, verified announce: no mail yet, but a handshake to prove it.
        assert!(runtime.on_announce(&link, &announce(now_millis()), &peer).await);
        runtime.drain_writes().await;
        assert!(sent(&radio, MessageType::CourierEnvelope).is_empty());
        assert!(runtime.couriers.carries_for(&recipient, now_s));
        let opening = sent(&radio, MessageType::NoiseHandshake);
        assert_eq!(opening.len(), 1, "one handshake opened to prove the link");

        // The peer answers on this link; completing the handshake marks it.
        let mut remote = super::super::session::SessionManager::new();
        let m2 = remote
            .on_handshake(&local_peer, &peer, &peer_identity, &opening[0].payload)
            .expect("peer reads m1")
            .reply
            .expect("m2");
        let reply = BitchatPacket::new(
            MessageType::NoiseHandshake,
            peer.to_routing_bytes().expect("routing bytes"),
            local_peer.to_routing_bytes(),
            now_millis(),
            m2,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        runtime.on_handshake(&link, &reply, &peer).await;
        assert!(state.with(|inner| inner.links.is_noise_authenticated(&link, &peer)));

        // The next announce on the proved link releases the mail.
        assert!(runtime.on_announce(&link, &announce(now_millis() + 1), &peer).await);
        runtime.drain_writes().await;
        assert_eq!(sent(&radio, MessageType::CourierEnvelope).len(), 1);
        assert!(!runtime.couriers.carries_for(&recipient, now_s));
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

        runtime.drain_writes().await;
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
        runtime.drain_writes().await;

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
        crate::test_tempdir::hold(dir);
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

    /// `writable_link`, with the radio told the same MTU the planner will use.
    ///
    /// Worth the extra argument: it is what makes an oversized write fail in a
    /// test rather than on a radio. The two numbers being the same is the point —
    /// a fragmenter that sizes against one and writes against the other is exactly
    /// the bug this catches.
    fn writable_link_on(
        state: &MeshState,
        radio: &MockLink,
        name: &str,
        max_write: usize,
    ) -> LinkId {
        let link = writable_link(state, name, max_write);
        radio.set_max_write(&link, max_write);
        link
    }

    #[tokio::test]
    async fn the_tick_reclaims_a_partial_frame_from_a_peer_that_goes_silent() {
        // One byte in through the real inbound path, then silence on a link that
        // stays up. `append` would only reclaim it on the next write, which never
        // comes; the tick has to.
        let (mut runtime, _radio, state, _timeline) = runtime_parts();
        let link = writable_link(&state, "silent", 512);

        runtime.on_data(&link, vec![0x01]).await;
        assert_eq!(state.with(|s| s.assembler.buffered_bytes(&link)), 1);

        tokio::time::sleep(Duration::from_millis(
            InboundLimits::default().stall_reset_ms + 50,
        ))
        .await;
        runtime.on_tick().await;

        assert_eq!(
            state.with(|s| s.assembler.total_buffered_bytes()),
            0,
            "a stalled partial survived the tick"
        );
        assert!(state.with(|s| s.assembler.is_empty()));
    }

    #[tokio::test]
    async fn the_tick_leaves_a_partial_frame_that_is_still_inside_its_window() {
        // Stamped in the future so idle time is zero however slow the machine
        // is: the sweep must not eat frames that are still arriving.
        let (mut runtime, _radio, state, _timeline) = runtime_parts();
        let link = writable_link(&state, "busy", 512);
        let _ = state.with(|s| {
            s.assembler.append(
                &link,
                &[InboundChunk { offset: 0, bytes: vec![0x01, 0x02] }],
                InboundLimits::default(),
                now_millis() + 60_000,
            )
        });

        runtime.on_tick().await;

        assert_eq!(state.with(|s| s.assembler.buffered_bytes(&link)), 2);
    }

    #[tokio::test]
    async fn a_radio_that_will_not_restart_when_nobody_can_see_us_is_reported() {
        // `should_ensure_advertising` fires whenever the peer registry is empty,
        // and restarting the radio is the only lever the trait offers for "nobody
        // can see us". The result was discarded, so a restart that failed left the
        // peripheral role invisible and said nothing — the recovery path giving up
        // in silence, which is indistinguishable from an empty room.
        let (mut runtime, radio, _state, _timeline) = runtime_parts();
        let mut events = runtime.events.subscribe();

        // A stopped MockLink refuses to start again, exactly as the Android link
        // does: its handle is single-use.
        let _ = radio.stop().await;

        runtime.on_tick().await;

        let mut degraded = None;
        while let Ok(event) = events.try_recv() {
            if let MeshEvent::Degraded(message) = event {
                degraded = Some(message);
                break;
            }
        }
        let message = degraded.expect(
            "the radio refused to restart while nobody could see us and nothing was reported",
        );
        assert!(
            message.contains("restart"),
            "the notice does not say the restart failed: {message}"
        );
    }

    #[tokio::test]
    async fn a_rotated_address_of_a_held_peer_is_not_dialled() {
        // The fix that stops the churn at its source. Every rotation used to cost
        // a connection just to learn who was behind it, and that connection
        // displaced the ACL already held — whose loss restarted the peer's
        // advertiser under yet another address. Nineteen addresses in four
        // minutes, all one phone.
        let (mut runtime, radio, state, _timeline) = runtime_parts();
        let peer = PeerId::from_str("4b51a6a51aa7eb3b");
        let held = writable_link(&state, "central:dev_54_CB_34#1", 514);
        state.with(|inner| {
            inner.links.bind_if_unbound(&held, &peer);
        });

        let bytes = peer.to_routing_bytes().expect("an eight byte peer id");
        runtime
            .on_discovered("70:E0:8F:D7:6B:84", -50, Some(bytes))
            .await;
        assert!(
            radio.connect_attempts().is_empty(),
            "dialled another address for a peer already held"
        );

        // An advert with no id still gets dialled. The scan response often lands
        // after the advertisement, so `None` has to mean "unknown", never "not a
        // peer" — reading it the other way would make the mesh undiscoverable.
        runtime.on_discovered("70:E0:8F:D7:6B:84", -50, None).await;
        assert_eq!(
            radio.connect_attempts(),
            vec!["70:E0:8F:D7:6B:84".to_string()],
            "an advert with no peer id must still be dialled"
        );
    }

    #[test]
    fn one_peer_under_several_rotated_addresses_keeps_exactly_one_central_link() {
        // The measured failure: a Galaxy S9+ rotating its private address gave one
        // desktop eight central links to one peer inside two minutes, against six
        // central slots. Every extra ACL is a slot the rest of the mesh cannot
        // have and a GATT server the phone has to keep serving.
        let (_runtime, _radio, state, _timeline) = runtime_parts();
        let peer = PeerId::from_str("4b51a6a51aa7eb3b");

        // Three links to the same peer, in the order the rotations arrived. The
        // first is stale — its socket is gone, so it is not writable.
        let stale = writable_link(&state, "central:dev_54_CB_34#1", 514);
        let second = writable_link(&state, "central:dev_70_E0_8F#2", 514);
        let third = writable_link(&state, "central:dev_5B_4F_1D#3", 514);
        // ...and one *peripheral* link to the same peer, which the peer dialled to
        // us. Not ours to hang up.
        let inbound = LinkId::new("peripheral:dev_5B_4F_1D#4");
        state.with(|inner| {
            inner.links.link_up(inbound.clone(), LinkRole::Peripheral);
            inner.links.set_writable(&inbound, true);
            inner.links.set_writable(&stale, false);
            for link in [&stale, &second, &third, &inbound] {
                inner.links.bind_if_unbound(link, &peer);
            }
        });

        let doomed = state.with(|inner| redundant_central_links_locked(inner, &peer));

        // Asserted as properties rather than a fixed list: `links()` is ordered by
        // `LinkId`, which is the D-Bus object path, and a rotated address sorts
        // wherever its hex happens to fall — arrival order is not recoverable from
        // it and the test should not pretend otherwise.
        assert_eq!(doomed.len(), 2, "expected exactly one central link to survive");
        assert!(doomed.contains(&stale), "kept a link whose socket is gone");
        assert!(!doomed.contains(&inbound), "hung up a link the peer dialled to us");
        let keeper = [&second, &third]
            .into_iter()
            .find(|link| !doomed.contains(link))
            .expect("a writable central link survives");
        // The newest writable link, specifically. A duplicate exists because the
        // peer rotated, so the older link sits on an address it has moved off —
        // and keeping that one cost both links in the measured cold-start race.
        assert_eq!(keeper, &third, "kept an older link over a fresher one");

        // Deterministic across ingresses: every link carrying an announce computes
        // this, and two announces landing together must agree on the survivor or
        // they close each other. Same input, same answer.
        state.with(|inner| {
            assert_eq!(
                redundant_central_links_locked(inner, &peer),
                doomed,
                "two announces would disagree on the survivor"
            );
            for link in &doomed {
                inner.links.link_down(link);
            }
            assert!(
                redundant_central_links_locked(inner, &peer).is_empty(),
                "kept pruning once {keeper} was the only one left"
            );
        });
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
        writable_link_on(&state, &radio, "/mock/req", 512);
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
        runtime.drain_writes().await;
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
        writable_link_on(&state, &radio, "/mock/bad", 512);
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
        assert_eq!(restarted.files_in.restore(now_secs()), 1, "the partial transfer is back");
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
        assert_eq!(restarted.files_in.restore(now_secs()), 1);
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
    async fn losing_the_peripheral_role_is_reported_rather_than_silent() {
        // The half-deaf state: this device still advertises, still scans, still
        // dials out — and accepts nobody. Seen as a desktop registering zero
        // inbound links all session while a phone in the same room reported
        // twenty-one successful outbound connections to it, each having subscribed
        // from its own side. Neither log could explain the other, because the side
        // that stopped working said nothing.
        let (mut runtime, _radio, _state, _timeline) = runtime_parts();
        let mut events = runtime.events.subscribe();

        runtime.on_link_event(LinkEvent::PeripheralRoleEnded).await;

        match events.try_recv() {
            Ok(MeshEvent::Degraded(reason)) => assert!(
                reason.to_lowercase().contains("connect"),
                "the reason has to name what stopped working, got {reason:?}"
            ),
            other => panic!(
                "losing inbound links must be reported, not swallowed; got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn a_connect_timeout_earns_a_longer_ignore_than_an_ordinary_refusal() {
        // The runtime tells the two apart on purpose — a timeout means the peer
        // held a scarce central slot for twenty seconds and gave nothing back, an
        // outright refusal means it said no cheaply — and until the mock could fail
        // a connect, neither branch had ever run in a test. The comment in `dial`
        // records that `note_connect_timeout` "existed and was never reachable
        // from here", which is the same class of gap: code written for a case
        // nothing could produce.
        use chatter_mesh_link::LinkError;

        const PEER: &str = "AA:BB:CC:DD:EE:FF";
        const STRONG: i16 = -40;

        // A timeout: ignored for the full timeout window.
        let (mut runtime, radio, state, _timeline) = runtime_parts();
        radio.set_connect_error(Some(LinkError::ConnectTimedOut {
            address: PEER.to_string(),
            stage: "subscribing for notifications".to_string(),
        }));
        runtime.dial(PEER).await;
        let after = now_millis() + chatter_mesh_link::scheduler::TIMEOUT_DISCOVERY_IGNORE_MS / 2;
        assert!(
            !state.with(|inner| inner.scheduler.should_connect(PEER, STRONG, 0, after)),
            "a peer that timed out is left alone for the whole timeout window"
        );

        // A single ordinary refusal: not enough to ignore it. One failed connect is
        // a peer that walked behind a wall, not a peer to give up on.
        let (mut runtime, radio, state, _timeline) = runtime_parts();
        radio.set_connect_error(Some(LinkError::ConnectFailed {
            address: PEER.to_string(),
            reason: "refused".to_string(),
        }));
        runtime.dial(PEER).await;
        let soon = now_millis() + chatter_mesh_link::scheduler::TIMEOUT_DISCOVERY_IGNORE_MS / 2;
        assert!(
            state.with(|inner| inner.scheduler.should_connect(PEER, STRONG, 0, soon)),
            "one refusal is not a timeout, and must not buy the timeout's silence"
        );
    }

    #[tokio::test]
    async fn the_mock_refuses_an_oversized_frame_like_a_real_radio_would() {
        // Guards the guard. Both real link layers return `PayloadTooLarge` past the
        // negotiated MTU and the mock used to accept anything, so a fragment sized
        // wrongly would pass every test and be discarded on a radio — the failure
        // mode fragmentation bugs actually have.
        //
        // The whole mesh suite now writes against radios that enforce the same
        // number the planner fragments against, and passes. That is only worth
        // something if the enforcement can fail, which is what this checks.
        let (_runtime, radio, state, _timeline) = runtime_parts();
        let link = writable_link_on(&state, &radio, "/mock/tight", 64);

        assert!(
            matches!(
                radio.send(&link, &[0u8; 65]).await,
                Err(chatter_mesh_link::LinkError::PayloadTooLarge { actual: 65, max: 64 })
            ),
            "one byte over the ceiling is refused"
        );
        assert!(
            radio.send(&link, &[0u8; 64]).await.is_ok(),
            "and exactly the ceiling is not"
        );
    }

    #[tokio::test]
    async fn a_dead_link_takes_its_whole_queue_with_it() {
        // Observed on hardware once the drop was logged: the error is "link is no
        // longer connected", and every remaining frame for that link will fail the
        // same way. Draining them one per pacing interval spends 20 ms each to
        // learn that — twenty seconds for one queued chunk — while live links wait
        // their turn behind it.
        let (mut runtime, radio, state, _timeline) = runtime_parts();
        // 512 so the payload below fragments into a queue worth discarding.
        let link = writable_link_on(&state, &radio, "/mock/doomed", 512);
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("doomed.bin");
        let bytes = body(4_000);
        std::fs::write(&source, &bytes).expect("write");

        runtime
            .start_streamed_file("MSG-DEAD", test_manifest([0x3D; 8], &bytes), source)
            .await;
        runtime.pump_streamed_files(now_secs()).await;
        let queued = state.with(|inner| inner.writes.queued_count(&link));
        assert!(queued > 2, "a chunk is many frames, all of them queued");

        // The radio now fails terminally, as a disconnected link does.
        radio.set_links_gone(true);
        runtime.flush_buffered_writes().await;

        assert_eq!(
            state.with(|inner| inner.writes.queued_count(&link)),
            0,
            "one failure is enough to know the rest will fail too"
        );
        assert_eq!(
            radio.write_count(),
            0,
            "and nothing reached the air, since the link is gone"
        );
    }

    #[tokio::test]
    async fn a_relay_never_sends_a_packet_back_to_the_peer_it_came_from() {
        // Split horizon by link is not enough. Two devices can hold two links to
        // each other at once — each having connected outward to the other — and
        // excluding only the ingress link leaves the sibling link to the same peer
        // selected, so the relay sends the packet straight back.
        //
        // Measured on a two-node mesh before this: the sender logged 250 expired
        // reassemblies of its *own* chunks against 14 real ones, roughly seventeen
        // thousand fragments of its own traffic returning to occupy the air its
        // transfer needed.
        let (mut runtime, radio, state, _timeline) = runtime_parts();
        let neighbour = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let identity = register_verified_peer(&state, &neighbour, "twolinks");

        // Both links to the same peer, as two devices that each dialled the other.
        let inbound = writable_link_on(&state, &radio, "/mock/they-dialled-us", 512);
        let sibling = writable_link_on(&state, &radio, "/mock/we-dialled-them", 512);
        state.with(|inner| {
            inner.links.bind_if_unbound(&inbound, &neighbour);
            inner.links.bind_if_unbound(&sibling, &neighbour);
        });

        // A broadcast from that peer, arriving on one of the two links.
        let packet = signed(
            &neighbour,
            &identity,
            MessageType::Message,
            b"worth relaying".to_vec(),
            true,
        );
        runtime.relay(&inbound, &packet, &runtime.identity.peer_id()).await;
        // `relay` spawns the write behind a collision-avoidance delay, so the
        // frames are not queued the instant it returns.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        runtime.drain_writes().await;

        let sent_back: Vec<_> = radio
            .writes()
            .into_iter()
            .filter(|(link, _)| *link == inbound || *link == sibling)
            .collect();
        assert!(
            sent_back.is_empty(),
            "the only links are to the peer that sent it, so the relay has nowhere \
             to go — it wrote {} frame(s) back at the sender",
            sent_back.len()
        );
    }

    #[tokio::test]
    async fn frames_go_out_one_at_a_time_rather_than_all_at_once() {
        // The pacing that makes a large transfer possible, and where it now lives.
        //
        // It used to be a prediction: hold the next *chunk* back by how long its
        // 70 fragments ought to take, from a calibrated rate. That needed a rate
        // to calibrate, and one figure could not fit two radios — the same sender
        // measured ~10.5 kB/s to one phone and ~2.5 kB/s to another.
        //
        // So the spacing moved to the frames themselves, which is where the limit
        // actually is and needs no estimate. `transmit` queues; the drain takes one
        // frame per link per interval. bitchat spaces fragments the same way
        // (`interFragmentDelayMs = 20`).
        let (mut runtime, radio, state, _timeline) = runtime_parts();
        // 512, the real negotiated write size on this stack, so the chunk below
        // actually fragments. A text payload is no good here: `broadcast_public`
        // DEFLATEs, and any string long enough to fragment is compressible enough
        // not to.
        writable_link_on(&state, &radio, "/mock/paced", 512);
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("paced.bin");
        let bytes = body(4_000);
        std::fs::write(&source, &bytes).expect("write");

        let manifest = test_manifest([0x2C; 8], &bytes);
        runtime
            .start_streamed_file("MSG-PACE", manifest.clone(), source)
            .await;
        // The manifest is one frame; the chunk is many. Clear the manifest out so
        // what is counted below is the fragment stream and nothing else.
        runtime.drain_writes().await;
        runtime.pump_streamed_files(now_secs()).await;

        let link = LinkId::new("/mock/paced");
        let queued_frames = state.with(|inner| inner.writes.queued_count(&link));
        assert!(
            queued_frames > 1,
            "a chunk is many frames, and all of them are queued rather than written"
        );
        assert_eq!(
            radio.write_count(),
            1,
            "only the manifest, drained above; the chunk's frames are all still queued"
        );

        // Each turn of the drain puts exactly one frame on the air per link.
        let baseline = radio.write_count();
        for step in 1..=queued_frames {
            runtime.flush_buffered_writes().await;
            assert_eq!(
                radio.write_count(),
                baseline + step,
                "one frame per interval, so the rate is the interval and not a guess"
            );
        }

        // And the queue empties rather than the tail being lost.
        assert!(state.with(|inner| inner.writes.is_empty()));
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
        writable_link_on(&state, &radio, "/mock/send", 4096);
        let mut events = runtime.events.subscribe();

        let manifest = test_manifest([0x17; 8], &bytes);
        runtime
            .start_streamed_file("MSG-S1", manifest.clone(), source.clone())
            .await;
        runtime.drain_writes().await;

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
            runtime.pump_streamed_files(now_secs()).await;
            runtime.drain_writes().await;
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
        runtime.pump_streamed_files(now_secs()).await;
        runtime.drain_writes().await;
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
        writable_link_on(&state, &radio, "/mock/serve", 4096);
        let asker = PeerId::from_str("bbbbbbbbbbbbbbbb");
        let asker_identity = register_verified_peer(&state, &asker, "wanderer");

        let manifest = test_manifest([0x18; 8], &bytes);
        runtime
            .start_streamed_file("MSG-S2", manifest.clone(), source)
            .await;
        runtime.drain_writes().await;
        // Two passes, with the clock moved on between them so the second is not
        // held back by the first chunk's pacing interval.
        runtime.pump_streamed_files(now_secs()).await;
        runtime.drain_writes().await;
        runtime
            .pump_streamed_files(now_secs())
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

        // Everything the first pass queued has to be on the air before the
        // baseline is taken, or `before` indexes into frames that were merely
        // waiting rather than the one the re-request produces.
        runtime.drain_writes().await;
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
        runtime.pump_streamed_files(now_secs()).await;
        runtime.drain_writes().await;
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
        runtime.pump_streamed_files(now_secs()).await;
        runtime.drain_writes().await;
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
        let (mut runtime, radio, state, timeline) = runtime_parts();
        isolate_spool(&mut runtime);
        writable_link_on(&state, &radio, "/mock/echo", 4096);
        let dir = tempfile::tempdir().expect("temp dir");
        let source = dir.path().join("mine.bin");
        let bytes = body(2_048);
        std::fs::write(&source, &bytes).expect("write");

        let manifest = test_manifest([0x19; 8], &bytes);
        runtime
            .start_streamed_file("MSG-S3", manifest.clone(), source)
            .await;
        runtime.drain_writes().await;
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
        writable_link_on(&state, &radio, "/mock/fork", 4096);

        runtime.broadcast_file("MSG-SMALL", &sample_file()).await;

        runtime.drain_writes().await;
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
        runtime.drain_writes().await;
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
