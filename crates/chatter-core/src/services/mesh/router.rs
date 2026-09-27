//! The store-and-forward actor.
//!
//! # Why this is a thread and not a task
//!
//! [`RouterTransport`](chatter_mesh_store::RouterTransport),
//! [`CourierDirectory`](chatter_mesh_store::router::CourierDirectory),
//! [`DropRelay`] and [`DropSink`] are all declared without a `Send` bound, so
//! [`MessageRouter`] and [`BridgeCourier`] — which hold them as trait objects —
//! are `!Send`. That is deliberate in those crates ("the caller owns it on one
//! task"), and it means the router cannot go behind an `Arc<Mutex<_>>` or onto
//! `tokio::spawn`.
//!
//! So it is confined to one owned thread and reached by message passing. Nothing
//! is lost: every input and output crosses the channel as owned data, the work
//! is CPU plus synchronous SQLCipher access, and confinement removes the
//! re-entrancy those crates refused to have — a `&mut self` router that also
//! calls back into the app.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::thread::JoinHandle;

use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::{NoisePayloadType, decode_noise_payload, encode_noise_payload};
use chatter_mesh::tlv::courier_envelope::{CourierEnvelope, TAG_LEN, epoch_day, recipient_tag};
use chatter_mesh::tlv::private_message::PrivateMessagePacket;
use chatter_mesh_noise::MeshIdentity;
use chatter_mesh_noise::seal::{open_courier, seal_courier};
use chatter_mesh_nostr::identity::NostrIdentity;
use chatter_mesh_store::bridge_drop::{
    AbandonedDrop, BridgeCourier, BridgeView, DedupSnapshot, DepositOutcome, DepositRefusal,
    DropEvent, DropOperationId, DropRelay, DropSink, PublishRequest, PublisherKey,
};
use chatter_mesh_store::outbox::{AckKind, DropReason, LinkState, OutboxStore, Sealer};
use chatter_mesh_store::router::{MessageRouter, RouterEvent, SendOutcome};
use chatter_mesh_store::storage::{Storage, StorageKey};
use chatter_mesh_store::{MESSAGE_TTL_SECS, RouterTransport, StoreError};
use parking_lot::Mutex;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

use super::error::MeshError;
use super::events::{MeshDelivery, MeshEvent, MeshMessage};
use super::mapping::{InboundMessage, MeshConversation};
use super::state::{MeshState, NostrState, now_secs};
use super::timeline::MeshEvents;
use super::transport::{
    BleCommand, BleTransport, MeshDirectory, MeshFavorites, NostrCommand, NostrTransport,
};

/// The bridge courier, shared between the actor and the router's deposit
/// closure. Single-threaded by construction, so `Rc`/`RefCell` rather than a
/// lock — and the two never borrow it at the same time, because the closure only
/// runs inside a router call the actor is not itself inside.
type SharedBridge = Rc<RefCell<BridgeCourier<NostrDropRelay, MeshDropSink>>>;

/// Everything the actor needs, all of it `Send`.
pub struct RouterDeps {
    pub state: Arc<MeshState>,
    pub nostr_state: Arc<NostrState>,
    pub identity: Arc<MeshIdentity>,
    pub storage: Arc<dyn Storage>,
    pub sealer: Arc<dyn Sealer>,
    pub favorites: Arc<MeshFavorites>,
    pub ble: Arc<super::CommandBus<BleCommand>>,
    pub nostr: Arc<super::CommandBus<NostrCommand>>,
    pub events: MeshEvents,
    /// Whether to park sealed copies on relays when no courier is nearby.
    pub bridge_enabled: bool,
}

/// Work for the actor.
pub enum RouterCommand {
    /// Route one private message, and report which branch was taken.
    SendPrivate {
        content: String,
        to: PeerId,
        nickname: String,
        message_id: String,
        reply: oneshot::Sender<SendOutcome>,
    },
    /// A peer became reachable: push anything retained for it, and offer it mail
    /// for others.
    PeerAvailable { peer: PeerId },
    /// An acknowledgement arrived.
    Acked {
        message_id: String,
        peer: PeerId,
        kind: AckKind,
    },
    /// Periodic expiry, retry, and bridge refresh.
    Maintenance,
    /// A relay reported the result of a courier drop publish.
    ResolveDrop {
        operation: DropOperationId,
        accepted: bool,
    },
    /// A courier drop addressed to a tag we watch arrived.
    InboundDrop {
        event_id: String,
        tag: [u8; TAG_LEN],
        envelope: Vec<u8>,
    },
    /// Forget everything retained. Panic wipe.
    Wipe,
    /// Stop the actor.
    Stop,
}

/// Handle onto the actor thread.
///
/// The channel is a tokio one even though the far end is a plain thread: a
/// `tokio` sender is `Send + Sync` so this handle can live in an `Arc` shared by
/// every task, and `blocking_recv` is exactly the blocking take the actor loop
/// wants. `std::sync::mpsc::Sender`'s `Sync` status has changed across releases,
/// and this is not the place to depend on which.
pub struct RouterHandle {
    commands: UnboundedSender<RouterCommand>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl RouterHandle {
    /// Start the actor.
    #[must_use]
    pub fn spawn(deps: RouterDeps) -> Self {
        let (commands, inbox) = unbounded_channel();
        let thread = std::thread::Builder::new()
            .name("chatter-mesh-router".to_string())
            .spawn(move || Actor::new(deps).run(inbox))
            .ok();
        Self {
            commands,
            thread: Mutex::new(thread),
        }
    }

    /// Submit work. Fails only once the actor is gone.
    pub fn send(&self, command: RouterCommand) -> Result<(), MeshError> {
        self.commands
            .send(command)
            .map_err(|_| MeshError::WorkerGone)
    }

    /// Route a private message and wait for the routing decision.
    pub async fn send_private(
        &self,
        content: String,
        to: PeerId,
        nickname: String,
        message_id: String,
    ) -> Result<SendOutcome, MeshError> {
        let (reply, answer) = oneshot::channel();
        self.send(RouterCommand::SendPrivate {
            content,
            to,
            nickname,
            message_id,
            reply,
        })?;
        answer.await.map_err(|_| MeshError::WorkerGone)
    }

    /// Stop the actor and wait for it, so shutdown leaves no thread behind.
    pub fn shutdown(&self) {
        let _ = self.commands.send(RouterCommand::Stop);
        if let Some(handle) = self.thread.lock().take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RouterHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// The relay and sink the bridge courier drives
// ---------------------------------------------------------------------------

/// Publishes sealed drops onto Nostr relays.
struct NostrDropRelay {
    state: Arc<NostrState>,
    commands: Arc<super::CommandBus<NostrCommand>>,
}

impl DropRelay for NostrDropRelay {
    fn is_connected(&self) -> bool {
        self.state.any_relay_connected()
    }

    fn fresh_publisher_key(&self) -> PublisherKey {
        // A throwaway key per drop. Publishing every drop under one key would
        // let a relay operator join all of this device's drops into one sender,
        // which is the exact linkage the bridge exists to avoid.
        //
        // A uniform 32-byte value falls outside the secp256k1 scalar range with
        // probability below 2⁻¹²⁷. The loop is bounded so a broken RNG cannot
        // hang the actor; its last draw is still a fresh key.
        let mut bytes = [0u8; 32];
        for _ in 0..16 {
            rand::rng().fill_bytes(&mut bytes);
            if NostrIdentity::from_secret(bytes).is_ok() {
                break;
            }
        }
        PublisherKey::from_bytes(bytes)
    }

    fn publish(&self, request: PublishRequest<'_>) {
        let _ = self.commands.send(NostrCommand::PublishDrop {
            operation: request.operation,
            tag: request.recipient_tag,
            sealed: request.sealed_envelope.to_vec(),
            expires_at_unix_secs: request.expires_at_unix_secs,
            publisher_secret: request.publisher_key.into_bytes(),
        });
    }

    fn open_subscription(&self, tags: &[[u8; TAG_LEN]]) {
        let _ = self.commands.send(NostrCommand::WatchTags(tags.to_vec()));
    }

    fn close_subscription(&self) {
        let _ = self.commands.send(NostrCommand::CloseWatch);
    }
}

/// Where an inbound drop goes once it has been recognised.
struct MeshDropSink {
    identity: Arc<MeshIdentity>,
    state: Arc<MeshState>,
    ble: Arc<super::CommandBus<BleCommand>>,
    events: MeshEvents,
}

impl MeshDropSink {
    /// Open a sealed envelope addressed to us and turn it into a timeline row.
    fn open(&self, envelope: &CourierEnvelope) -> Option<MeshMessage> {
        let (plaintext, sender_key) = open_courier(&envelope.ciphertext, &self.identity).ok()?;
        let (payload_type, body) = decode_noise_payload(&plaintext)?;
        if payload_type != NoisePayloadType::PrivateMessage {
            return None;
        }
        let message = PrivateMessagePacket::decode(body).ok()?;
        let sender = PeerId::from_noise_key(&sender_key);
        let nickname = self
            .state
            .with(|state| state.peers.nickname(&sender, false).map(str::to_string))
            .unwrap_or_else(|| sender.as_str().to_string());

        let inbound = InboundMessage {
            wire_id: message.message_id,
            conversation: MeshConversation::Dm(sender.clone()),
            sender: sender.clone(),
            nickname: nickname.clone(),
            content: message.content,
            // A drop carries no trustworthy send time: the only timestamp on it
            // is the envelope's expiry, which is the sender's deadline rather
            // than their clock. Stamping arrival is honest and keeps the
            // timeline monotonic.
            timestamp_ms: now_secs().saturating_mul(1_000),
        };
        Some(MeshMessage {
            message: inbound.to_domain(),
            conversation: inbound.conversation,
            sender,
            nickname,
            is_local: false,
            // The seal authenticates the sender's static key, but nothing here
            // checked an announce signature.
            sender_verified: false,
        })
    }
}

impl DropSink for MeshDropSink {
    fn open_for_self(&self, envelope: &CourierEnvelope) -> bool {
        match self.open(envelope) {
            Some(message) => {
                self.events.send(MeshEvent::Message(Box::new(message)));
                true
            }
            // Not ours, or malformed. Consuming the event ID for something we
            // cannot open would hide it from a later attempt that could.
            None => false,
        }
    }

    fn deliver_to_peer(&self, envelope: &CourierEnvelope, peer: &PeerId) -> bool {
        if !self.state.can_write_to(peer) {
            return false;
        }
        self.ble
            .send(BleCommand::Courier {
                peer: peer.clone(),
                envelope: envelope.clone(),
            })
            .is_ok()
    }
}

// ---------------------------------------------------------------------------
// Dedup persistence
// ---------------------------------------------------------------------------

/// The bridge's dedup sets, on disk.
///
/// This **must** survive a restart. bitchat recorded a field-verified
/// amplification storm from holding it in memory only: after a relaunch the
/// device no longer recognises its own drops, republishes them, and every
/// gateway watching the tag does the same.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DedupBlob {
    published: Vec<(String, u64)>,
    seen: Vec<(String, u64)>,
}

fn load_dedup(storage: &dyn Storage, sealer: &dyn Sealer) -> Result<DedupSnapshot, StoreError> {
    let Some(sealed) = storage.load(StorageKey::BridgeDropDedup)? else {
        return Ok(DedupSnapshot {
            published_drop_keys: Vec::new(),
            seen_drop_event_ids: Vec::new(),
        });
    };
    let plaintext = sealer.open(&sealed)?;
    let blob: DedupBlob = bincode::deserialize(&plaintext)
        .map_err(|_| StoreError::Malformed("bridge drop dedup"))?;
    Ok(DedupSnapshot {
        published_drop_keys: blob.published,
        seen_drop_event_ids: blob.seen,
    })
}

fn save_dedup(
    storage: &dyn Storage,
    sealer: &dyn Sealer,
    snapshot: &DedupSnapshot,
) -> Result<(), StoreError> {
    // Sealed, not written in the clear: this is a set of recipient tags and
    // event IDs, i.e. metadata about who this device is messaging and when.
    let blob = DedupBlob {
        published: snapshot.published_drop_keys.clone(),
        seen: snapshot.seen_drop_event_ids.clone(),
    };
    let plaintext =
        bincode::serialize(&blob).map_err(|_| StoreError::Malformed("bridge drop dedup"))?;
    let sealed = sealer.seal(&plaintext)?;
    storage.save(StorageKey::BridgeDropDedup, &sealed)
}

// ---------------------------------------------------------------------------
// The actor
// ---------------------------------------------------------------------------

struct Actor {
    router: MessageRouter,
    bridge: SharedBridge,
    /// Drops evicted from the bridge's pending queue while the router held its
    /// own borrow. Drained after every command.
    abandoned: Rc<RefCell<Vec<AbandonedDrop>>>,
    identity: Arc<MeshIdentity>,
    storage: Arc<dyn Storage>,
    sealer: Arc<dyn Sealer>,
    events: MeshEvents,
}

impl Actor {
    fn new(deps: RouterDeps) -> Self {
        let RouterDeps {
            state,
            nostr_state,
            identity,
            storage,
            sealer,
            favorites,
            ble,
            nostr,
            events,
            bridge_enabled,
        } = deps;

        let outbox = OutboxStore::new(Arc::clone(&storage), Arc::clone(&sealer));
        let directory = MeshDirectory::new(Arc::clone(&state), favorites);

        // Order matters. The router takes the *first* transport reporting a link
        // binding, and only the radio ever has one; the reachable fallback is
        // also first-match, so a peer in radio range is answered over the radio
        // rather than over the internet.
        let transports: Vec<Box<dyn RouterTransport>> = vec![
            Box::new(BleTransport::new(
                Arc::clone(&state),
                Arc::clone(&identity),
                ble.clone(),
            )),
            Box::new(NostrTransport::new(Arc::clone(&nostr_state), nostr.clone())),
        ];

        let mut router = MessageRouter::new(transports, Box::new(directory), outbox);

        let mut courier = BridgeCourier::new(
            NostrDropRelay {
                state: nostr_state,
                commands: nostr,
            },
            MeshDropSink {
                identity: Arc::clone(&identity),
                state,
                ble,
                events: events.clone(),
            },
        );
        courier.set_bridge_enabled(bridge_enabled);
        // Not a gateway: this device publishes and collects its own mail and
        // never republishes other people's.
        courier.set_gateway_enabled(false);
        let bridge: SharedBridge = Rc::new(RefCell::new(courier));
        let abandoned: Rc<RefCell<Vec<AbandonedDrop>>> = Rc::new(RefCell::new(Vec::new()));

        let deposit_identity = Arc::clone(&identity);
        let deposit_bridge = Rc::clone(&bridge);
        let deposit_abandoned = Rc::clone(&abandoned);
        router.set_bridge_deposit(Box::new(move |content, message_id, recipient_key| {
            let now = now_secs();
            let Some(envelope) =
                seal_for_drop(&deposit_identity, content, message_id, recipient_key, now)
            else {
                return false;
            };
            let deposited = deposit_bridge
                .borrow_mut()
                .deposit(message_id, &envelope, now);
            deposit_abandoned
                .borrow_mut()
                .extend(deposited.abandoned.iter().cloned());

            // The contract is strict: `true` means a relay has *accepted* the
            // event, not that we handed it to the relay layer. `Publishing`
            // is only the former half — acceptance arrives later as
            // `resolve_publish`, which is where the carried signal is emitted.
            // The one case that already satisfies the contract is a deposit
            // refused because a relay accepted this message ID earlier.
            matches!(
                deposited.outcome,
                DepositOutcome::Refused(DepositRefusal::AlreadyPublished)
            )
        }));

        let mut actor = Self {
            router,
            bridge,
            abandoned,
            identity,
            storage,
            sealer,
            events,
        };
        actor.restore();
        actor
    }

    fn restore(&mut self) {
        match load_dedup(self.storage.as_ref(), self.sealer.as_ref()) {
            Ok(snapshot) => self.bridge.borrow_mut().restore_dedup(&snapshot, now_secs()),
            Err(error) => {
                // A locked keyring is not an empty set. Starting empty would
                // republish every drop this device has ever sent, which is the
                // amplification storm the persistence exists to prevent.
                self.events.send(MeshEvent::Degraded(format!(
                    "bridge drop history unreadable; the relay bridge stays off: {error}"
                )));
                self.bridge.borrow_mut().set_bridge_enabled(false);
            }
        }
        let events = self.router.recover_outbox(now_secs());
        self.publish(events);
    }

    fn run(mut self, mut inbox: UnboundedReceiver<RouterCommand>) {
        while let Some(command) = inbox.blocking_recv() {
            if matches!(command, RouterCommand::Stop) {
                break;
            }
            self.handle(command);
            self.drain_abandoned();
        }
        // Persist the dedup sets on the way out: losing them costs an
        // amplification storm on the next launch, which is worse than the write.
        let snapshot = self.bridge.borrow().dedup_snapshot();
        let _ = save_dedup(self.storage.as_ref(), self.sealer.as_ref(), &snapshot);
    }

    fn handle(&mut self, command: RouterCommand) {
        let now = now_secs();
        match command {
            RouterCommand::Stop => {}
            RouterCommand::SendPrivate {
                content,
                to,
                nickname,
                message_id,
                reply,
            } => {
                let outcome = self
                    .router
                    .send_private(&content, &to, &nickname, &message_id, now);
                self.emit_link_state(&message_id, &to, outcome.link);
                self.publish(outcome.events.clone());
                self.persist_dedup();
                let _ = reply.send(outcome);
            }
            RouterCommand::PeerAvailable { peer } => {
                let flushed = self.router.flush_outbox(&peer, now);
                self.publish(flushed);
                let carried = self.router.courier_became_available(&peer, now);
                self.publish(carried);
            }
            RouterCommand::Acked {
                message_id,
                peer,
                kind,
            } => {
                let (outcome, events) = self.router.mark_acked(&peer, &message_id, kind);
                self.publish(events);
                let state = match outcome.strongest {
                    AckKind::Delivered => MeshDelivery::Delivered,
                    AckKind::Read => MeshDelivery::Read,
                };
                self.events.send(MeshEvent::Delivery {
                    message_id,
                    peer: Some(peer),
                    state,
                });
            }
            RouterCommand::Maintenance => {
                let expired = self.router.cleanup_expired(now);
                self.publish(expired);
                let retried = self.router.retry_bridge_deposits(now);
                self.publish(retried);

                let mut bridge = self.bridge.borrow_mut();
                let flushed = bridge.flush_pending(now);
                self.abandoned.borrow_mut().extend(flushed);
                if now >= bridge.next_refresh_due() {
                    let key = self.identity.static_public();
                    // Sender-only: this device watches its own recipient tags
                    // and never other people's, because it is not a gateway.
                    let evicted = bridge.refresh(BridgeView::sender_only(Some(key)), now);
                    self.abandoned.borrow_mut().extend(evicted);
                }
                drop(bridge);
                self.persist_dedup();
            }
            RouterCommand::ResolveDrop {
                operation,
                accepted,
            } => {
                let resolved = self
                    .bridge
                    .borrow_mut()
                    .resolve_publish(operation, accepted, now);
                if let Some(resolved) = resolved
                    && resolved.accepted
                    && let Some(message_id) = resolved.message_id
                {
                    self.events.send(MeshEvent::Delivery {
                        message_id,
                        peer: None,
                        state: MeshDelivery::Carried,
                    });
                }
                self.persist_dedup();
            }
            RouterCommand::InboundDrop {
                event_id,
                tag,
                envelope,
            } => {
                let event = DropEvent {
                    event_id: &event_id,
                    tag,
                    envelope_bytes: &envelope,
                };
                let _ = self.bridge.borrow_mut().handle_drop(&event, now);
                self.persist_dedup();
            }
            RouterCommand::Wipe => {
                let evicted = self.bridge.borrow_mut().wipe();
                self.abandoned.borrow_mut().extend(evicted);
                if let Err(error) = self.router.wipe_outbox() {
                    self.events
                        .send(MeshEvent::Degraded(format!("outbox wipe failed: {error}")));
                }
                // Mail cleanup does not reset identity trust. Panic wipe
                // destroys PeerPins explicitly in MeshService.
                for key in StorageKey::ALL {
                    if *key == StorageKey::PeerPins { continue; }
                    if let Err(error) = self.storage.wipe(*key) {
                        self.events.send(MeshEvent::Degraded(format!(
                            "mesh storage wipe failed: {error}"
                        )));
                    }
                }
            }
        }
    }

    /// Report every drop that ended without a relay result. Their mail is still
    /// in the outbox, so this is a status change rather than a loss.
    fn drain_abandoned(&self) {
        let drops: Vec<AbandonedDrop> = self.abandoned.borrow_mut().drain(..).collect();
        for drop in drops {
            if let Some(message_id) = drop.message_id {
                self.events.send(MeshEvent::Delivery {
                    message_id,
                    peer: None,
                    state: MeshDelivery::Unreachable,
                });
            }
        }
    }

    fn persist_dedup(&self) {
        let snapshot = self.bridge.borrow().dedup_snapshot();
        if let Err(error) = save_dedup(self.storage.as_ref(), self.sealer.as_ref(), &snapshot) {
            self.events.send(MeshEvent::Degraded(format!(
                "bridge drop history could not be persisted: {error}"
            )));
        }
    }

    fn emit_link_state(&self, message_id: &str, peer: &PeerId, link: LinkState) {
        let state = match link {
            LinkState::Secure => MeshDelivery::Secure,
            LinkState::Insecure => MeshDelivery::Insecure,
            LinkState::Reachable => MeshDelivery::Reachable,
            LinkState::Unreachable => MeshDelivery::Unreachable,
        };
        self.events.send(MeshEvent::Delivery {
            message_id: message_id.to_string(),
            peer: Some(peer.clone()),
            state,
        });
    }

    fn publish(&self, events: Vec<RouterEvent>) {
        for event in events {
            let mapped = match event {
                RouterEvent::Carried { message_id, peer } => MeshEvent::Delivery {
                    message_id,
                    peer: Some(peer),
                    state: MeshDelivery::Carried,
                },
                RouterEvent::Dropped(dropped) => {
                    // Every drop has to surface: a message that leaves the
                    // outbox without an ack and without a visible failure sits
                    // on "sending" forever.
                    let reason = match dropped.reason {
                        DropReason::Expired => "expired",
                        DropReason::AttemptsExhausted => "no route after repeated attempts",
                        DropReason::QueueOverflow => "outbox full",
                    };
                    self.events.send(MeshEvent::Degraded(format!(
                        "message {} to {} dropped: {reason}",
                        dropped.message_id, dropped.peer
                    )));
                    MeshEvent::Delivery {
                        message_id: dropped.message_id,
                        peer: Some(dropped.peer),
                        state: MeshDelivery::Dropped,
                    }
                }
                RouterEvent::StorageFailed { error } => MeshEvent::Degraded(format!(
                    "retained mail could not reach durable storage: {error}"
                )),
            };
            self.events.send(mapped);
        }
    }
}

/// Seal a message for a relay drop.
///
/// One copy: a relay drop is not a spray-and-wait courier, and a copy budget
/// above one would only invite the gateway layer to fan it out further.
fn seal_for_drop(
    identity: &MeshIdentity,
    content: &str,
    message_id: &str,
    recipient_key: &[u8; 32],
    now_unix_secs: u64,
) -> Option<CourierEnvelope> {
    let inner = PrivateMessagePacket::new(message_id.to_string(), content.to_string())
        .encode()
        .ok()?;
    let payload = encode_noise_payload(NoisePayloadType::PrivateMessage, &inner);
    let ciphertext = seal_courier(&payload, recipient_key, identity).ok()?;
    let expiry_ms = now_unix_secs
        .saturating_add(MESSAGE_TTL_SECS)
        .saturating_mul(1_000);
    let envelope = CourierEnvelope::new(
        recipient_tag(recipient_key, epoch_day(now_unix_secs)),
        expiry_ms,
        ciphertext,
        1,
        None,
    );
    envelope.encode().ok()?;
    Some(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh_store::MemoryStorage;
    use rand_core_06::OsRng;

    /// Not encryption — just enough to prove the dedup blob round trips through
    /// the seal seam without pulling the real keyring in.
    struct XorSealer;

    impl Sealer for XorSealer {
        fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
            Ok(plaintext.iter().map(|b| b ^ 0x5a).collect())
        }

        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, StoreError> {
            Ok(sealed.iter().map(|b| b ^ 0x5a).collect())
        }
    }

    #[test]
    fn an_absent_dedup_blob_is_an_empty_set_not_an_error() {
        let storage = MemoryStorage::new();
        let snapshot = load_dedup(&storage, &XorSealer).expect("absent is fine");
        assert!(snapshot.published_drop_keys.is_empty());
        assert!(snapshot.seen_drop_event_ids.is_empty());
    }

    #[test]
    fn the_dedup_blob_round_trips_and_is_never_written_in_the_clear() {
        let storage = MemoryStorage::new();
        let snapshot = DedupSnapshot {
            published_drop_keys: vec![("msg-1".to_string(), 10)],
            seen_drop_event_ids: vec![("event-a".to_string(), 20)],
        };
        save_dedup(&storage, &XorSealer, &snapshot).expect("save");

        let raw = storage
            .load(StorageKey::BridgeDropDedup)
            .expect("load")
            .expect("present");
        assert!(
            !raw.windows(5).any(|w| w == b"msg-1"),
            "recipient metadata must be sealed before it reaches storage"
        );

        let restored = load_dedup(&storage, &XorSealer).expect("load");
        assert_eq!(restored.published_drop_keys, snapshot.published_drop_keys);
        assert_eq!(restored.seen_drop_event_ids, snapshot.seen_drop_event_ids);
    }

    #[test]
    fn a_drop_envelope_carries_only_ciphertext_and_opens_for_its_recipient() {
        let identity = MeshIdentity::generate(&mut OsRng);
        let recipient = MeshIdentity::generate(&mut OsRng);
        let envelope = seal_for_drop(
            &identity,
            "meet at the bridge",
            "m1",
            &recipient.static_public(),
            1_700_000_000,
        )
        .expect("sealed");

        let bytes = envelope.encode().expect("encodable");
        assert!(
            !bytes
                .windows(b"meet at the bridge".len())
                .any(|w| w == b"meet at the bridge")
        );

        let (plaintext, sender_key) =
            open_courier(&envelope.ciphertext, &recipient).expect("opened");
        assert_eq!(sender_key, identity.static_public());
        let (kind, body) = decode_noise_payload(&plaintext).expect("payload");
        assert_eq!(kind, NoisePayloadType::PrivateMessage);
        let message = PrivateMessagePacket::decode(body).expect("decoded");
        assert_eq!(message.content, "meet at the bridge");
        assert_eq!(message.message_id, "m1");
    }

    #[test]
    fn a_stranger_cannot_open_a_drop() {
        let identity = MeshIdentity::generate(&mut OsRng);
        let recipient = MeshIdentity::generate(&mut OsRng);
        let stranger = MeshIdentity::generate(&mut OsRng);
        let envelope = seal_for_drop(
            &identity,
            "secret",
            "m1",
            &recipient.static_public(),
            1_700_000_000,
        )
        .expect("sealed");
        assert!(open_courier(&envelope.ciphertext, &stranger).is_err());
    }

    #[test]
    fn the_recipient_tag_rotates_daily_so_a_watcher_cannot_track_a_recipient() {
        let key = [3u8; 32];
        let day = 86_400;
        let today = recipient_tag(&key, epoch_day(1_700_000_000));
        let tomorrow = recipient_tag(&key, epoch_day(1_700_000_000 + day));
        assert_ne!(today, tomorrow);
    }
}
