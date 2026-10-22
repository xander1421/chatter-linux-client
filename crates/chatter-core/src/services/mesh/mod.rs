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
pub mod private_channel;
pub mod radio;
pub mod router;
pub mod session;
pub mod signal_mesh;
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

use crate::crypto::signal::store::SignalStore;
use crate::domain::UnifiedConversation;
use crate::domain::ids::{ChannelId, UserId};
use crate::domain::message::Message;
use crate::storage::KeyStore;
use crate::storage::mesh_binding_store::{BindingMethod, MeshBindingStore, PeerBinding};
use crate::storage::mesh_identity_store::MeshIdentityStore;
use crate::storage::repositories::MessageRepository;
use account_binding::{BindingQr, BindingV1, PROOF_TTL_SECS};
use private_channel::{Member, PendingLeave, PrivateChannel, PrivateChannelStore};

/// A mesh conversation with its sidebar state resolved by the mesh service.
pub struct MeshSidebarRow {
    pub conversation: MeshConversation,
    pub id: String,
    pub kind: &'static str,
    pub scope: String,
    pub title: String,
    pub last_message_at: Option<chrono::DateTime<chrono::Utc>>,
    pub unread_count: i32,
    pub reachable: bool,
    pub verified: bool,
    pub muted: bool,
    pub blocked: bool,
    pub binding_label: &'static str,
}

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
const DIRECT_INVITE_LIFETIME_SECS: u64 = 600;

/// A member the channel creator can remove, with the name shown in the roster.
pub struct PrivateChannelMember {
    pub peer_id: String,
    pub display_name: String,
    pub status_label: &'static str,
}

/// Everything [`MeshService`] needs from the rest of the client.
pub struct MeshDeps {
    /// Account key access is used only after a user selects a peer to share with.
    pub key_store: Arc<KeyStore>,
    pub binding_store: Arc<MeshBindingStore>,
    pub private_channels: Arc<PrivateChannelStore>,
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
    private_channels: Arc<PrivateChannelStore>,
    signal_lock: Arc<Mutex<()>>,
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
    /// Set by a panic wipe. `identity` above is the erased keypair; announcing
    /// it again would relink this device to everything the wipe was meant to
    /// cut off, so the radio stays down until a restart loads a fresh one.
    identity_wiped: AtomicBool,
    /// Set when the sealer holds no key — see [`MeshError::SealerUnavailable`].
    sealer_unavailable: Option<String>,
}

mod account_identity;
mod channels;
mod control;
mod conversations;
mod files;
mod lifecycle;
mod messaging;

/// A fresh wire message ID.
///
/// Uppercase UUID, matching what bitchat puts on the wire, so an ID this client
/// mints is indistinguishable from one theirs did.
fn new_message_id() -> String {
    Uuid::new_v4().to_string().to_uppercase()
}

#[cfg(test)]
mod tests;
