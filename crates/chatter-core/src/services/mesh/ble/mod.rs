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
    BitchatPacket, MessageType, NoisePayloadType, SENDER_ID_LEN, decode_noise_payload,
    encode_noise_payload,
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
    AnnounceDistrust, AnnounceLinkContext, AnnouncePreflightConfig, AnnounceRejection,
    evaluate_announce,
};
use chatter_mesh_link::backpressure::{
    InboundAppend, InboundAppendMetadata, InboundChunk, InboundLimits,
    PENDING_WRITE_BUFFER_CAP_BYTES, WriteEnqueue, should_buffer,
};
use chatter_mesh_link::duty::{self, ScanCommand};
use chatter_mesh_link::guard::{IngressGuard, IngressInput, IngressRejection};
use chatter_mesh_link::planner::{self, DIRECTED_SPOOL_WINDOW_SECS, OutboundPlanRequest};
use chatter_mesh_link::registry::VerifiedDirectAnnounce;
use chatter_mesh_link::scheduler::{
    DiscoveryDecision, ExistingConnectionState, MAX_CENTRAL_LINKS, PeripheralConnectionState,
    QueueDecision,
};
use chatter_mesh_link::{LinkEvent, LinkId, LinkRole, MeshLink};
use chatter_mesh_noise::prekey::PrekeyBundle;
use chatter_mesh_noise::sign::{sign_packet, verify_packet};
use chatter_mesh_noise::{MeshIdentity, NoiseError};
use chatter_mesh_store::archive::GossipMessageArchive;
use chatter_mesh_store::courier::{AnnounceIngress, CourierStore};
use chatter_mesh_store::outbox::AckKind;
use chatter_mesh_store::outbox::Sealer;
use chatter_mesh_store::storage::{Storage, StorageKey};
use chatter_mesh_store::sync::{GossipSyncManager, SyncConfig};
use tokio::sync::mpsc::UnboundedReceiver;

use super::account_binding::BindingV1;
use super::chatter_hello::ChatterHello;
use super::config::MeshConfig;
use super::events::{MeshEvent, MeshMessage};
use super::file_stream::{
    CHUNK_REQUEST_QUIET_SECS, ChunkOutcome, FileStreamSpool, ManifestOutcome, OutboundTransfer,
    TRANSFER_ABANDON_SECS, TransferId,
};
use super::mapping::{self, InboundMessage, MeshConversation, outbound_message};
use super::private_channel::{
    EpochProof, Invite, LeaveNotice, Member, PrivateChannel, PrivateChannelStore,
    SenderDistribution,
};
use super::router::{RouterCommand, RouterHandle};
use super::signal_mesh::{self, BundleFrame};
use super::state::{
    LinkStateInner, MeshState, NostrState, link_candidates_locked, now_millis, now_secs,
};
use super::timeline::MeshEvents;
use super::transport::BleCommand;
use crate::storage::{
    KeyStore,
    mesh_binding_store::{MeshBindingStore, PeerBinding},
};

/// How often the loop wakes to run the parts that nothing else can schedule.
const TICK: Duration = Duration::from_millis(500);
const DM_HANDSHAKE_RETRY_MS: u64 = 5_000;

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
/// Bounds open-channel ingress including fragments on one radio link.
const CHANNEL_INGRESS_PER_MINUTE: u32 = 60;
const CARRIER_INGRESS_PER_MINUTE: u32 = 60;

/// The radio loop.
pub struct BleRuntime {
    binding_store: Arc<MeshBindingStore>,
    private_channels: Arc<PrivateChannelStore>,
    key_store: Arc<KeyStore>,
    signal_lock: Arc<parking_lot::Mutex<()>>,
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
    /// Per-ingress budget for open channels, independent of outbound pacing.
    channel_ingress: std::collections::HashMap<String, (u64, u32)>,
    private_ingress: std::collections::HashMap<String, (u64, u32)>,
    /// Authenticated private authors share one budget across relay links.
    private_authored_ingress: std::collections::HashMap<PeerId, (u64, u32)>,
    carrier_ingress: std::collections::HashMap<String, (u64, u32)>,
    last_channel_presence_ms: u64,
    last_private_rekey_ms: u64,
    /// Connect attempts run off the loop and report back here. Awaiting one
    /// inline froze every radio event, command and tick for up to the twenty
    /// second connect timeout; on the desktop a run of stale private addresses
    /// kept the loop frozen long enough that Noise handshakes never finished.
    dial_tx: tokio::sync::mpsc::UnboundedSender<DialOutcome>,
    dial_rx: Option<tokio::sync::mpsc::UnboundedReceiver<DialOutcome>>,
    /// One connect at a time. BlueZ (and Android's GATT client) hold a single
    /// pending LE connection; a second `Connect` aborts the first with
    /// `le-connection-abort-by-local`. The blocking loop used to serialize this
    /// by accident.
    dial_in_flight: bool,
}

/// The result of one connect attempt, delivered back to the loop.
pub(super) struct DialOutcome {
    address: String,
    started_ms: u64,
    result: Result<LinkId, chatter_mesh_link::LinkError>,
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
        binding_store: Arc<MeshBindingStore>,
        private_channels: Arc<PrivateChannelStore>,
        key_store: Arc<KeyStore>,
        signal_lock: Arc<parking_lot::Mutex<()>>,
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
        let (dial_tx, dial_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            binding_store,
            private_channels,
            key_store,
            signal_lock,
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
            channel_ingress: std::collections::HashMap::new(),
            dial_tx,
            dial_rx: Some(dial_rx),
            dial_in_flight: false,
            private_ingress: std::collections::HashMap::new(),
            private_authored_ingress: std::collections::HashMap::new(),
            carrier_ingress: std::collections::HashMap::new(),
            last_channel_presence_ms: 0,
            last_private_rekey_ms: 0,
        }
    }

    fn restore_pins(&self) -> Result<(), String> {
        if let Some(sealed) = self
            .storage
            .load(StorageKey::PeerPins)
            .map_err(|e| e.to_string())?
        {
            let bytes = self.sealer.open(&sealed).map_err(|e| e.to_string())?;
            self.state
                .with(|state| state.peers.restore_pins(&bytes))
                .map_err(|_| "malformed peer pins".to_string())?;
        }
        Ok(())
    }

    fn restore_prekeys(&self) -> Result<super::prekeys::LocalPrekeys, String> {
        let Some(sealed) = self
            .storage
            .load(StorageKey::LocalPrekeys)
            .map_err(|e| e.to_string())?
        else {
            return Ok(super::prekeys::LocalPrekeys::default());
        };
        let bytes = zeroize::Zeroizing::new(self.sealer.open(&sealed).map_err(|e| e.to_string())?);
        super::prekeys::LocalPrekeys::from_bytes(&bytes)
            .ok_or_else(|| "malformed prekey store".to_string())
    }

    /// Keep our prekey batch topped up, and publish its bundle whenever it
    /// changes: broadcast now, and into the gossip archive so sync carries it to
    /// peers we meet later.
    ///
    /// Persist strictly before publishing. A published prekey whose secret was
    /// never saved is mail that can never be opened.
    async fn maintain_prekeys(&mut self, now_ms: u64) {
        let changed = self.state.with(|state| {
            state
                .prekeys
                .local
                .as_mut()
                .is_some_and(|local| local.refresh(now_ms, &mut crate::crypto::os_rng()))
        });
        if !changed {
            return;
        }
        let Some(bytes) = self.state.with(|state| {
            state
                .prekeys
                .local
                .as_ref()
                .map(super::prekeys::LocalPrekeys::to_bytes)
        }) else {
            return;
        };
        let saved = self
            .sealer
            .seal(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|sealed| {
                self.storage
                    .save(StorageKey::LocalPrekeys, &sealed)
                    .map_err(|e| e.to_string())
            });
        if let Err(error) = saved {
            // Not marked saved, so the next tick retries before anything is
            // published.
            self.degraded(format!("mesh prekeys could not be saved: {error}"));
            return;
        }
        let identity = Arc::clone(&self.identity);
        let bundle = self.state.with(|state| {
            let local = state.prekeys.local.as_mut()?;
            local.mark_saved();
            local.bundle(&identity)
        });
        let Some(Ok(payload)) = bundle.map(|bundle| bundle.encode()) else {
            return;
        };
        let packet = self.build_packet(
            MessageType::PrekeyBundle,
            None,
            payload,
            chatter_mesh::relay::MESSAGE_TTL_DEFAULT,
        );
        let packet = self.maybe_sign(packet, true);
        self.originate_broadcast(&packet).await;
    }

    /// A peer's prekey bundle. Cached and re-gossiped only if it verifies
    /// against the signing key pinned from that owner's verified announce — the
    /// bundle's own signature proves nothing about whose static key it names.
    fn on_prekey_bundle(&mut self, packet: &BitchatPacket) -> bool {
        let Ok(bundle) = PrekeyBundle::decode(&packet.payload) else {
            return false;
        };
        if bundle.owner_static == self.identity.static_public() {
            return false;
        }
        let owner = PeerId::from_noise_key(&bundle.owner_static);
        let now_ms = now_millis();
        self.state.with(|state| {
            let verified = state.peers.pinned(&owner).is_some_and(|pin| {
                pin.noise_public_key.as_slice() == bundle.owner_static.as_slice()
                    && <[u8; 32]>::try_from(pin.signing_public_key.as_slice())
                        .is_ok_and(|key| bundle.verify(&key))
            });
            if verified {
                state.prekeys.peers.ingest(bundle, now_ms);
            }
            verified
        })
    }

    #[cfg(test)]
    fn save_pins(&self) -> Result<(), String> {
        let bytes = self.state.with(|state| state.peers.pins_to_bytes());
        let sealed = self.sealer.seal(&bytes).map_err(|e| e.to_string())?;
        self.storage
            .save(StorageKey::PeerPins, &sealed)
            .map_err(|e| e.to_string())
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
        self.state.with(|state| {
            state.radio_running = true;
            // Discovery backoff is about the previous radio session's links. A
            // duplicate-peer note ignores an address for five minutes, so after
            // a mesh off/on the S25 never dialled the S9 again (5+ min, ui159).
            state.scheduler.reset();
        });
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
        match self.restore_prekeys() {
            Ok(local) => self.state.with(|state| state.prekeys.local = Some(local)),
            // Left `None`: generating a fresh set over secrets we merely could
            // not read would orphan every envelope already sealed to them. Mail
            // to us falls back to the static seal until the next start.
            Err(error) => self.degraded(format!("mesh prekeys could not be restored: {error}")),
        }
        self.maintain_prekeys(now_millis()).await;

        // A crash may have stopped after libsignal wrote its ratchet but before
        // the timeline and dedup marker were both durable. Restore the saved
        // pre-decrypt state, then replay the exact retained carrier.
        if let Ok(Some((owner, _))) = self.key_store.mesh_local_account_device() {
            match self.binding_store.pending_mesh_receive_replays(&owner) {
                Ok(pending) => {
                    for (peer, _id, carrier, timestamp) in pending {
                        self.process_signal_carrier(&carrier, &peer, timestamp, true)
                            .await;
                    }
                }
                Err(error) => {
                    self.degraded(format!("mesh Signal receive recovery failed: {error}"))
                }
            }
        }

        self.announce().await;

        let radio = Arc::clone(&self.radio);
        let mut dials = self
            .dial_rx
            .take()
            .expect("BleRuntime::run is called once per runtime");
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
                Some(outcome) = dials.recv() => self.finish_dial(outcome),
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
            state.chatter_hello.clear_live();
            state.pending_bindings.clear();
        });
        self.emit_status();
    }
}

mod announces;
mod broadcasts;
mod commands;
mod direct_messages;
mod event_reporting;
mod incoming_files;
mod links;
mod maintenance;
mod messages;
mod outgoing_files;
mod secure_ingress;
mod transmission;

/// Preserve a dead link's bounded queue on another writable ACL to the same peer.
/// A missing sibling leaves DMs in the durable router outbox for reconnect retry.
fn rescue_link_writes(
    state: &mut LinkStateInner,
    link: &LinkId,
    failed: Option<chatter_mesh_link::backpressure::PendingWrite>,
) -> (usize, Vec<u8>) {
    let mut writes = state.writes.take_all(link);
    if let Some(failed) = failed {
        writes.insert(0, failed);
    }
    if writes.is_empty() {
        return (0, Vec::new());
    }
    let peer = state.links.peer_for(link).cloned();
    if let Some(peer) = peer {
        let bytes: usize = writes.iter().map(|write| write.bytes.len()).sum();
        if let Some(sibling) = state.links.links_to(&peer).into_iter().find(|candidate| {
            candidate != link
                && state.links.is_writable(candidate)
                && state
                    .max_write
                    .get(candidate)
                    .is_some_and(|max| writes.iter().all(|write| write.bytes.len() <= *max))
                && state.writes.queued_bytes(candidate).saturating_add(bytes)
                    <= PENDING_WRITE_BUFFER_CAP_BYTES
        }) {
            let count = writes.len();
            state.writes.prepend(&sibling, writes);
            return (count, Vec::new());
        }
    }
    (
        0,
        writes
            .iter()
            .filter_map(|write| write.bytes.get(1).copied())
            .collect(),
    )
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
    central
        .into_iter()
        .skip(1)
        // A fragment stream is pinned to the link chosen when it was planned.
        // Closing that link after its first frame has gone out loses the tail if
        // the keeper has a smaller MTU (rescue_link_writes cannot move it).
        // Wait for the queued stream to drain; a later announce prunes the idle
        // duplicate. This also preserves a queued non-fragment frame.
        .filter(|(id, _)| state.writes.queued_count(id) == 0)
        .map(|(id, _)| id.clone())
        .collect()
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
async fn transmit_forwarded(state: &Arc<MeshState>, packet: &BitchatPacket, ingress: &LinkId) {
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
mod tests;
