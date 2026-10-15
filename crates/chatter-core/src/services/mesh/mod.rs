//! The bitchat mesh, wired into chatter.
//!
//! Seven crates sit underneath this one and none of them reference each other's
//! IO: the wire format parses, the link policy decides, the Noise layer seals,
//! the store retains, and the Nostr layer builds events — all of it sans-IO,
//! owning no clock, no task, and no socket. [`MeshService`] is the composition
//! root that gives them one.
//!
//! ```text
//!                     MeshService  (this module)
//!        ┌──────────────────┼───────────────────────┐
//!   BleRuntime         RouterHandle            NostrRuntime
//!   (radio task)       (store thread)          (relay task)
//!        │                  │                       │
//!   MeshLink          MessageRouter            RelayPool
//!   BlueZ / JNI       OutboxStore              RelayConnector
//!   link policy       CourierStore             (Tor)
//!   Noise sessions    BridgeCourier
//! ```
//!
//! # Three invariants worth stating once
//!
//! - **No server, ever.** A mesh conversation makes no RPC. Anything in here
//!   reaching for `GrpcClientPool` would be a design error, not a missing
//!   feature — [`ConversationType::is_mesh`] exists so that code keyed on "the
//!   server knows about this conversation" excludes these rows.
//!   [`ConversationType::is_mesh`]: crate::domain::message::ConversationType::is_mesh
//! - **The mesh identity is not the Signal identity.** Separate keypair,
//!   separate table, no derivation between them. See
//!   [`MeshIdentityStore`](crate::storage::mesh_identity_store::MeshIdentityStore).
//! - **Decisions live here.** The UI crate renders [`MeshEvent`]s and calls the
//!   methods below; it filters nothing and derives nothing.

pub mod account_binding;
pub mod ble;
pub mod chatter_hello;
pub mod config;
pub mod error;
pub mod events;
pub mod file_stream;
pub mod identity;
pub mod mapping;
#[cfg(test)]
pub(crate) mod mock;
pub mod nostr;
pub mod prekeys;
pub mod radio;
pub mod router;
pub mod session;
pub mod state;
pub mod timeline;
#[cfg(feature = "onion")]
pub mod tor_relay;
pub mod transport;
// Gated with `tor_relay`: the WebSocket adapter now sits on tokio-tungstenite,
// which is an `onion`-feature dependency, and a build with no relay connector has
// nothing to frame anyway.
#[cfg(feature = "onion")]
pub mod ws;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chatter_mesh::ids::PeerId;
use chatter_mesh::packet::NoisePayloadType;
use chatter_mesh::tlv::file::BitchatFilePacket;
use chatter_mesh::tlv::file_stream::{MAX_SINGLE_PACKET_FILE_BYTES, MAX_STREAMED_FILE_BYTES};
use chatter_mesh_link::MeshLink;
use chatter_mesh_noise::MeshIdentity;
use chatter_mesh_nostr::relay::{RelayConnector, RelayPool};
use chatter_mesh_store::outbox::{LinkState, Sealer};
use chatter_mesh_store::storage::Storage;
use parking_lot::Mutex;
use tokio::sync::broadcast;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::domain::UnifiedConversation;
use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::Message;
use crate::storage::KeyStore;
use crate::storage::mesh_binding_store::{BindingMethod, MeshBindingStore, PeerBinding};
use crate::storage::mesh_identity_store::MeshIdentityStore;
use crate::storage::repositories::MessageRepository;
use account_binding::{BindingQr, BindingV1, PROOF_TTL_SECS};

pub use config::MeshConfig;
pub use error::MeshError;
pub use events::{MeshDelivery, MeshEvent, MeshMessage, MeshPeer, MeshStatus, RadioDown};
pub use mapping::MeshConversation;
pub use timeline::{MeshEvents, MeshTimeline};

use ble::BleRuntime;
use identity::NostrIdentities;
use nostr::NostrRuntime;
use router::{RouterCommand, RouterDeps, RouterHandle};
use state::{MeshState, NostrState, now_millis, now_secs};
use transport::{BleCommand, NostrCommand};

/// How many events the broadcast channel buffers before a slow subscriber
/// starts missing them.
///
/// Generous: a mesh in a crowded room produces bursts, and a UI that was busy
/// rendering should not lose messages because of it.
const EVENT_CAPACITY: usize = 512;

/// Everything [`MeshService`] needs from the rest of the client.
pub struct MeshDeps {
    /// Account key access is used only after a user selects a peer to share with.
    pub key_store: Arc<KeyStore>,
    pub binding_store: Arc<MeshBindingStore>,
    /// The mesh keypair store. Owns the only long-term secret this service has.
    pub identity_store: Arc<MeshIdentityStore>,
    /// Durable, already-sealed storage for the store-and-forward stack.
    pub storage: Arc<dyn Storage>,
    /// The unified message store. Mesh messages are rows in it like every other
    /// conversation type — see [`MeshTimeline::record`] for why they are stored
    /// at all, which is where this client parts company with bitchat.
    pub messages: Arc<MessageRepository>,
    /// Durable directory of open mesh conversations. Without it a DM's history
    /// survives a restart while its sidebar entry does not, which looks like
    /// lost data.
    pub conversations: Arc<crate::storage::repositories::MeshConversationRepository>,
    /// Durable delivery state for messages this device sent. Without it every
    /// mesh delivery state is live-only, so a message that was never delivered
    /// comes back after a reopen looking sent.
    pub deliveries: Arc<crate::storage::repositories::MeshDeliveryRepository>,
    /// Where a file that arrived over the radio lands. The same table every
    /// other conversation type's attachments live in — a mesh file is not a
    /// second kind of attachment, it simply never had a CDN behind it.
    pub attachments: Arc<crate::storage::repositories::AttachmentRepository>,
    /// Seals outbox blobs before they reach storage. The key never enters the
    /// store crates, which is what makes "nothing plaintext reaches disk"
    /// structural rather than a matter of remembering.
    pub sealer: Arc<dyn Sealer>,
    /// How a Nostr relay is reached. `None` disables the relay half entirely:
    /// geohash channels and gift-wrapped DMs go quiet, the mesh still works over
    /// the radio, and `can_deliver_promptly` correctly reports false.
    pub relay_connector: Option<Arc<dyn RelayConnector>>,
    /// Overrides the platform radio. `None` selects
    /// [`radio::platform_radio`], which is what production does; tests pass a
    /// double.
    /// Build a radio, instead of asking the platform for one. Tests only.
    ///
    /// A factory rather than an instance because `start` needs a *fresh* radio
    /// every time: stopping one is terminal on both real link layers. A test
    /// holding one instance cannot tell a service that reuses a dead radio from
    /// one that does not, which is exactly the gap that let a broken mesh switch
    /// reach a phone.
    pub radio_factory: Option<Arc<dyn Fn() -> Arc<dyn MeshLink> + Send + Sync>>,
}

/// Receivers parked between construction and `start`.
/// A command channel to a runtime, whose receiving end can be replaced.
///
/// The mesh can be switched off and on again, and `start` spawns fresh runtimes
/// each time. Their receivers therefore have to be fresh too — a runtime that
/// exited took its receiver with it, and the senders held by the service *and by
/// the router* would otherwise point at a channel nobody reads. That was the
/// shape before this existed: `start` consumed a one-shot `Pending`, so a second
/// call returned `Ok(())` having started nothing, and the mesh could be stopped
/// but never resumed.
///
/// Swapping the sender behind a lock keeps every holder correct without any of
/// them knowing a restart happened. Deliberately not a broadcast or a rebuild of
/// the router: the runtime loops and their shutdown handling are the part that is
/// hard to get right, and this leaves them untouched.
pub struct CommandBus<T> {
    tx: Mutex<UnboundedSender<T>>,
}

impl<T> CommandBus<T> {
    /// A bus and the receiver for its first channel.
    fn new() -> (Arc<Self>, UnboundedReceiver<T>) {
        let (tx, rx) = unbounded_channel();
        (Arc::new(Self { tx: Mutex::new(tx) }), rx)
    }

    /// Replace the channel, returning the receiver for the new one.
    ///
    /// Anything still queued on the old channel is dropped with it. That is
    /// correct: those commands were addressed to a runtime that has stopped, and
    /// carrying them across a restart would replay work from a session the user
    /// ended.
    fn rebind(&self) -> UnboundedReceiver<T> {
        let (tx, rx) = unbounded_channel();
        *self.tx.lock() = tx;
        rx
    }

    /// Queue a command for whichever runtime is current.
    pub fn send(&self, command: T) -> Result<(), SendError<T>> {
        self.tx.lock().send(command)
    }
}

/// The mesh, composed.
pub struct MeshService {
    key_store: Arc<KeyStore>,
    binding_store: Arc<MeshBindingStore>,
    identity: Arc<MeshIdentity>,
    /// Retained so a panic wipe can destroy the keypair, not merely the mail.
    identity_store: Arc<MeshIdentityStore>,
    config: Arc<MeshConfig>,
    state: Arc<MeshState>,
    nostr_state: Arc<NostrState>,
    storage: Arc<dyn Storage>,
    sealer: Arc<dyn Sealer>,
    router: Arc<RouterHandle>,
    ble: Arc<CommandBus<BleCommand>>,
    nostr: Arc<CommandBus<NostrCommand>>,
    events: MeshEvents,
    /// The message store side of the mesh. Everything the service surfaces has
    /// already been through it by the time a subscriber sees it.
    timeline: Arc<MeshTimeline>,
    /// A fixed radio to use instead of the platform's, for tests.
    ///
    /// Production leaves this `None` and [`start`](Self::start) builds a fresh one
    /// each time, because a stopped link layer cannot be restarted: `AndroidLink`
    /// refuses with "this link layer has been stopped; construct a new AndroidLink
    /// to restart". Holding one radio for the life of the service made the mesh
    /// switchable off and then never back on — the service restarted, the runtime
    /// spawned, and the radio it was handed declined.
    ///
    /// Found on hardware rather than in a test, because `MockLink::start` always
    /// succeeds and latches no stopped state, so the mock was more forgiving than
    /// either real link layer.
    radio_factory: Option<Arc<dyn Fn() -> Arc<dyn MeshLink> + Send + Sync>>,
    /// Whether this platform has a radio at all, which is a different question
    /// from whether one is running.
    has_radio: bool,
    relay_connector: Option<Arc<dyn RelayConnector>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    running: AtomicBool,
    /// Set when the sealer holds no key — see [`MeshError::SealerUnavailable`].
    sealer_unavailable: Option<String>,
}

impl MeshService {
    /// The account identity is disclosed only to this selected, live Noise peer.
    /// Presence alone never calls this method.
    pub fn share_identity(
        &self,
        peer: &PeerId,
        account_id: UserId,
        device_id: u32,
    ) -> Result<(), MeshError> {
        use libsignal_protocol::IdentityKeyPair;
        let (mesh_static, transcript, sender_nonce, recipient_nonce) = self.state.with(|s| {
            (
                s.sessions.remote_static(peer),
                s.sessions.handshake_hash(peer),
                s.chatter_hello.local_nonce(peer),
                s.chatter_hello.get(peer).map(|h| h.nonce),
            )
        });
        let (Some(_), Some(handshake_hash), Some(sender_nonce), Some(recipient_nonce)) =
            (mesh_static, transcript, sender_nonce, recipient_nonce)
        else {
            return Err(MeshError::Binding(
                "wait for an encrypted Chatter peer connection".into(),
            ));
        };
        if device_id == 0 {
            return Err(MeshError::Binding("no account device is active".into()));
        }
        let (_, bytes) = self
            .key_store
            .get_local_identity()?
            .ok_or_else(|| MeshError::Binding("no account identity is available".into()))?;
        let bytes = zeroize::Zeroizing::new(bytes);
        let keypair = IdentityKeyPair::try_from(bytes.as_slice())
            .map_err(|_| MeshError::Binding("invalid local account identity".into()))?;
        let now = now_secs();
        let proof = BindingV1 {
            account_id: *account_id.0.as_bytes(),
            device_id,
            signal_identity: keypair
                .identity_key()
                .serialize()
                .as_ref()
                .try_into()
                .map_err(|_| MeshError::Binding("invalid Signal public key length".into()))?,
            mesh_static: self.identity.static_public(),
            mesh_signing: self.identity.signing_public(),
            handshake_hash,
            sender_nonce,
            recipient_nonce,
            created_at: now,
            expires_at: now + PROOF_TTL_SECS,
            generation: 1,
            account_signature: [0; 64],
            mesh_signature: [0; 64],
        }
        .sign(&keypair, &self.identity)
        .ok_or_else(|| MeshError::Binding("could not sign account binding".into()))?;
        self.ble
            .send(BleCommand::BindingProof {
                peer: peer.clone(),
                bytes: proof.encode(),
            })
            .map_err(|_| MeshError::NotRunning)?;
        self.binding_store
            .offer(&account_id.to_string(), peer, now)?;
        self.state.with(|s| {
            s.binding_offers.insert(peer.clone());
        });
        Ok(())
    }

    /// QR contains a signed identity attestation and invite nonce. It is useful
    /// only alongside a fresh live Noise proof from the same peer.
    pub fn identity_qr(
        &self,
        peer: &PeerId,
        account_id: UserId,
        device_id: u32,
    ) -> Result<String, MeshError> {
        use libsignal_protocol::IdentityKeyPair;
        if device_id == 0 {
            return Err(MeshError::Binding("no account device is active".into()));
        }
        let (_, bytes) = self
            .key_store
            .get_local_identity()?
            .ok_or_else(|| MeshError::Binding("no account identity is available".into()))?;
        let bytes = zeroize::Zeroizing::new(bytes);
        let keypair = IdentityKeyPair::try_from(bytes.as_slice())
            .map_err(|_| MeshError::Binding("invalid local account identity".into()))?;
        let nonce = self
            .state
            .with(|s| s.chatter_hello.local_nonce(peer))
            .ok_or_else(|| {
                MeshError::Binding("wait for an encrypted Chatter peer connection".into())
            })?;
        let qr = BindingQr {
            account_id: *account_id.0.as_bytes(),
            device_id,
            signal_identity: keypair
                .identity_key()
                .serialize()
                .as_ref()
                .try_into()
                .map_err(|_| MeshError::Binding("invalid Signal public key length".into()))?,
            mesh_static: self.identity.static_public(),
            mesh_signing: self.identity.signing_public(),
            nonce,
            expires_at: now_secs() + PROOF_TTL_SECS,
            signature: [0; 64],
        }
        .sign(&keypair, &self.identity)
        .ok_or_else(|| MeshError::Binding("could not sign QR identity".into()))?;
        Ok(qr.to_text())
    }

    pub fn identity_qr_png(
        &self,
        peer: &PeerId,
        account_id: UserId,
        device_id: u32,
    ) -> Result<Vec<u8>, MeshError> {
        use image::{ImageEncoder, Luma};
        let text = self.identity_qr(peer, account_id, device_id)?;
        let qr = qrcode::QrCode::new(text.as_bytes())
            .map_err(|e| MeshError::Binding(format!("could not encode QR: {e}")))?;
        let image = qr.render::<Luma<u8>>().quiet_zone(true).build();
        let scaled =
            image::imageops::resize(&image, 512, 512, image::imageops::FilterType::Nearest);
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(scaled.as_raw(), 512, 512, image::ExtendedColorType::L8)
            .map_err(|e| MeshError::Binding(format!("could not render QR: {e}")))?;
        Ok(bytes)
    }

    /// Human comparison code for the current Noise transcript and both static keys.
    pub fn comparison_code(&self, peer: &PeerId) -> Option<String> {
        use sha2_10::{Digest, Sha256};
        let (remote, transcript) = self.state.with(|s| {
            (
                s.sessions.remote_static(peer),
                s.sessions.handshake_hash(peer),
            )
        });
        let (Some(remote), Some(transcript)) = (remote, transcript) else {
            return None;
        };
        let own = self.identity.static_public();
        let (first, second) = if own <= remote {
            (own, remote)
        } else {
            (remote, own)
        };
        let mut hash = Sha256::new();
        hash.update(b"chatter.mesh.comparison.v1\0");
        hash.update(transcript);
        hash.update(first);
        hash.update(second);
        let digest = hash.finalize();
        let code = u32::from_be_bytes(digest[..4].try_into().ok()?) % 1_000_000;
        Some(format!("{code:06}"))
    }

    pub fn binding_state(&self, owner: UserId, peer: &PeerId) -> Result<PeerBinding, MeshError> {
        if let Some(key) = self.state.with(|s| s.sessions.remote_static(peer)) {
            self.binding_store
                .observe_live_key(&owner.to_string(), peer, &key)?;
        }
        if let Some((account_id, old_key)) = self
            .binding_store
            .pinned_account_key(&owner.to_string(), peer)?
        {
            let account = uuid::Uuid::from_bytes(account_id).to_string();
            if self
                .key_store
                .get_identity(&account)?
                .is_some_and(|current| current != old_key)
            {
                self.binding_store
                    .mark_key_changed(&owner.to_string(), peer)?;
            }
        }
        let stored = self.binding_store.state(&owner.to_string(), peer)?;
        if stored == PeerBinding::Unverified
            && self.binding_store.has_offer(&owner.to_string(), peer)?
        {
            return Ok(PeerBinding::BindingOffered);
        }
        Ok(stored)
    }

    pub fn remove_account_bindings(&self, owner: UserId) -> Result<(), crate::error::StorageError> {
        self.binding_store.wipe_owner(&owner.to_string())?;
        self.state.with(|s| {
            s.pending_bindings.clear();
            s.binding_offers.clear();
        });
        Ok(())
    }

    /// Finish verification only when the current transcript-bound proof agrees
    /// with a previously pinned Signal contact or the in-person scanned QR.
    pub fn verify_binding(
        &self,
        owner: UserId,
        peer: &PeerId,
        scanned_qr: Option<&str>,
    ) -> Result<PeerBinding, MeshError> {
        let proof = self
            .state
            .with(|s| s.pending_bindings.get(peer).cloned())
            .ok_or_else(|| MeshError::Binding("wait for this peer's live account proof".into()))?;
        let account = uuid::Uuid::from_bytes(proof.account_id).to_string();
        let method = if let Some(text) = scanned_qr {
            let qr = BindingQr::from_text(text, now_secs())
                .ok_or_else(|| MeshError::Binding("QR invite is invalid or expired".into()))?;
            if !qr.matches(&proof) {
                return Err(MeshError::Binding(
                    "QR identity does not match the live peer".into(),
                ));
            }
            BindingMethod::InPersonQr
        } else {
            let pinned = self.key_store.get_identity(&account)?.ok_or_else(|| {
                MeshError::Binding("this account has no pinned Signal identity; scan its QR".into())
            })?;
            if pinned != proof.signal_identity {
                return Err(MeshError::Binding("pinned Signal identity changed".into()));
            }
            BindingMethod::PinnedContact
        };
        let live = self.state.with(|s| {
            (
                s.sessions.remote_static(peer),
                s.sessions.handshake_hash(peer),
                s.chatter_hello.local_nonce(peer),
                s.chatter_hello.get(peer).map(|h| h.nonce),
            )
        });
        let (Some(static_key), Some(hash), Some(local), Some(remote)) = live else {
            return Err(MeshError::Binding("the live Noise exchange ended".into()));
        };
        if !proof.verify_live(&static_key, &hash, &local, &remote, now_secs()) {
            return Err(MeshError::Binding(
                "live account proof is no longer valid".into(),
            ));
        }
        let result = self.binding_store.accept(
            &owner.to_string(),
            peer,
            &proof,
            method,
            now_secs(),
            scanned_qr.is_some(),
        )?;
        self.state.with(|s| {
            s.pending_bindings.remove(peer);
        });
        Ok(result)
    }
    /// Load the identity and wire the stack. Does not touch the radio or the
    /// network — [`start`](Self::start) does that.
    pub fn new(deps: MeshDeps, config: MeshConfig) -> Result<Self, MeshError> {
        let identity_store = Arc::clone(&deps.identity_store);
        let identity = Arc::new(deps.identity_store.get_or_create()?);
        let config = Arc::new(config);
        let state = Arc::new(MeshState::new(identity.peer_id()));
        let nostr_state = Arc::new(NostrState::new());
        let timeline = Arc::new(MeshTimeline::new(
            deps.messages,
            deps.conversations,
            deps.deliveries,
            deps.attachments,
        ));
        let events = MeshEvents::new(EVENT_CAPACITY, Arc::clone(&timeline));

        // The receivers for the first channel are dropped: nothing runs until
        // `start`, and `start` rebinds. A command sent before then goes nowhere,
        // which is what "not running" means.
        let (ble_bus, _) = CommandBus::new();
        let (nostr_bus, _) = CommandBus::new();

        let router = Arc::new(RouterHandle::spawn(RouterDeps {
            state: Arc::clone(&state),
            nostr_state: Arc::clone(&nostr_state),
            identity: Arc::clone(&identity),
            storage: Arc::clone(&deps.storage),
            sealer: Arc::clone(&deps.sealer),
            ble: Arc::clone(&ble_bus),
            nostr: Arc::clone(&nostr_bus),
            events: events.clone(),
            bridge_enabled: config.bridge_enabled,
        }));

        state.with(|inner| inner.duty_enabled = config.scan_duty_cycle);

        // Probed once, here, by sealing a byte. `Sealer` is a trait object so it
        // cannot be asked directly, and a probe covers every way a sealer can be
        // unable to hold a key rather than only the one this crate knows about.
        // Doing it at construction keeps `start()` cheap and means the reason is
        // available before the radio is ever touched.
        let sealer_unavailable = match deps.sealer.seal(b"probe") {
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(
                    target: "mesh",
                    %error,
                    "the mesh cannot seal what it retains; it will not start"
                );
                Some(error.to_string())
            }
        };

        Ok(Self {
            key_store: deps.key_store,
            binding_store: deps.binding_store,
            identity,
            config,
            state,
            nostr_state,
            identity_store,
            storage: deps.storage,
            sealer: deps.sealer,
            router,
            ble: Arc::clone(&ble_bus),
            nostr: Arc::clone(&nostr_bus),
            events,
            timeline,
            has_radio: deps.radio_factory.is_some() || radio::platform_radio().is_some(),
            radio_factory: deps.radio_factory,
            relay_connector: deps.relay_connector,
            tasks: Mutex::new(Vec::new()),
            running: AtomicBool::new(false),
            sealer_unavailable,
        })
    }

    /// Why the mesh cannot seal what it retains, if it cannot.
    fn sealer_unavailable_reason(&self) -> Option<String> {
        self.sealer_unavailable.clone()
    }

    /// Self-asserted name. No identity or account verification is implied.
    pub fn nickname(&self) -> String {
        self.config.nickname()
    }

    pub fn set_nickname(&self, nickname: String) {
        self.config.set_nickname(nickname);
        if self.is_running() {
            let _ = self.ble.send(BleCommand::Announce);
        }
    }

    /// Start the radio and the relay pool.
    ///
    /// Both halves are optional and independent: a build with no radio still
    /// carries geohash channels, and a client with no relay connector still
    /// carries the BLE mesh. Neither absence is an error.
    pub async fn start(&self) -> Result<(), MeshError> {
        // Checked before the radio is touched. A mesh whose sealer has no key
        // would advertise, connect, and then fail every retained write — a peer
        // list that looks healthy over a store that cannot keep anything. Better
        // to stay off and say which key is missing.
        if let Some(reason) = self.sealer_unavailable_reason() {
            self.running.store(false, Ordering::SeqCst);
            return Err(MeshError::SealerUnavailable(reason));
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // Fresh channels for fresh runtimes. Every holder — this service and the
        // router — sends through the bus, so none of them has to be told.
        let ble_rx = self.ble.rebind();
        let nostr_rx = self.nostr.rebind();

        let mut tasks = Vec::new();

        // Fresh per start, never reused: see `radio_override`.
        let radio = match self.radio_factory.as_ref() {
            Some(build) => Some(build()),
            None => radio::platform_radio(),
        };
        if let Some(radio) = radio {
            let runtime = BleRuntime::new(
                radio,
                Arc::clone(&self.state),
                Arc::clone(&self.nostr_state),
                Arc::clone(&self.identity),
                Arc::clone(&self.config),
                self.events.clone(),
                Arc::clone(&self.router),
                self.storage.clone(),
                Arc::clone(&self.sealer),
                Arc::clone(&self.binding_store),
            );
            tasks.push(tokio::spawn(runtime.run(ble_rx)));
        } else {
            tracing::info!(
                target: "mesh",
                "no radio on this platform; the mesh runs over nostr only"
            );
        }

        if let Some(connector) = self.relay_connector.clone() {
            // `RelayPool::new` spawns its dispatcher, so it has to be built
            // inside the runtime rather than at construction time.
            let (pool, relay_events) = RelayPool::new(connector);
            let runtime = NostrRuntime::new(
                pool,
                Arc::clone(&self.nostr_state),
                Arc::clone(&self.state),
                NostrIdentities::from_mesh(&self.identity),
                Arc::clone(&self.config),
                self.events.clone(),
                Arc::clone(&self.router),
            );
            tasks.push(tokio::spawn(runtime.run(nostr_rx, relay_events)));
        } else {
            tracing::info!(
                target: "mesh",
                "no relay connector configured; geohash channels and gift-wrapped DMs are off"
            );
        }

        *self.tasks.lock() = tasks;
        Ok(())
    }

    /// Stop and join both transport runtimes; the router lives until service drop.
    pub async fn shutdown(&self) {
        if !self.running.swap(false, Ordering::SeqCst) {
            return;
        }
        let _ = self.ble.send(BleCommand::Stop);
        let _ = self.nostr.send(NostrCommand::Stop);

        let tasks: Vec<JoinHandle<()>> = std::mem::take(&mut *self.tasks.lock());
        for task in tasks {
            let _ = task.await;
        }
        // The router's channel is permanent. Keep its retained outbox actor
        // alive across a radio restart; RouterHandle::drop joins it at teardown.
    }

    // -------------------------------------------------------------- read paths

    /// Subscribe to everything the mesh reports.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<MeshEvent> {
        self.events.subscribe()
    }

    /// This device's mesh peer ID — `SHA256(static key)[..8]` as hex.
    #[must_use]
    pub fn local_peer(&self) -> PeerId {
        self.identity.peer_id()
    }

    /// This device's mesh fingerprint, for out-of-band verification.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        self.identity.fingerprint()
    }

    /// Every peer the mesh currently knows, shaped for display.
    #[must_use]
    pub fn peers(&self) -> Vec<MeshPeer> {
        self.state.peers()
    }

    /// Peers still in radio range, shaped and ordered by the core peer registry.
    #[must_use]
    pub fn nearby_peers(&self) -> Vec<MeshPeer> {
        if !self.status().radio_running {
            return Vec::new();
        }
        self.state.peers().into_iter().filter(|peer| peer.reachable || peer.connected).collect()
    }

    /// Resolve a message's local mesh sender id without consulting the server.
    #[must_use]
    pub fn peer_for_sender(&self, sender: &UserId) -> Option<MeshPeer> {
        self.state.peers().into_iter().find(|peer| mapping::peer_user_id(&peer.peer) == *sender)
    }

    /// Create a durable DM row so it can be opened before the first message.
    pub fn open_dm(&self, peer: &PeerId) -> Result<String, MeshError> {
        let known = self.state.peers().into_iter().find(|known| known.peer == *peer)
            .ok_or(MeshError::OutOfRange("unknown mesh peer"))?;
        self.timeline.open_dm(peer, &known.nickname);
        Ok(MeshConversation::Dm(peer.clone()).conversation_id().to_string())
    }

    /// The nickname behind a mesh-derived `UserId`, if this device knows one.
    ///
    /// Mesh messages are stored with `mapping::peer_user_id(peer)` as their
    /// sender — a hash of the peer ID, so it lives in the same `UserId` space as
    /// every other message and needs no special case in storage or in the
    /// timeline. The human-readable nickname deliberately does not travel with
    /// it (`mapping.rs`), which is fine while a message is in flight — the live
    /// path carries the nickname alongside — and wrong the moment it is read
    /// back from the store, where all that survived is the hash. Re-opening a
    /// mesh conversation therefore listed every sender as a raw UUID.
    ///
    /// `peer_user_id` is one-way, so this recovers the name by recomputing it
    /// over the peers this device knows rather than by inverting anything. That
    /// is O(peers) per lookup, over a set bounded by Bluetooth range and by the
    /// geohash cells joined — tens, not thousands.
    ///
    /// A peer that is no longer known falls back to the nickname it last used,
    /// remembered when its message was stored. `None` means no peer we know
    /// hashes to this id and none ever named itself to us; callers must render
    /// that case rather than let a UUID through.
    ///
    /// Either way the name is self-declared — what the peer called itself, not
    /// a verified identity.
    #[must_use]
    pub fn sender_display_name(&self, sender: &UserId) -> Option<String> {
        self.state
            .peers()
            .into_iter()
            .find(|p| mapping::peer_user_id(&p.peer) == *sender)
            .map(|p| p.nickname)
            .filter(|n| !n.is_empty())
            .or_else(|| self.timeline.remembered_sender_name(sender))
    }

    /// Transport status across both halves.
    #[must_use]
    pub fn status(&self) -> MeshStatus {
        self.state.status(self.nostr_state.connected_relay_count())
    }

    /// Whether [`start`](Self::start) succeeded and has not been stopped.
    ///
    /// Deliberately **not** a field on [`MeshStatus`]. That struct is "which
    /// transports are live", it is rebuilt from `MeshState` in several places
    /// that have no view of the service, and it travels as
    /// [`MeshEvent::Status`](crate::services::mesh::MeshEvent::Status) — so a
    /// service flag riding along inside it would have read `false` every time a
    /// relay flapped, and told the UI the mesh had died.
    ///
    /// Distinct from `MeshStatus::radio_running`, and the distinction is the
    /// point: a running service with no radio still carries geohash cells over
    /// relays and still retains what you write for a carrier that has not
    /// arrived yet, whereas a service that never started has nothing at all — no
    /// timeline, no identity loaded, no store to queue into.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// The geohash cells currently joined.
    #[must_use]
    pub fn joined_geohashes(&self) -> Vec<String> {
        self.nostr_state.joined_cells()
    }

    /// The conversation a public mesh room message belongs to.
    #[must_use]
    pub fn room_conversation() -> MeshConversation {
        MeshConversation::Room
    }

    /// Every mesh conversation, shaped for the unified sidebar.
    ///
    /// The public room, every joined geohash cell, and every mesh DM — already
    /// ordered and counted. The caller renders this list; it filters nothing and
    /// derives nothing, and in particular it does not go looking for a server
    /// record of any of these conversations, because there is none.
    pub fn conversations(&self) -> Result<Vec<UnifiedConversation>, MeshError> {
        Ok(self.timeline.conversations()?)
    }

    /// The mesh conversation a sidebar row refers to, or `None` when the id is
    /// not one of ours.
    ///
    /// The UI holds conversation ids as strings and cannot re-derive a
    /// [`MeshConversation`] from one — the derivation is one-way — so opening a
    /// mesh row goes through here.
    #[must_use]
    pub fn conversation_for(&self, id: &str) -> Option<MeshConversation> {
        let id: ChannelId = id.parse().ok()?;
        self.timeline.conversation_for(&id)
    }

    /// History for one mesh conversation, oldest first.
    ///
    /// `before_sent_at` pages backwards through it in the sender's nanosecond
    /// timestamps, the same cursor every other conversation type uses.
    pub fn history(
        &self,
        conversation: &MeshConversation,
        before_sent_at: Option<u64>,
        limit: u32,
    ) -> Result<Vec<Message>, MeshError> {
        Ok(self.timeline.history(conversation, before_sent_at, limit)?)
    }

    /// Mark a mesh conversation read on this device.
    ///
    /// Local only. Telling the peer costs airtime, so it is a separate act:
    /// [`mark_read`](Self::mark_read) puts a receipt on the wire.
    pub fn mark_conversation_read(&self, conversation: &MeshConversation) -> Result<(), MeshError> {
        Ok(self.timeline.mark_read(conversation)?)
    }

    // ------------------------------------------------------------- write paths

    /// Broadcast into the public mesh room.
    ///
    /// Returns the wire message ID. The message itself arrives on the event
    /// stream as a [`MeshEvent::Message`] with `is_local` set — one path for
    /// local and remote messages, so a row cannot be built two different ways.
    pub fn send_room(&self, text: &str) -> Result<String, MeshError> {
        let content = Self::require_content(text)?;
        if !self.has_radio {
            return Err(MeshError::NoRadio);
        }
        let message_id = new_message_id();
        self.ble
            .send(BleCommand::PublicMessage {
                message_id: message_id.clone(),
                content,
            })
            .map_err(|_| MeshError::WorkerGone)?;
        // A broadcast has no recipient and therefore no later state to progress
        // to. Recording it now is the whole of its delivery history; leaving it
        // unrecorded would make it render as an ordinary one-to-one "sent".
        self.timeline
            .record_delivery(&message_id, MeshDelivery::Broadcast);
        Ok(message_id)
    }

    pub fn join_channel(&self, name: &str) -> Result<String, MeshError> {
        let name = chatter_mesh::channel::canonical_name(name).ok_or(MeshError::OutOfRange(
            "channel name must be 1-32 ASCII letters, digits or hyphens",
        ))?;
        if !self.timeline.has_channel(&name) && self.timeline.active_channels().len() >= 8 {
            return Err(MeshError::OutOfRange(
                "at most eight mesh channels can be active",
            ));
        }
        self.timeline.join_channel(&name);
        let _ = self
            .ble
            .send(BleCommand::ChannelPresence { name: name.clone() });
        Ok(name)
    }

    pub fn nearby_channels(&self) -> Vec<String> {
        self.timeline.nearby_channels()
    }

    pub fn set_channel_muted(&self, name: &str, muted: bool) -> Result<(), MeshError> {
        let name = chatter_mesh::channel::canonical_name(name)
            .ok_or(MeshError::OutOfRange("invalid channel name"))?;
        self.events.set_channel_muted(&name, muted);
        Ok(())
    }

    pub fn channel_muted(&self, name: &str) -> bool {
        self.events.channel_muted(name)
    }

    pub fn set_peer_muted(&self, peer: &PeerId, muted: bool) {
        self.events.set_peer_muted(peer, muted);
    }

    pub fn peer_muted(&self, peer: &PeerId) -> bool {
        self.events.peer_muted(peer)
    }

    pub fn set_peer_blocked(&self, peer: &PeerId, blocked: bool) {
        self.events.set_peer_blocked(peer, blocked);
    }

    pub fn peer_blocked(&self, peer: &PeerId) -> bool {
        self.events.peer_blocked(peer)
    }

    pub fn leave_channel(&self, name: &str) -> Result<(), MeshError> {
        let name = chatter_mesh::channel::canonical_name(name)
            .ok_or(MeshError::OutOfRange("invalid channel name"))?;
        self.timeline.leave_channel(&name);
        Ok(())
    }

    pub fn send_channel(&self, name: &str, text: &str) -> Result<String, MeshError> {
        let name = chatter_mesh::channel::canonical_name(name)
            .ok_or(MeshError::OutOfRange("invalid channel name"))?;
        if !self.timeline.has_channel(&name) {
            return Err(MeshError::OutOfRange("join the channel before sending"));
        }
        let content = Self::require_content(text)?;
        if chatter_mesh::channel::encode(&name, &content).is_none() {
            return Err(MeshError::OutOfRange("channel text exceeds 256 bytes"));
        }
        if !self.has_radio {
            return Err(MeshError::NoRadio);
        }
        let message_id = new_message_id();
        self.ble
            .send(BleCommand::ChannelMessage {
                message_id: message_id.clone(),
                name,
                content,
            })
            .map_err(|_| MeshError::WorkerGone)?;
        Ok(message_id)
    }

    /// Put a file on the air for the public mesh room.
    ///
    /// Returns the wire message ID; the row itself arrives on the event stream as
    /// a [`MeshEvent::Message`] whose content is
    /// [`Mixed`](crate::domain::message::MessageContent::Mixed), one path for
    /// local and remote files just as [`send_room`](Self::send_room) is for text.
    ///
    /// # Room only, and chunked above one packet's worth
    ///
    /// A mesh file transfer is a cleartext broadcast, so there is no directed
    /// form to offer: this client's DMs are Noise-encrypted, and
    /// `on_file_transfer` refuses a directed file for that reason.
    ///
    /// The *size* decides the protocol, and it is decided here because this is
    /// where the file's length is first known:
    ///
    /// - At or below [`MAX_SINGLE_PACKET_FILE_BYTES`] the file rides one
    ///   `FileTransfer` packet, exactly as P8 shipped it and exactly as real
    ///   bitchat speaks. Interop must not regress for the common case.
    /// - Above it the file is *chunked* — see [`send_file_streamed`] — because a
    ///   single packet above that size cannot be reassembled inside
    ///   `FRAGMENT_LIFETIME_SECS` at any goodput this radio achieves. That is not
    ///   an optimisation: the single-packet path cannot deliver a large file at
    ///   all, whatever the sender does.
    /// - Above [`MAX_STREAMED_FILE_BYTES`] it is refused, and the ceiling is
    ///   *time* rather than memory now that nothing buffers the whole file.
    ///
    /// [`send_file_streamed`]: Self::send_file_streamed
    pub fn send_file(&self, path: &Path) -> Result<String, MeshError> {
        if !self.has_radio {
            return Err(MeshError::NoRadio);
        }
        // Against the file's own length first, so a multi-gigabyte pick is
        // refused without being read into memory.
        let declared = std::fs::metadata(path)
            .map_err(|source| MeshError::FileIo {
                path: path.display().to_string(),
                source,
            })?
            .len();
        if declared > MAX_STREAMED_FILE_BYTES as u64 {
            return Err(MeshError::FileTooLarge {
                size: declared,
                max: MAX_STREAMED_FILE_BYTES,
            });
        }
        if declared > MAX_SINGLE_PACKET_FILE_BYTES as u64 {
            return self.send_file_streamed(path, declared);
        }
        let content = std::fs::read(path).map_err(|source| MeshError::FileIo {
            path: path.display().to_string(),
            source,
        })?;
        // Re-checked against what was actually read: the file can grow between
        // the two calls, and the limit is the receiver's decode ceiling, so one
        // byte over it is a packet nobody will accept.
        if content.len() > MAX_SINGLE_PACKET_FILE_BYTES {
            return Err(MeshError::FileTooLarge {
                size: content.len() as u64,
                max: MAX_SINGLE_PACKET_FILE_BYTES,
            });
        }
        if content.is_empty() {
            // `BitchatFilePacket::decode` rejects an empty content TLV outright,
            // so an empty file is airtime spent on a packet no peer can accept.
            return Err(MeshError::EmptyContent);
        }

        // The name is the leaf, not the path: the rest is this device's directory
        // layout and is nobody else's business.
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
        // `file_size: None` lets `encode` write the content length, so the
        // declared size can never disagree with the bytes that follow it.
        let file = BitchatFilePacket::new(file_name, None, Some(mime), content);

        let message_id = new_message_id();
        self.ble
            .send(BleCommand::FileTransfer {
                message_id: message_id.clone(),
                file,
            })
            .map_err(|_| MeshError::WorkerGone)?;
        // A broadcast, exactly as `send_room`: nobody acknowledges it, so this is
        // the whole of its delivery history. Leaving it unrecorded would make the
        // row render as an ordinary one-to-one "sent" — see `outbound_message`.
        self.timeline
            .record_delivery(&message_id, MeshDelivery::Broadcast);
        Ok(message_id)
    }

    /// Put a large file on the air as a chunked, resumable transfer.
    ///
    /// Reached from [`send_file`](Self::send_file) by size, never called
    /// directly by a UI: which protocol a file rides is a property of the file,
    /// not a choice a surface should be making.
    ///
    /// The file is not read into memory here either. It is hashed in windows —
    /// which is unavoidable, because the manifest's whole-file digest is the only
    /// thing that will tell the receiver its assembled file is the right one — and
    /// then the *path* is handed to the runtime, which reads one chunk at a time.
    ///
    /// # Why the transfer id is derived rather than random
    ///
    /// `SHA-256(local peer ‖ file digest)[..8]`. Deterministic, which buys two
    /// things a random id would not:
    ///
    /// - **Resume across a re-send.** A transfer interrupted at 80%, re-sent an
    ///   hour later, keys onto the same spool entry on the receiver and finishes
    ///   the remaining 20% instead of starting over.
    /// - **No cross-sender collision.** Two peers sending the same file get
    ///   different ids, so neither can write into the other's transfer — which the
    ///   per-chunk sender check would refuse, but only after wasting the airtime.
    fn send_file_streamed(&self, path: &Path, declared: u64) -> Result<String, MeshError> {
        use chatter_mesh::tlv::file_stream::{CHUNK_BYTES, FileManifest};
        use sha2::{Digest, Sha256};

        if declared == 0 {
            return Err(MeshError::EmptyContent);
        }

        // O(256 KiB) regardless of the file, like everything else on this path.
        let digest = file_stream::digest_file(path)?;

        let transfer_id: [u8; 8] = {
            let mut hasher = Sha256::new();
            hasher.update(self.identity.peer_id().as_str().as_bytes());
            hasher.update(digest);
            let full: [u8; 32] = hasher.finalize().into();
            let mut id = [0u8; 8];
            id.copy_from_slice(&full[..8]);
            id
        };

        // The leaf name only: the rest is this device's directory layout.
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();

        let manifest = FileManifest::new(
            transfer_id,
            file_name,
            Some(mime),
            declared,
            CHUNK_BYTES as u32,
            digest,
        )?;

        let message_id = new_message_id();
        self.ble
            .send(BleCommand::StreamedFile {
                message_id: message_id.clone(),
                manifest,
                path: path.to_path_buf(),
            })
            .map_err(|_| MeshError::WorkerGone)?;
        // A broadcast like any other room file: nobody acknowledges it. That is
        // true even though this one takes minutes — progress is not delivery, and
        // there is no acknowledgement to promote it with.
        self.timeline
            .record_delivery(&message_id, MeshDelivery::Broadcast);
        Ok(message_id)
    }

    /// How long a file would take to put on the air, in seconds, at the two ends
    /// of the goodput measured on this hardware.
    ///
    /// Exposed because the honest answer for a large file is "twenty minutes",
    /// and a surface that starts a twenty-minute transfer without saying so is
    /// promising something the radio cannot deliver quickly — the same class of
    /// claim P7 removed from the public room. The UI decides how to present it;
    /// this decides what the numbers are.
    #[must_use]
    pub fn file_transfer_estimate_secs(&self, size_bytes: u64) -> (u64, u64) {
        use chatter_mesh::tlv::file_stream::{
            CHUNK_BYTES, MAX_SINGLE_PACKET_FILE_BYTES, OPTIMISTIC_GOODPUT_BYTES_PER_SEC,
            PESSIMISTIC_GOODPUT_BYTES_PER_SEC, estimated_seconds,
        };
        // A single-packet file is one "chunk" of its own size, which is what
        // makes the estimate continuous across the size fork rather than jumping
        // at the boundary.
        let chunk = if size_bytes > MAX_SINGLE_PACKET_FILE_BYTES as u64 {
            CHUNK_BYTES as u32
        } else {
            size_bytes.max(1) as u32
        };
        (
            estimated_seconds(size_bytes, chunk, OPTIMISTIC_GOODPUT_BYTES_PER_SEC),
            estimated_seconds(size_bytes, chunk, PESSIMISTIC_GOODPUT_BYTES_PER_SEC),
        )
    }

    /// Publish into a geohash channel.
    pub fn send_geohash(&self, cell: &str, text: &str) -> Result<String, MeshError> {
        let content = Self::require_content(text)?;
        if !chatter_mesh_nostr::geohash::is_valid(cell) {
            return Err(MeshError::InvalidGeohash(cell.to_string()));
        }
        if self.relay_connector.is_none() {
            return Err(MeshError::NoRelayConnector);
        }
        let message_id = new_message_id();
        self.nostr
            .send(NostrCommand::GeohashMessage {
                cell: cell.to_string(),
                message_id: message_id.clone(),
                content,
                nickname: self.config.nickname(),
            })
            .map_err(|_| MeshError::WorkerGone)?;
        self.timeline
            .record_delivery(&message_id, MeshDelivery::Broadcast);
        Ok(message_id)
    }

    /// Send a private message, letting the router choose the transport.
    ///
    /// The returned [`MeshDelivery`] is the branch the router took, which is the
    /// whole decision the UI needs: [`MeshDelivery::Secure`] means nothing was
    /// retained, everything else means a copy is held and will be retried.
    pub async fn send_dm(&self, peer: &PeerId, text: &str) -> Result<MeshDelivery, MeshError> {
        if self.events.peer_blocked(peer) {
            return Err(MeshError::OutOfRange("peer is locally blocked"));
        }
        let content = Self::require_content(text)?;
        if !self.can_route_to(peer) {
            return Err(MeshError::Unroutable(peer.as_str().to_string()));
        }
        let message_id = new_message_id();
        let nickname = self.nickname_for(peer);

        // Mirror it into the timeline before routing. The mesh has no server
        // echo, so this is the only copy — and it must exist whichever transport
        // the router picks, which is why it is here and not in a runtime.
        //
        // The send goes through [`MeshEvents`], so this both stores the row and
        // publishes it: the message is on disk before this call returns, and a
        // restart mid-delivery still shows what the user sent.
        let conversation = MeshConversation::Dm(peer.clone());
        let message = mapping::outbound_message(
            &conversation,
            &message_id,
            &self.identity.peer_id(),
            &content,
            now_millis(),
        );
        self.events.send(MeshEvent::Message(Box::new(MeshMessage {
            message,
            conversation,
            sender: self.identity.peer_id(),
            nickname: self.config.nickname(),
            is_local: true,
            sender_verified: true,
        })));

        let outcome = self
            .router
            .send_private(content, peer.clone(), nickname, message_id.clone())
            .await?;

        let delivery = match outcome.link {
            LinkState::Secure => MeshDelivery::Secure,
            LinkState::Insecure => MeshDelivery::Insecure,
            LinkState::Reachable => MeshDelivery::Reachable,
            LinkState::Unreachable => MeshDelivery::Unreachable,
        };
        // Persisted here rather than left to the router's own `Delivery` event.
        // That event is the right vehicle for every *later* transition, but it
        // travels through an actor, so relying on it alone leaves a window in
        // which the message is on disk with no state — and a message with no
        // recorded state renders as an ordinary "sent". The outcome is already
        // in hand at this point; storing it is not a guess.
        self.timeline.record_delivery(&message_id, delivery);
        Ok(delivery)
    }

    /// Acknowledge that a message has been read.
    ///
    /// Routed over the same first-reachable rule the router uses, because a
    /// receipt that goes out on a transport the peer is not on leaves their
    /// message unread forever.
    pub fn mark_read(&self, peer: &PeerId, message_id: &str) -> Result<(), MeshError> {
        if self.state.is_connected(peer) || self.state.is_reachable(peer) {
            self.ble
                .send(BleCommand::Ack {
                    peer: peer.clone(),
                    message_id: message_id.to_string(),
                    payload_type: NoisePayloadType::ReadReceipt,
                })
                .map_err(|_| MeshError::WorkerGone)
        } else if self.nostr_state.knows_peer(peer) {
            self.nostr
                .send(NostrCommand::Ack {
                    peer: peer.clone(),
                    message_id: message_id.to_string(),
                    payload_type: NoisePayloadType::ReadReceipt,
                })
                .map_err(|_| MeshError::WorkerGone)
        } else {
            Err(MeshError::Unroutable(peer.as_str().to_string()))
        }
    }

    /// Join a geohash channel.
    pub fn join_geohash(&self, cell: &str) -> Result<(), MeshError> {
        if !chatter_mesh_nostr::geohash::is_valid(cell) {
            return Err(MeshError::InvalidGeohash(cell.to_string()));
        }
        if self.relay_connector.is_none() {
            return Err(MeshError::NoRelayConnector);
        }
        self.nostr
            .send(NostrCommand::Join(cell.to_string()))
            .map_err(|_| MeshError::WorkerGone)
    }

    /// Leave a geohash channel.
    pub fn leave_geohash(&self, cell: &str) -> Result<(), MeshError> {
        self.nostr
            .send(NostrCommand::Leave(cell.to_string()))
            .map_err(|_| MeshError::WorkerGone)
    }

    /// Announce our identity now, outside the normal cadence.
    pub fn announce_now(&self) -> Result<(), MeshError> {
        if !self.has_radio {
            return Err(MeshError::NoRadio);
        }
        self.ble
            .send(BleCommand::Announce)
            .map_err(|_| MeshError::WorkerGone)
    }

    /// Tell the duty cycle whether the app is in the foreground.
    ///
    /// Scanning is the expensive half of BLE. In the foreground it runs
    /// continuously; in the background it duty-cycles.
    pub fn set_app_active(&self, active: bool) {
        self.state.with(|state| state.app_is_active = active);
    }

    /// Forget everything: retained mail, carried envelopes, gossip history, and
    /// the bridge's dedup sets.
    ///
    /// The identity itself is wiped separately, by
    /// [`MeshIdentityStore::wipe`](crate::storage::mesh_identity_store::MeshIdentityStore::wipe) —
    /// they are deliberately not the same operation, because clearing a device's
    /// mail is not the same act as destroying its identity.
    pub fn wipe(&self) -> Result<(), MeshError> {
        self.state.with(|state| {
            state.sessions.clear();
            state.chatter_hello.clear_live();
            state.pending_bindings.clear();
        });
        self.router.send(RouterCommand::Wipe)
    }

    /// Panic wipe: destroy everything this device holds about the mesh.
    ///
    /// Clears, in order: live sessions, peer pins, our one-time prekeys,
    /// retained mail, carried courier envelopes, gossip history, the bridge
    /// dedup sets, every mesh message, the conversation directory, and finally
    /// the identity keypair itself.
    ///
    /// # What this does not clear
    ///
    /// The chatter account, its Signal identity, and its messages. They are a
    /// separate identity with a separate threat model, and destroying an
    /// account is a larger, irreversible act that deserves its own deliberate
    /// confirmation rather than riding along with this one. Anyone relying on
    /// this to leave a device clean of *everything* needs to know that.
    ///
    /// # Why it continues after a failure
    ///
    /// Every step runs even if an earlier one fails, and the first error is
    /// returned at the end. Stopping at the first failure would leave the
    /// remainder on disk — the worst possible outcome for the one operation
    /// whose entire purpose is leaving nothing behind. A partial wipe that
    /// reports success would be worse still, so the error is not swallowed.
    ///
    /// The identity goes last. The steps before it log and emit events, and
    /// doing those under a destroyed identity would be the one moment this
    /// service could not describe itself.
    pub fn panic_wipe(&self) -> Result<(), MeshError> {
        self.config.set_nickname("anon".to_owned());
        self.events.clear_moderation();
        let mut first_error: Option<MeshError> = None;
        let fail = |error: MeshError, slot: &mut Option<MeshError>| {
            if slot.is_none() {
                *slot = Some(error);
            }
        };

        self.state.with(|state| {
            state.sessions.clear();
            state.chatter_hello.clear_live();
            state.pending_bindings.clear();
            state.binding_offers.clear();
            state.peers.clear_pins();
            state.peers.remove_all();
            state.prekeys = prekeys::PrekeyState::default();
        });
        for key in [
            chatter_mesh_store::storage::StorageKey::PeerPins,
            chatter_mesh_store::storage::StorageKey::LocalPrekeys,
        ] {
            if let Err(error) = self.storage.wipe(key) {
                fail(error.into(), &mut first_error);
            }
        }

        if let Err(error) = self.router.send(RouterCommand::Wipe) {
            fail(error, &mut first_error);
        }
        if let Err(error) = self.timeline.wipe() {
            fail(error.into(), &mut first_error);
        }
        if let Err(error) = self.identity_store.wipe() {
            fail(error.into(), &mut first_error);
        }
        if let Err(error) = self.binding_store.wipe_all() {
            fail(error.into(), &mut first_error);
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    // ----------------------------------------------------------------- helpers

    /// Whether any transport could carry a DM to this peer.
    fn can_route_to(&self, peer: &PeerId) -> bool {
        if peer.is_empty() {
            return false;
        }
        // Nostr peers are addressed by their gift-wrap pubkey; mesh peers by
        // their routing ID. A peer that is neither cannot be reached at all, and
        // saying so up front beats queueing a message nothing will ever pick up.
        if self.nostr_state.knows_peer(peer) {
            return self.relay_connector.is_some();
        }
        self.has_radio && peer.to_routing_bytes().is_some()
    }

    fn nickname_for(&self, peer: &PeerId) -> String {
        self.state
            .with(|state| state.peers.nickname(peer, false).map(str::to_string))
            .or_else(|| self.nostr_state.nickname(peer))
            .unwrap_or_else(|| peer.as_str().to_string())
    }

    /// Empty bodies never reach the wire: they cost airtime, render as nothing,
    /// and are indistinguishable from a decode failure on the far side.
    fn require_content(text: &str) -> Result<String, MeshError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(MeshError::EmptyContent);
        }
        Ok(trimmed.to_string())
    }
}

/// A fresh wire message ID.
///
/// Uppercase UUID, matching what bitchat puts on the wire, so an ID this client
/// mints is indistinguishable from one theirs did.
fn new_message_id() -> String {
    Uuid::new_v4().to_string().to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::mock::{LoopbackConnector, MockLink, OfflineConnector};
    use super::*;
    use crate::domain::message::ConversationType;
    use chatter_mesh_store::MemoryStorage;
    use chatter_mesh_store::StoreError;

    struct NoopSealer;

    impl Sealer for NoopSealer {
        fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, StoreError> {
            Ok(plaintext.to_vec())
        }

        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, StoreError> {
            Ok(sealed.to_vec())
        }
    }

    /// A service built without touching the database.
    ///
    /// `MeshIdentityStore` needs a real `Database`, and construction tests are
    /// about the wiring rather than about SQLCipher, so this assembles the same
    /// parts `MeshService::new` does from an identity handed in directly.
    fn service(
        radio: Option<Arc<dyn MeshLink>>,
        connector: Option<Arc<dyn RelayConnector>>,
    ) -> MeshService {
        let factory = radio.map(|radio| {
            Arc::new(move || Arc::clone(&radio)) as Arc<dyn Fn() -> Arc<dyn MeshLink> + Send + Sync>
        });
        service_with_radio_factory(factory, connector)
    }

    /// A service that builds its radio per `start`, as production does.
    fn service_with_radio_factory(
        radio_factory: Option<Arc<dyn Fn() -> Arc<dyn MeshLink> + Send + Sync>>,
        connector: Option<Arc<dyn RelayConnector>>,
    ) -> MeshService {
        let identity = Arc::new(MeshIdentity::generate(&mut crate::crypto::os_rng()));
        let config = Arc::new(MeshConfig::default());
        let state = Arc::new(MeshState::new(identity.peer_id()));
        let nostr_state = Arc::new(NostrState::new());
        let timeline = timeline::temp_timeline();
        let identity_store = timeline::temp_identity_store();
        let events = MeshEvents::new(EVENT_CAPACITY, Arc::clone(&timeline));
        let (ble_bus, ble_rx) = CommandBus::new();
        let (nostr_bus, nostr_rx) = CommandBus::new();
        // Kept alive rather than dropped. Several tests exercise send paths on a
        // service they never start, and a channel with no receiver refuses every
        // send — so dropping these would fail those tests for a reason that has
        // nothing to do with what they assert. `start` rebinds and does not use
        // them; they exist only so the bus has somewhere to put a command.
        Box::leak(Box::new((ble_rx, nostr_rx)));
        let storage: Arc<dyn Storage> = Arc::new(MemoryStorage::new());
        let sealer: Arc<dyn Sealer> = Arc::new(NoopSealer);

        let router = Arc::new(RouterHandle::spawn(RouterDeps {
            state: Arc::clone(&state),
            nostr_state: Arc::clone(&nostr_state),
            identity: Arc::clone(&identity),
            storage: Arc::clone(&storage),
            sealer: Arc::clone(&sealer),
            ble: Arc::clone(&ble_bus),
            nostr: Arc::clone(&nostr_bus),
            events: events.clone(),
            bridge_enabled: false,
        }));

        MeshService {
            // The test sealer is a no-op that always succeeds, so there is
            // nothing to report.
            sealer_unavailable: None,
            identity,
            identity_store,
            config,
            state,
            nostr_state,
            storage,
            sealer,
            router,
            ble: ble_bus,
            nostr: nostr_bus,
            events,
            timeline,
            has_radio: radio_factory.is_some(),
            radio_factory,
            relay_connector: connector,
            tasks: Mutex::new(Vec::new()),
            running: AtomicBool::new(false),
        }
    }

    #[tokio::test]
    async fn the_mesh_can_be_stopped_and_started_again() {
        // What the user's switch needs, and what neither half of this used to do.
        //
        // `start` once consumed a one-shot `Pending`, so a second call returned
        // `Ok(())` having started nothing. That was fixed with `CommandBus` — and
        // the test passed while the feature was still broken, because the service
        // kept *one radio* for its lifetime and handed the same stopped object to
        // the new runtime. A phone refused it outright: "this link layer has been
        // stopped; construct a new AndroidLink to restart".
        //
        // The mock hid that by being more permissive than production. It refuses a
        // second start now, so a service that reuses a radio fails here rather than
        // on somebody's phone — and this test supplies fresh radios, which is what
        // `start` does for real via `platform_radio`.
        let built: Arc<Mutex<Vec<Arc<MockLink>>>> = Arc::new(Mutex::new(Vec::new()));
        let radios = Arc::clone(&built);
        let factory: Arc<dyn Fn() -> Arc<dyn MeshLink> + Send + Sync> = Arc::new(move || {
            let radio = MockLink::shared();
            radios.lock().push(Arc::clone(&radio));
            radio as Arc<dyn MeshLink>
        });
        let service = service_with_radio_factory(
            Some(factory),
            Some(Arc::new(LoopbackConnector::new()) as Arc<dyn RelayConnector>),
        );

        async fn settle() {
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }

        service.start().await.expect("start");
        settle().await;
        assert!(service.is_running());
        assert_eq!(built.lock().len(), 1, "one radio built for the first start");
        assert!(built.lock()[0].is_started(), "and it is up");

        service.shutdown().await;
        assert!(!service.is_running());

        service.start().await.expect("restart");
        settle().await;
        assert!(service.is_running(), "a stopped mesh starts again");
        assert_eq!(
            built.lock().len(),
            2,
            "a *new* radio, because stopping one is terminal on both real link layers"
        );
        assert!(
            built.lock()[1].is_started(),
            "and the radio is up again, not merely the flag"
        );

        // And a command sent after the restart reaches the new runtime rather
        // than the channel the stopped one took with it.
        service
            .send_room("still here")
            .expect("a send after a restart is routed");
        service
            .router
            .send_private(
                "after restart".into(),
                PeerId::from_str("bbbbbbbbbbbbbbbb"),
                "Bob".into(),
                "restart-router-test".into(),
            )
            .await
            .expect("the private-message router also survives restart");

        service.shutdown().await;
        assert!(!service.is_running());
    }

    #[tokio::test]
    async fn it_starts_and_stops_with_both_transports_mocked() {
        let radio = MockLink::shared();
        let service = service(
            Some(Arc::clone(&radio) as Arc<dyn MeshLink>),
            Some(Arc::new(LoopbackConnector::new()) as Arc<dyn RelayConnector>),
        );
        service.start().await.expect("start");
        // Give the runtimes a turn so the radio actually comes up.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(radio.is_started(), "the radio was started");
        service.shutdown().await;
    }

    #[tokio::test]
    async fn starting_twice_is_a_no_op_rather_than_a_second_stack() {
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
        service.start().await.expect("first start");
        service.start().await.expect("second start");
        assert_eq!(service.tasks.lock().len(), 1, "one radio task, not two");
        service.shutdown().await;
    }

    #[tokio::test]
    async fn it_constructs_and_runs_with_no_radio_at_all() {
        // The supported configuration on Windows and every other target with no
        // MeshLink implementation.
        let service = service(
            None,
            Some(Arc::new(OfflineConnector) as Arc<dyn RelayConnector>),
        );
        service.start().await.expect("start");
        assert!(matches!(
            service.send_room("hello"),
            Err(MeshError::NoRadio)
        ));
        assert!(matches!(service.announce_now(), Err(MeshError::NoRadio)));
        // The Nostr half still works.
        assert!(service.join_geohash("u4pruy").is_ok());
        service.shutdown().await;
    }

    #[tokio::test]
    async fn it_constructs_and_runs_with_no_relay_connector() {
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
        service.start().await.expect("start");
        assert!(matches!(
            service.join_geohash("u4pruy"),
            Err(MeshError::NoRelayConnector)
        ));
        assert!(matches!(
            service.send_geohash("u4pruy", "hi"),
            Err(MeshError::NoRelayConnector)
        ));
        assert!(service.send_room("hello").is_ok());
        service.shutdown().await;
    }

    #[tokio::test]
    async fn empty_messages_never_reach_the_wire() {
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
        assert!(matches!(
            service.send_room("   "),
            Err(MeshError::EmptyContent)
        ));
        assert!(matches!(
            service.send_room(""),
            Err(MeshError::EmptyContent)
        ));
        let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
        assert!(matches!(
            service.send_dm(&peer, "\n\t ").await,
            Err(MeshError::EmptyContent)
        ));
    }

    /// A file of `bytes` zero bytes, in a temp dir deleted when the test ends.
    fn temp_file(name: &str, bytes: usize) -> std::path::PathBuf {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(name);
        std::fs::write(&path, vec![0x5A; bytes]).expect("write");
        crate::test_tempdir::hold(dir);
        path
    }

    /// A file of `bytes` length that occupies almost no disk.
    ///
    /// The size refusal is made against `metadata().len()` precisely so that a
    /// multi-gigabyte pick is refused without being read, which means a sparse
    /// file exercises it exactly as a real one would — and a real 16 MiB temp
    /// file per test run is 16 MiB of pointless IO.
    fn sparse_file(name: &str, bytes: u64) -> std::path::PathBuf {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(name);
        std::fs::File::create(&path)
            .expect("create")
            .set_len(bytes)
            .expect("set_len");
        crate::test_tempdir::hold(dir);
        path
    }

    #[tokio::test]
    async fn a_file_over_one_packet_is_chunked_rather_than_refused_or_truncated() {
        // The P9 size fork. A single `FileTransfer` packet above this size cannot
        // be reassembled inside `FRAGMENT_LIFETIME_SECS` at any goodput this
        // radio reaches — the assembly is evicted at 30 s measured from its first
        // fragment, whatever its progress — so sending one would spend minutes of
        // airtime on a packet that can never complete. Chunking is the only thing
        // that delivers it, and this is where that decision is made.
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);

        let path = temp_file("chunk-me.bin", MAX_SINGLE_PACKET_FILE_BYTES + 1);
        assert!(
            service.send_file(&path).is_ok(),
            "above one packet's worth is streamed, not refused"
        );

        // Exactly at the threshold still rides one packet, which is what keeps
        // interop with real bitchat for everything small.
        let path = temp_file("one-packet.bin", MAX_SINGLE_PACKET_FILE_BYTES);
        assert!(service.send_file(&path).is_ok());
    }

    #[tokio::test]
    async fn a_file_over_the_time_cap_is_refused_because_nobody_would_wait_for_it() {
        // Once nothing buffers the whole file, memory stops being the ceiling and
        // *time* becomes it. The refusal is against the declared length, before a
        // byte is read.
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);

        let path = sparse_file("far-too-big.bin", MAX_STREAMED_FILE_BYTES as u64 + 1);
        match service.send_file(&path) {
            Err(MeshError::FileTooLarge { size, max }) => {
                assert_eq!(size, MAX_STREAMED_FILE_BYTES as u64 + 1);
                assert_eq!(max, MAX_STREAMED_FILE_BYTES);
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_transfer_estimate_is_honest_about_what_a_large_file_costs() {
        // P7's rule: never offer what the radio cannot deliver. A file that takes
        // twenty minutes should be presented as taking twenty minutes, and this is
        // the number a surface would show.
        let service = service(None, None);
        let (fast, slow) = service.file_transfer_estimate_secs(8 * 1024 * 1024);
        assert!(slow > fast, "the pessimistic figure is the slower one");
        assert!(
            fast > 10 * 60,
            "8 MiB is more than ten minutes even at the optimistic rate, got {fast}s"
        );

        // And a small file is seconds, so the estimate is not uniformly alarming.
        let (fast, _) = service.file_transfer_estimate_secs(4 * 1024);
        assert!(fast < 5, "a 4 KiB file is not a commitment, got {fast}s");
    }

    #[tokio::test]
    async fn an_empty_file_never_reaches_the_wire() {
        // `BitchatFilePacket::decode` rejects an empty content TLV outright, so an
        // empty file is airtime spent on a packet nobody can accept.
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
        let path = temp_file("empty.bin", 0);
        assert!(matches!(
            service.send_file(&path),
            Err(MeshError::EmptyContent)
        ));
    }

    #[tokio::test]
    async fn a_file_that_is_not_there_says_so_with_its_path() {
        // The two i/o failures this error covers are opposites — a send cannot
        // read what the user picked, a receive cannot write to the attachments
        // directory — and without the path the message cannot tell them apart.
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
        match service.send_file(std::path::Path::new("/nonexistent/nope.bin")) {
            Err(MeshError::FileIo { path, .. }) => {
                assert_eq!(path, "/nonexistent/nope.bin");
            }
            other => panic!("expected an i/o error naming the file, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_sent_file_lands_in_the_room_as_a_broadcast_and_not_as_sent() {
        // Same reason as `send_room`: nobody acknowledges a room file, so there is
        // no later event to correct the state with, and a message with no recorded
        // state falls through to the converter's one-to-one word "sent" — which
        // claims an arrival from a recipient the message never had.
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
        service.start().await.expect("start");
        let path = temp_file("note.bin", 32);

        service.send_file(&path).expect("queued");
        // The send is a queue submission; the runtime mirrors the row.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let history = service
            .history(&MeshConversation::Room, None, 50)
            .expect("history");
        assert_eq!(history.len(), 1, "the sender's own copy of the file");
        assert_eq!(
            history[0].delivery_status,
            MeshDelivery::Broadcast.as_str(),
            "a room file has no recipient to have been \"sent\" to"
        );
        assert_ne!(history[0].delivery_status, "sent");
        assert_eq!(
            mapping::attachments_of(&history[0]).len(),
            1,
            "the sender must be able to open what they just sent"
        );
        service.shutdown().await;
    }

    #[tokio::test]
    async fn sending_a_file_with_no_radio_fails_instead_of_queueing_forever() {
        let service = service(None, None);
        let path = temp_file("note.bin", 32);
        assert!(matches!(service.send_file(&path), Err(MeshError::NoRadio)));
    }

    #[tokio::test]
    async fn a_geohash_that_is_not_a_geohash_is_refused_before_it_is_published() {
        let service = service(
            None,
            Some(Arc::new(OfflineConnector) as Arc<dyn RelayConnector>),
        );
        assert!(matches!(
            service.join_geohash("not a geohash!"),
            Err(MeshError::InvalidGeohash(_))
        ));
        assert!(matches!(
            service.send_geohash("!!!", "hi"),
            Err(MeshError::InvalidGeohash(_))
        ));
    }

    #[tokio::test]
    async fn a_dm_to_an_unroutable_peer_fails_instead_of_queueing_forever() {
        // No radio, no relay connector: nothing can carry anything.
        let service = service(None, None);
        let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
        assert!(matches!(
            service.send_dm(&peer, "hello").await,
            Err(MeshError::Unroutable(_))
        ));
    }

    #[tokio::test]
    async fn a_dm_is_mirrored_into_the_timeline_before_it_is_routed() {
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
        let mut events = service.subscribe();
        let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");

        let delivery = service.send_dm(&peer, "hello").await.expect("routed");
        // Nothing is connected, so nothing can carry it — and the router says so
        // rather than pretending.
        assert_eq!(delivery, MeshDelivery::Unreachable);

        match events.try_recv() {
            Ok(MeshEvent::Message(message)) => {
                assert!(message.is_local);
                assert_eq!(message.conversation, MeshConversation::Dm(peer.clone()));
                assert_eq!(
                    message.message.conversation_type,
                    ConversationType::MeshDm,
                    "a 1:1 mesh conversation is MeshDm, not Mesh"
                );
                assert_eq!(mapping::text_of(&message.message), Some("hello"));
            }
            other => panic!("expected the local mirror first, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_sent_dm_is_on_disk_by_the_time_send_returns() {
        // No server echo will ever bring this message back, so the local copy is
        // the message — and it has to survive a restart mid-delivery.
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
        let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");

        service.send_dm(&peer, "hello").await.expect("routed");

        let conversation = MeshConversation::Dm(peer.clone());
        let history = service.history(&conversation, None, 50).expect("history");
        assert_eq!(history.len(), 1);
        assert_eq!(mapping::text_of(&history[0]), Some("hello"));
        assert_eq!(history[0].conversation_type, ConversationType::MeshDm);
        // The state is the mesh's own, and it is durable: it used to live only
        // on the event stream, so reopening the conversation brought a message
        // back as a plain server-style "sent" no matter what had really happened
        // to it. Nothing here can carry the message, so it is waiting.
        assert_eq!(
            history[0].delivery_status,
            MeshDelivery::Unreachable.as_str(),
            "mesh delivery state must survive a reload, in the mesh's vocabulary"
        );
        assert_ne!(
            history[0].delivery_status, "sent",
            "the server's \"sent\" means a server accepted it, which never happened"
        );

        // Typing the same text twice is two messages, not a duplicate: each send
        // mints its own wire ID. Dedup keys on that ID, and a resend carrying an
        // ID we already stored collapses onto the existing row instead — see
        // `timeline::tests::the_same_message_arriving_three_ways_is_stored_once`.
        service.send_dm(&peer, "hello").await.expect("routed");
        let history = service.history(&conversation, None, 50).expect("history");
        assert_eq!(history.len(), 2);
    }

    #[tokio::test]
    async fn the_public_room_is_a_conversation_before_anything_has_happened() {
        let service = service(None, None);
        let conversations = service.conversations().expect("conversations");
        assert_eq!(conversations.len(), 1, "just the public room");
        assert_eq!(conversations[0].conversation_type, ConversationType::Mesh);
        assert!(
            conversations[0].owner_id.is_none(),
            "no server account owns the mesh room"
        );
        assert!(conversations[0].my_role.is_empty());
    }

    #[tokio::test]
    async fn a_dm_shows_up_in_the_sidebar_once_it_has_been_used() {
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
        let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
        service.send_dm(&peer, "hello").await.expect("routed");

        let conversations = service.conversations().expect("conversations");
        assert_eq!(conversations.len(), 2, "the room and the dm");
        let dm = conversations
            .iter()
            .find(|c| c.conversation_type == ConversationType::MeshDm)
            .expect("the dm");
        assert_eq!(
            dm.id,
            MeshConversation::Dm(peer).conversation_id().to_string()
        );
        assert_eq!(dm.unread_count, 0, "our own message is already read");
    }

    #[tokio::test]
    async fn a_message_is_trimmed_before_it_is_sent() {
        let radio = MockLink::shared();
        let service = service(Some(Arc::clone(&radio) as Arc<dyn MeshLink>), None);
        let mut events = service.subscribe();
        let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
        service.send_dm(&peer, "  hello  ").await.expect("routed");
        match events.try_recv() {
            Ok(MeshEvent::Message(message)) => {
                assert_eq!(mapping::text_of(&message.message), Some("hello"));
            }
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_receipts_refuse_a_peer_no_transport_can_reach() {
        let service = service(None, None);
        let peer = PeerId::from_str("bbbbbbbbbbbbbbbb");
        assert!(matches!(
            service.mark_read(&peer, "MSG-1"),
            Err(MeshError::Unroutable(_))
        ));
    }

    #[tokio::test]
    async fn the_local_identity_is_stable_and_self_consistent() {
        let service = service(None, None);
        assert_eq!(service.local_peer(), service.local_peer());
        assert_eq!(service.fingerprint().len(), 64, "SHA-256 as hex");
        assert!(service.local_peer().is_hex());
    }

    #[tokio::test]
    async fn message_ids_are_unique_and_uppercase() {
        let a = new_message_id();
        let b = new_message_id();
        assert_ne!(a, b);
        assert_eq!(a, a.to_uppercase());
    }

    #[tokio::test]
    async fn a_fresh_service_reports_nothing_connected() {
        let service = service(None, None);
        let status = service.status();
        assert!(!status.radio_running);
        assert_eq!(status.connected_links, 0);
        assert_eq!(status.connected_relays, 0);
        assert!(service.peers().is_empty());
        assert!(service.joined_geohashes().is_empty());
    }
}
