//! [`AndroidLink`] — the [`MeshLink`] implementation, and the state the JNI
//! callbacks feed into.
//!
//! This module owns the two things the rest of the crate borrows:
//!
//! - the **connection table**, mapping [`LinkId`] to the Kotlin handle we write
//!   on, and
//! - the **event queue** that [`MeshLink::next_event`] drains.
//!
//! [`crate::bridge`] and [`crate::callbacks`] do the JNI work and report in
//! here; neither of them decides what a link id is or when an event is emitted.
//! Everything in this module that is not a bridge call is a pure state
//! transition, which is what makes the lifecycle testable without a radio.
//!
//! # A link exists only once it can carry a write
//!
//! [`LinkEvent::Connected`] promises the link is writable *now*, so this module
//! withholds it until Kotlin says the write path exists:
//!
//! - **Central**: after `onServicesDiscovered` *and* the descriptor write that
//!   enables notifications has completed. An Android GATT connection by itself
//!   carries nothing.
//! - **Peripheral**: when the remote central subscribes with a CCCD write.
//!   Before that there is nobody to notify — an inbound write channel is not a
//!   link, because it only goes one way.
//!
//! That obligation is shared with the Kotlin half: `nativeOnConnected` must not
//! fire at `onConnectionStateChange`. The two files are read together, so it is
//! spelled out on both sides. A peer may write before it subscribes; those
//! early bytes are held in a small per-link buffer and replayed immediately
//! after `Connected`, so no `Data` event ever names a link the layer above has
//! not been told about, and a packet that arrived a millisecond early is not
//! simply lost — nothing below us retransmits.
//!
//! # Link identity
//!
//! A [`LinkId`] here is `{kotlin handle}#{sequence}`, for example
//! `c:AA:BB:CC:DD:EE:FF#3`. Three parts, three reasons, the same as on Linux:
//!
//! - **role**, carried by Kotlin's own `c:`/`p:` prefix, because two devices
//!   running the mesh will often connect to each other in both directions at
//!   once, and those are two links with two independent send paths;
//! - **address**, because an opaque integer is unreadable in a log next to a
//!   `logcat` trace of the Kotlin side;
//! - **sequence**, added here, because ids must never be reused. Kotlin's
//!   handle is stable across incarnations — it is derived from the address —
//!   so without a sequence a link that drops and immediately re-establishes
//!   would produce `Connected(X)` for an id the layer above still holds state
//!   for, and a late `Disconnected(X)` from the old incarnation would then
//!   destroy the new one's state.
//!
//! None of this makes a link id a peer identity. A link is bound to a peer only
//! after that peer proves who it is with a signed announce, several layers
//! above here; conflating the two is what lets a replayed announce steal a
//! link.
//!
//! # What the Kotlin handle has to guarantee
//!
//! `nativeOnData` and `nativeOnDisconnected` carry a handle and nothing else,
//! so a handle must name exactly one connection. Kotlin's `c:`/`p:` prefix is
//! what provides that: two devices meshing with each other are routinely
//! connected both ways, and one handle for both would deliver inbound bytes on
//! whichever link happened to be found first.
//!
//! # Lifetime of this object
//!
//! [`MeshLink::stop`] is terminal: after it, `next_event` drains whatever is
//! left and then resolves to `None` forever. Restarting the radio means
//! constructing a new `AndroidLink`. The duty cycle does *not* go through
//! `stop` — that is [`MeshLink::set_scanning`], which leaves the peripheral
//! role and every established link untouched.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use chatter_mesh_link::{BoxFuture, LinkError, LinkEvent, LinkId, LinkRole, MeshLink};
use tokio::sync::Notify;

use crate::bridge;
use crate::error;

/// Soft event limit: evict the oldest data/discovery event under pressure.
/// Lifecycle-only bursts may exceed it so the policy layer never retains a
/// phantom link or misses a new connection. Binder callbacks never await space.
const EVENT_QUEUE_CAPACITY: usize = 256;

/// How many inbound packets to hold for a connection that has delivered bytes
/// but is not writable yet.
///
/// The window is normally sub-millisecond — a central subscribes immediately
/// after discovering the characteristic — so this is a safety margin, not a
/// queue. It is bounded because a peer that writes and never subscribes would
/// otherwise grow it without limit; past the bound the oldest packet is
/// dropped, since in a mesh the oldest is the one most likely to be stale.
const PENDING_PACKETS_PER_LINK: usize = 16;

/// Bytes of ATT overhead on every write: one opcode, two for the handle.
pub const ATT_WRITE_OVERHEAD: i32 = 3;

/// The ATT MTU every BLE connection starts on, per the Bluetooth spec.
pub const MIN_ATT_MTU: i32 = 23;

/// The largest ATT MTU Android will negotiate, and what we ask for on connect.
pub const MAX_ATT_MTU: i32 = 517;

/// Payload bytes available on a link whose ATT MTU is `mtu`.
///
/// The one number in this file that must never be a constant or a guess:
/// fragment sizing depends on it, and an oversized write is dropped by the
/// controller with no error reported anywhere. Clamped at both ends because a
/// value outside the spec's range says the reporter is confused, not that the
/// radio can do something impossible.
pub const fn max_write_for_mtu(mtu: i32) -> usize {
    // Saturating, because this is fed straight from a number Kotlin reported:
    // `i32::MIN - 3` is a debug-build panic, and a panic here would be reached
    // from a binder thread.
    let usable = mtu.saturating_sub(ATT_WRITE_OVERHEAD);
    if usable < MIN_ATT_MTU - ATT_WRITE_OVERHEAD {
        (MIN_ATT_MTU - ATT_WRITE_OVERHEAD) as usize
    } else if usable > MAX_ATT_MTU - ATT_WRITE_OVERHEAD {
        (MAX_ATT_MTU - ATT_WRITE_OVERHEAD) as usize
    } else {
        usable as usize
    }
}

/// What a link can carry before `onMtuChanged` has been heard from: 23 − 3.
///
/// Assumed for every new link. Starting optimistic and correcting downward
/// would mean silently truncating every packet sent in the meantime.
pub const DEFAULT_MAX_WRITE: usize = max_write_for_mtu(MIN_ATT_MTU);

/// The most any link can carry: 517 − 3.
pub const LARGEST_MAX_WRITE: usize = max_write_for_mtu(MAX_ATT_MTU);

/// Constrain a size reported by Kotlin to something a BLE link can actually be.
///
/// Clamping *up* to [`DEFAULT_MAX_WRITE`] is safe rather than optimistic: the
/// Bluetooth spec guarantees an ATT MTU of at least 23, so 20 payload bytes
/// always fit. Clamping *down* to [`LARGEST_MAX_WRITE`] is the important half —
/// an inflated number would be believed by the fragmenter and every packet
/// built from it would vanish inside the controller.
pub fn sanitise_max_write(reported: i32) -> usize {
    reported.clamp(DEFAULT_MAX_WRITE as i32, LARGEST_MAX_WRITE as i32) as usize
}

/// Short, stable text for a role, used in link ids.
fn role_tag(role: LinkRole) -> &'static str {
    match role {
        LinkRole::Central => "central",
        LinkRole::Peripheral => "peripheral",
    }
}

/// One line describing an event, for logs.
///
/// Never includes payload bytes. Mesh traffic is other people's ciphertext, and
/// a logcat buffer is readable by anything holding `READ_LOGS` on older
/// devices, shipped in bug reports, and no help in debugging anyway — the
/// length is the part that matters.
fn describe(event: &LinkEvent) -> String {
    match event {
        LinkEvent::Discovered { address, rssi, .. } => format!("Discovered({address}, {rssi} dBm)"),
        LinkEvent::Connected {
            link,
            role,
            max_write,
        } => {
            format!(
                "Connected({link}, {}, max_write {max_write})",
                role_tag(*role)
            )
        }
        LinkEvent::Disconnected { link } => format!("Disconnected({link})"),
        LinkEvent::Data { link, bytes } => format!("Data({link}, {} bytes)", bytes.len()),
        LinkEvent::AdapterPowerChanged { powered } => format!("AdapterPowerChanged({powered})"),
        // The Android bridge does not emit this: its GATT server outlives the
        // process, and there is no equivalent of BlueZ closing a control stream
        // underneath us. Named anyway, so adding the case here is a deliberate act
        // rather than something the compiler stops asking about.
        LinkEvent::PeripheralRoleEnded => "PeripheralRoleEnded".to_string(),
    }
}

/// One connection, as this crate sees it.
struct Conn {
    role: LinkRole,
    /// The opaque handle Kotlin names this connection by.
    handle: String,
    /// Largest single write this link accepts, as last reported.
    max_write: usize,
    /// Has the layer above been given this id?
    ///
    /// True as soon as the id leaves this crate — which happens either when
    /// `Connected` is emitted or when [`MeshLink::connect`] returns the id of a
    /// link that is not writable yet. Only a visible link may produce a
    /// `Disconnected`; telling the caller about the death of an id it never saw
    /// names state it does not have.
    visible: bool,
    /// Has `Connected` been emitted, i.e. is the link writable?
    ///
    /// Distinct from `visible` precisely because of the connect path: an
    /// outbound attempt hands its id back immediately, but bytes must not flow
    /// on it until Kotlin confirms the write path exists.
    writable: bool,
}

/// A newly opened, not-yet-writable link, plus whatever it displaced.
struct Opened {
    id: LinkId,
    displaced: Option<LinkId>,
}

/// A link that has just become writable.
struct Established {
    id: LinkId,
    displaced: Option<LinkId>,
    /// Bytes that arrived before the link was writable, oldest first.
    flushed: Vec<Vec<u8>>,
}

/// The [`LinkId`] → connection map, plus the pre-`Connected` inbound buffer.
///
/// Deliberately free of any JNI: everything here is a pure state transition.
#[derive(Default)]
struct Connections {
    by_id: HashMap<LinkId, Conn>,
    /// Inbound packets held until the link they belong to is writable, keyed by
    /// Kotlin handle. See [`PENDING_PACKETS_PER_LINK`].
    pending: HashMap<String, VecDeque<Vec<u8>>>,
    next_seq: u64,
}

impl Connections {
    /// Insert a link that is not writable yet.
    ///
    /// Returns the new id and, if a link already existed for this handle and
    /// the layer above knew about it, the id it displaced — Kotlin reusing a
    /// handle without reporting the old connection closed means the old one is
    /// dead.
    fn open(&mut self, role: LinkRole, handle: &str, max_write: usize, visible: bool) -> Opened {
        let mut displaced = None;
        if let Some(old) = self.find(handle) {
            if let Some(conn) = self.by_id.remove(&old) {
                if conn.visible {
                    displaced = Some(old);
                }
            }
        }

        self.next_seq += 1;
        let id = LinkId::new(format!("{}#{}", handle, self.next_seq));
        self.by_id.insert(
            id.clone(),
            Conn {
                role,
                handle: handle.to_string(),
                max_write,
                visible,
                writable: false,
            },
        );
        Opened { id, displaced }
    }

    /// Register an outbound attempt, or find the one already running.
    ///
    /// Kotlin's `connect` is idempotent: asking twice for the same address
    /// hands back the same id, and the link it names may already be live.
    /// Displacing it — which is what [`Connections::open`] would do — would
    /// kill a working link and leave the caller holding an id for a connection
    /// nobody is going to establish a second time.
    fn open_outbound(&mut self, handle: &str, max_write: usize) -> Opened {
        if let Some(id) = self.find(handle) {
            if let Some(conn) = self.by_id.get_mut(&id) {
                conn.visible = true;
            }
            return Opened {
                id,
                displaced: None,
            };
        }
        self.open(LinkRole::Central, handle, max_write, true)
    }

    /// Make the link for `handle` writable, opening one if there is none.
    ///
    /// An outbound attempt already sits in the table — unwritable, with its id
    /// in the caller's hands — so this reuses it rather than minting a second
    /// id for the same connection.
    fn establish(&mut self, role: LinkRole, handle: &str, max_write: usize) -> Established {
        let pending_outbound = self.find(handle).filter(|id| {
            self.by_id
                .get(id)
                .is_some_and(|conn| conn.role == role && !conn.writable)
        });

        let (id, displaced) = match pending_outbound {
            Some(id) => (id, None),
            None => {
                let Opened { id, displaced } = self.open(role, handle, max_write, true);
                (id, displaced)
            }
        };

        if let Some(conn) = self.by_id.get_mut(&id) {
            conn.max_write = max_write;
            conn.visible = true;
            conn.writable = true;
        }
        Established {
            id,
            displaced,
            flushed: self.take_pending(handle),
        }
    }

    /// The link currently representing `handle`, if any.
    fn find(&self, handle: &str) -> Option<LinkId> {
        self.by_id
            .iter()
            .find(|(_, conn)| conn.handle == handle)
            .map(|(id, _)| id.clone())
    }

    /// Everything held back for `handle`, oldest first.
    fn take_pending(&mut self, handle: &str) -> Vec<Vec<u8>> {
        self.pending
            .remove(handle)
            .map(|queue| queue.into_iter().collect())
            .unwrap_or_default()
    }

    /// Route one inbound payload.
    ///
    /// Returns the link to emit it on, or buffers it when the link is not
    /// writable yet.
    fn accept_inbound(&mut self, handle: &str, bytes: Vec<u8>) -> Option<(LinkId, Vec<u8>)> {
        if let Some(id) = self.find(handle) {
            if self.by_id.get(&id).is_some_and(|conn| conn.writable) {
                return Some((id, bytes));
            }
        }

        let queue = self.pending.entry(handle.to_string()).or_default();
        if queue.len() >= PENDING_PACKETS_PER_LINK {
            queue.pop_front();
        }
        queue.push_back(bytes);
        None
    }

    /// Remove the link for `handle`. Returns its id only if the layer above
    /// knew about it.
    fn close(&mut self, handle: &str) -> Option<LinkId> {
        self.pending.remove(handle);
        let id = self.find(handle)?;
        let conn = self.by_id.remove(&id)?;
        conn.visible.then_some(id)
    }

    /// Remove by id. Returns the entry so the caller can act on its handle.
    fn remove(&mut self, id: &LinkId) -> Option<Conn> {
        let conn = self.by_id.remove(id)?;
        self.pending.remove(&conn.handle);
        Some(conn)
    }

    /// Remove every link, returning the ones the layer above knows about.
    fn drain(&mut self) -> Vec<LinkId> {
        self.pending.clear();
        self.by_id
            .drain()
            .filter(|(_, conn)| conn.visible)
            .map(|(id, _)| id)
            .collect()
    }

    fn max_write(&self, id: &LinkId) -> Option<usize> {
        self.by_id.get(id).map(|conn| conn.max_write)
    }

    /// Record a size Kotlin has just reported. No-op for a link that is gone.
    fn set_max_write(&mut self, id: &LinkId, max_write: usize) {
        if let Some(conn) = self.by_id.get_mut(id) {
            conn.max_write = max_write;
        }
    }

    fn handle(&self, id: &LinkId) -> Option<String> {
        self.by_id.get(id).map(|conn| conn.handle.clone())
    }

    /// Everything `send` needs, taken under one lock.
    fn target(&self, id: &LinkId) -> Option<(String, usize)> {
        self.by_id
            .get(id)
            .map(|conn| (conn.handle.clone(), conn.max_write))
    }

    fn len(&self) -> usize {
        self.by_id.len()
    }
}

/// State shared between the link object and the JNI callbacks.
struct Shared {
    conns: Mutex<Connections>,
    queue: Mutex<(std::collections::VecDeque<LinkEvent>, bool)>,
    available: Notify,
    capacity: usize,
}

impl Shared {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            conns: Mutex::new(Connections::default()),
            queue: Mutex::new((std::collections::VecDeque::new(), false)),
            available: Notify::new(),
            capacity,
        })
    }

    fn conns(&self) -> MutexGuard<'_, Connections> {
        // A panic on a binder thread must not turn every later lock into a
        // panic of its own; the map's invariants do not depend on unwinding.
        self.conns.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stopped(&self) -> bool {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner).1
    }

    fn close_queue(&self) {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
        self.available.notify_one();
    }

    // Data/discovery pressure must never lose link lifecycle transitions.
    // If only lifecycle events remain, allow them beyond the soft capacity.
    fn emit(&self, event: LinkEvent) {
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if queue.1 {
            return;
        }
        if queue.0.len() >= self.capacity {
            if let Some(index) = queue.0.iter().position(|event| !lifecycle(event)) {
                queue.0.remove(index);
            } else if !lifecycle(&event) {
                return;
            }
        }
        queue.0.push_back(event);
        drop(queue);
        self.available.notify_one();
    }

    /// Register an outbound attempt and hand back its id.
    ///
    /// The id is visible immediately — the caller is holding it — but not
    /// writable: `Connected` waits for Kotlin to confirm the write path.
    fn open_central(&self, handle: &str) -> LinkId {
        let Opened { id, displaced } = { self.conns().open_outbound(handle, DEFAULT_MAX_WRITE) };
        if let Some(old) = displaced {
            self.emit(LinkEvent::Disconnected { link: old });
        }
        id
    }

    /// Kotlin says this connection is writable now.
    fn establish(&self, role: LinkRole, handle: &str, max_write: usize) {
        let Established {
            id,
            displaced,
            flushed,
        } = { self.conns().establish(role, handle, max_write) };
        if let Some(old) = displaced {
            self.emit(LinkEvent::Disconnected { link: old });
        }

        self.emit(LinkEvent::Connected {
            link: id.clone(),
            role,
            max_write,
        });
        // Only now may inbound data be released. Doing it in this order is what
        // guarantees `Data` never precedes `Connected` for a link.
        for bytes in flushed {
            self.emit(LinkEvent::Data {
                link: id.clone(),
                bytes,
            });
        }
    }

    /// Route one inbound payload, holding it back if the link is not writable.
    fn inbound(&self, handle: &str, bytes: Vec<u8>) {
        let routed = { self.conns().accept_inbound(handle, bytes) };
        if let Some((link, bytes)) = routed {
            self.emit(LinkEvent::Data { link, bytes });
        }
    }

    /// Kotlin says this connection is gone.
    fn close_link(&self, handle: &str) {
        let gone = { self.conns().close(handle) };
        if let Some(id) = gone {
            self.emit(LinkEvent::Disconnected { link: id });
        }
    }

    /// Drop a link by id and announce it.
    fn drop_link(&self, id: &LinkId) {
        let removed = { self.conns().remove(id) };
        if removed.is_some_and(|conn| conn.visible) {
            self.emit(LinkEvent::Disconnected { link: id.clone() });
        }
    }

    /// Drop every link and announce each one. Used when the adapter dies under
    /// us and on shutdown.
    fn drop_all(&self) {
        let ids = { self.conns().drain() };
        for id in ids {
            self.emit(LinkEvent::Disconnected { link: id });
        }
    }
}

/// The link the JNI callbacks currently report into.
///
/// A `Weak`, not an `Arc`: the native method table is registered once for the
/// whole process, so callbacks have to find their way to a link through a
/// global — but holding a strong reference here would keep a dropped
/// `AndroidLink` alive forever, still advertising, with nobody draining it.
static ACTIVE: Mutex<Option<Weak<Shared>>> = Mutex::new(None);

fn set_active(shared: &Arc<Shared>) {
    *ACTIVE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(shared));
}

fn clear_active(shared: &Arc<Shared>) {
    let mut slot = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
    let ours = slot
        .as_ref()
        .and_then(Weak::upgrade)
        .is_some_and(|current| Arc::ptr_eq(&current, shared));
    if ours {
        *slot = None;
    }
}

/// The link a callback should report into, if there is one.
fn active(during: &str) -> Option<Arc<Shared>> {
    let shared = ACTIVE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(Weak::upgrade);
    if shared.is_none() {
        error::log_warn(&format!(
            "mesh: {during} arrived with no AndroidLink attached, dropped"
        ));
    }
    shared
}

/// Push one fully-formed event and apply any state change it implies.
///
/// Used for the two events that need nothing from the connection table.
pub(crate) fn dispatch(event: LinkEvent) {
    let Some(shared) = active(&describe(&event)) else {
        return;
    };
    let powered_off = matches!(event, LinkEvent::AdapterPowerChanged { powered: false });
    shared.emit(event);
    if powered_off {
        // Every connection died with the controller. Say so once per link
        // rather than leaving the layer above to discover it one failed write
        // at a time.
        shared.drop_all();
    }
}

/// `nativeOnConnected`: this connection is writable now.
pub(crate) fn on_connected(handle: &str, role: LinkRole, reported_max_write: i32) {
    let Some(shared) = active("nativeOnConnected") else {
        return;
    };
    shared.establish(role, handle, sanitise_max_write(reported_max_write));
}

/// `nativeOnDisconnected`: this connection is gone.
pub(crate) fn on_disconnected(handle: &str) {
    let Some(shared) = active("nativeOnDisconnected") else {
        return;
    };
    shared.close_link(handle);
}

/// `nativeOnData`: bytes arrived on this connection.
pub(crate) fn on_data(handle: &str, bytes: Vec<u8>) {
    let Some(shared) = active("nativeOnData") else {
        return;
    };
    shared.inbound(handle, bytes);
}

/// The Android radio, driven through a Kotlin `MeshBleBridge`.
///
/// Runs the central and peripheral roles together, as the mesh requires. Only
/// one instance can be receiving callbacks at a time: the native method table
/// belongs to the process, so constructing a second `AndroidLink` takes the
/// callbacks over from the first.
pub struct AndroidLink {
    inner: Arc<Shared>,
}

impl AndroidLink {
    /// No JNI is touched until [`MeshLink::start`], beyond taking over the
    /// callback registration so that nothing arriving in between is lost.
    pub fn new() -> Self {
        Self::with_queue_capacity(EVENT_QUEUE_CAPACITY)
    }

    fn with_queue_capacity(capacity: usize) -> Self {
        let inner = Shared::new(capacity);
        set_active(&inner);
        Self { inner }
    }

    /// Number of links currently established. Diagnostics only.
    pub fn link_count(&self) -> usize {
        self.inner.conns().len()
    }

    /// Ask Kotlin for the live size of a link, recording whatever it says.
    ///
    /// Polling is the only way an MTU renegotiation reaches Rust: there is no
    /// `nativeOnMtuChanged`, and `onMtuChanged` routinely lands after the link
    /// is already writable. Returns the cached size when Kotlin has nothing
    /// better to offer.
    fn refresh_max_write(&self, link: &LinkId) -> Option<usize> {
        let handle = self.inner.conns().handle(link)?;
        if !bridge::is_installed() {
            return self.inner.conns().max_write(link);
        }
        match bridge::max_write(&handle) {
            Some(reported) => {
                let live = sanitise_max_write(reported);
                self.inner.conns().set_max_write(link, live);
                Some(live)
            }
            None => self.inner.conns().max_write(link),
        }
    }
}

impl Default for AndroidLink {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AndroidLink {
    fn drop(&mut self) {
        // Leaving a dangling registration behind would make every later
        // callback log a warning about a link that no longer exists.
        clear_active(&self.inner);
    }
}

impl core::fmt::Debug for AndroidLink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AndroidLink")
            .field("links", &self.link_count())
            .field("stopped", &self.inner.stopped())
            .finish()
    }
}

impl MeshLink for AndroidLink {
    fn start(&self) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async move {
            if self.inner.stopped() {
                return Err(error::stopped());
            }
            // Reassert the registration in case another link was constructed
            // and dropped in between; callbacks must reach whoever is running.
            set_active(&self.inner);
            bridge::start()
        })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async move {
            // Best effort: the local link layer is stopping either way, and
            // reporting an error for a radio that is already unreachable would
            // only make callers handle a failure they cannot act on.
            if let Err(err) = bridge::stop() {
                error::log_warn(&format!("mesh: MeshBleBridge.stop() failed: {err}"));
            }

            // Announce the dead links while the queue is still open.
            self.inner.drop_all();
            clear_active(&self.inner);
            self.inner.close_queue();
            Ok(())
        })
    }

    fn set_scanning(&self, on: bool) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async move {
            if !on && (self.inner.stopped() || !bridge::is_installed()) {
                // The duty cycle calls this unconditionally, so turning
                // scanning off with nothing running is a no-op rather than an
                // error, exactly as on Linux.
                return Ok(());
            }
            if self.inner.stopped() {
                return Err(error::stopped());
            }
            bridge::set_scanning(on)
        })
    }

    fn set_local_peer(&self, peer: [u8; 8]) {
        // Best effort: without it peers cannot recognise us across an address
        // rotation, which is a churn problem, not a broken radio. Said at warn
        // because the symptom it produces — a peer reconnecting endlessly under
        // new addresses — looks like anything but a missing advertisement field.
        let hex: String = peer.iter().map(|byte| format!("{byte:02x}")).collect();
        if let Err(error) = bridge::set_local_peer(&hex) {
            error::log_warn(&format!(
                "mesh: could not advertise our peer id; peers will not recognise us \
                 across an address rotation: {error}"
            ));
        }
    }

    fn connect(&self, address: &str) -> BoxFuture<'_, Result<LinkId, LinkError>> {
        let address = address.to_string();
        Box::pin(async move {
            if self.inner.stopped() {
                return Err(error::stopped());
            }

            let handle = bridge::connect(&address)?;
            if handle.is_empty() {
                return Err(error::connect_failed(
                    &address,
                    "connect() returned an empty handle, which names no connection",
                ));
            }

            // The id exists now; the link is not writable yet. `Connected`
            // follows when Kotlin reports the write path is up, and a failed
            // attempt still produces `Disconnected` for this id — the caller is
            // holding it either way.
            Ok(self.inner.open_central(&handle))
        })
    }

    fn disconnect(&self, link: &LinkId) -> BoxFuture<'_, Result<(), LinkError>> {
        let link = link.clone();
        Box::pin(async move {
            let removed = { self.inner.conns().remove(&link) };
            let Some(conn) = removed else {
                return Err(error::link_gone(&link));
            };
            if conn.visible {
                self.inner.emit(LinkEvent::Disconnected { link });
            }

            // Best effort, and after the event: the link is gone from our point
            // of view whatever Kotlin does with the handle, and a failure here
            // must not look like a failure to disconnect.
            if let Err(err) = bridge::disconnect(&conn.handle) {
                error::log_warn(&format!("mesh: disconnect({}) failed: {err}", conn.handle));
            }
            Ok(())
        })
    }

    fn send(&self, link: &LinkId, bytes: &[u8]) -> BoxFuture<'_, Result<(), LinkError>> {
        let link = link.clone();
        let bytes = bytes.to_vec();
        Box::pin(async move {
            let target = { self.inner.conns().target(&link) };
            let Some((handle, cached)) = target else {
                return Err(error::link_gone(&link));
            };

            // Only ask Kotlin when the cached size would reject the payload: an
            // MTU that grew after `Connected` is invisible until something asks,
            // and rejecting a packet the link could actually carry is a silent
            // throughput collapse rather than an error anyone would notice.
            let mut max = cached;
            if bytes.len() > max {
                if let Some(live) = self.refresh_max_write(&link) {
                    max = live;
                }
            }
            if bytes.len() > max {
                return Err(LinkError::PayloadTooLarge {
                    actual: bytes.len(),
                    max,
                });
            }

            match bridge::send(&handle, &bytes) {
                Ok(()) => Ok(()),
                Err(err) => {
                    if error::is_terminal(&err) {
                        self.inner.drop_link(&link);
                    }
                    Err(err)
                }
            }
        })
    }

    fn max_write(&self, link: &LinkId) -> Option<usize> {
        self.refresh_max_write(link)
    }

    fn next_event(&self) -> BoxFuture<'_, Option<LinkEvent>> {
        Box::pin(async move {
            loop {
                let available = self.inner.available.notified();
                {
                    let mut queue = self
                        .inner
                        .queue
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if let Some(event) = queue.0.pop_front() {
                        return Some(event);
                    }
                    if queue.1 {
                        return None;
                    }
                }
                available.await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Handles as the Kotlin side mints them: `c:` for a connection we opened,
    /// `p:` for one a remote central opened to us, address uppercased. Distinct
    /// by construction, which is the contract the module docs spell out — note
    /// that `PEER` and `INBOUND` are the *same device* in both directions.
    const PEER: &str = "c:AA:BB:CC:DD:EE:FF";
    const OTHER: &str = "c:11:22:33:44:55:66";
    const INBOUND: &str = "p:AA:BB:CC:DD:EE:FF";

    fn table() -> Connections {
        Connections::default()
    }

    /// A link with a queue small enough to overflow on purpose.
    ///
    /// Deliberately not registered as the active link: these tests exercise the
    /// object, and the global registration is process-wide, so racing over it
    /// would make them order-dependent.
    fn shared(capacity: usize) -> Arc<Shared> {
        Shared::new(capacity)
    }

    /// Everything currently queued. Needs no runtime — `try_recv` is
    /// synchronous, which is what keeps these tests executor-free.
    fn drain_events(shared: &Shared) -> Vec<LinkEvent> {
        let mut queue = shared.queue.lock().expect("queue lock");
        let mut events = Vec::new();
        while let Some(event) = queue.0.pop_front() {
            events.push(event);
        }
        events
    }

    fn block_on<F: core::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime")
            .block_on(future)
    }

    // -- MTU arithmetic ----------------------------------------------------

    #[test]
    fn max_write_is_the_negotiated_mtu_minus_the_att_header() {
        assert_eq!(max_write_for_mtu(23), 20);
        assert_eq!(max_write_for_mtu(185), 182);
        assert_eq!(max_write_for_mtu(247), 244);
        assert_eq!(max_write_for_mtu(517), 514);
    }

    #[test]
    fn the_default_is_the_spec_minimum_not_an_optimistic_guess() {
        // Until onMtuChanged lands every link is a 23-byte link. Assuming more
        // means silently truncated packets for the whole negotiation window.
        assert_eq!(DEFAULT_MAX_WRITE, 20);
        assert_eq!(LARGEST_MAX_WRITE, 514);
    }

    #[test]
    fn an_impossible_mtu_is_clamped_into_the_spec_range() {
        // Below the spec minimum, 20 always fits, so clamping up is safe.
        assert_eq!(max_write_for_mtu(0), DEFAULT_MAX_WRITE);
        assert_eq!(max_write_for_mtu(i32::MIN), DEFAULT_MAX_WRITE);
        // Above it, believing the number is what loses packets.
        assert_eq!(max_write_for_mtu(4096), LARGEST_MAX_WRITE);
        assert_eq!(max_write_for_mtu(i32::MAX), LARGEST_MAX_WRITE);
    }

    #[test]
    fn a_size_reported_by_kotlin_is_never_believed_beyond_the_spec() {
        assert_eq!(sanitise_max_write(244), 244);
        assert_eq!(sanitise_max_write(20), 20);
        assert_eq!(sanitise_max_write(0), DEFAULT_MAX_WRITE);
        assert_eq!(sanitise_max_write(-1), DEFAULT_MAX_WRITE);
        assert_eq!(sanitise_max_write(i32::MIN), DEFAULT_MAX_WRITE);
        assert_eq!(sanitise_max_write(100_000), LARGEST_MAX_WRITE);
    }

    // -- the link id table -------------------------------------------------

    #[test]
    fn link_ids_name_the_role_the_handle_and_a_sequence() {
        let mut conns = table();
        let opened = conns.open(LinkRole::Central, PEER, DEFAULT_MAX_WRITE, true);
        assert!(opened.displaced.is_none());
        assert_eq!(opened.id.as_str(), "c:AA:BB:CC:DD:EE:FF#1");
    }

    #[test]
    fn the_same_peer_in_both_directions_gets_two_distinct_links() {
        // Two devices running the mesh routinely connect to each other at once.
        // Kotlin gives each direction its own handle, which is what keeps them
        // apart here — see the module docs.
        let mut conns = table();
        let outbound = conns.open(LinkRole::Central, PEER, 244, true);
        let inbound = conns.open(LinkRole::Peripheral, INBOUND, 244, true);
        assert!(
            inbound.displaced.is_none(),
            "the inbound link must not displace the outbound one"
        );
        assert_ne!(outbound.id, inbound.id);
        assert_eq!(conns.len(), 2);
    }

    #[test]
    fn a_reestablished_link_never_reuses_an_id() {
        // Reuse would let a late Disconnected from the old incarnation destroy
        // state belonging to the new one.
        let mut conns = table();
        let first = conns.establish(LinkRole::Central, PEER, 244).id;
        assert_eq!(conns.close(PEER).as_ref(), Some(&first));
        let second = conns.establish(LinkRole::Central, PEER, 244).id;
        assert_ne!(first, second);
    }

    #[test]
    fn reusing_a_handle_displaces_the_stale_link() {
        let mut conns = table();
        let first = conns.establish(LinkRole::Peripheral, INBOUND, 244).id;
        let second = conns.establish(LinkRole::Peripheral, INBOUND, 185);
        assert_eq!(second.displaced, Some(first));
        assert_eq!(conns.len(), 1);
        assert_eq!(conns.max_write(&second.id), Some(185));
    }

    #[test]
    fn an_outbound_attempt_is_announced_under_the_id_the_caller_already_holds() {
        // connect() hands the id back before the link is writable. Minting a
        // second id at Connected would leave the caller holding a link that
        // never comes up and never dies.
        let mut conns = table();
        let handed_out = conns.open_outbound(PEER, DEFAULT_MAX_WRITE).id;
        let established = conns.establish(LinkRole::Central, PEER, 244);
        assert_eq!(established.id, handed_out);
        assert!(established.displaced.is_none());
        assert_eq!(conns.len(), 1);
        assert_eq!(conns.max_write(&handed_out), Some(244));
    }

    #[test]
    fn connecting_twice_to_the_same_peer_returns_the_link_that_already_exists() {
        // Kotlin's connect() is idempotent and hands back the same id. Minting a
        // second one here would displace a live link and leave the caller
        // holding an id nothing will ever establish.
        let mut conns = table();
        let first = conns.open_outbound(PEER, DEFAULT_MAX_WRITE).id;
        conns.establish(LinkRole::Central, PEER, 244);

        let again = conns.open_outbound(PEER, DEFAULT_MAX_WRITE);
        assert_eq!(again.id, first);
        assert!(
            again.displaced.is_none(),
            "the live link must survive a second connect"
        );
        assert_eq!(conns.len(), 1);
        assert_eq!(
            conns.max_write(&first),
            Some(244),
            "and must keep its negotiated size"
        );
    }

    #[test]
    fn an_inbound_connection_on_a_handle_awaiting_an_outbound_one_does_not_steal_its_id() {
        // Kotlin's `c:`/`p:` prefixes make this impossible today. If that ever
        // stops being true, the two roles must still not be merged into one
        // link: they have two independent send paths.
        let mut conns = table();
        let outbound = conns.open_outbound(PEER, DEFAULT_MAX_WRITE).id;
        let established = conns.establish(LinkRole::Peripheral, PEER, 244);
        assert_ne!(established.id, outbound);
        assert_eq!(
            established.displaced,
            Some(outbound),
            "the outbound id was visible, so it dies"
        );
    }

    #[test]
    fn a_link_the_caller_never_saw_produces_no_disconnected() {
        let mut conns = table();
        conns.open(LinkRole::Peripheral, INBOUND, 244, false);
        assert_eq!(conns.close(INBOUND), None);
        assert_eq!(conns.len(), 0);
        assert!(conns.drain().is_empty());
    }

    #[test]
    fn an_outbound_attempt_that_never_becomes_writable_still_produces_disconnected() {
        // The whole reason visibility is tracked separately from writability.
        // The caller is holding this id; if it dies silently, the state keyed on
        // it is never released.
        let mut conns = table();
        let id = conns.open_outbound(PEER, DEFAULT_MAX_WRITE).id;
        assert_eq!(conns.close(PEER), Some(id));
    }

    #[test]
    fn closing_a_handle_that_is_not_there_is_harmless() {
        let mut conns = table();
        assert_eq!(conns.close(PEER), None);
    }

    #[test]
    fn inbound_data_before_the_link_is_writable_is_held_not_dropped() {
        // A central may write on the characteristic before it subscribes.
        // Nothing below us retransmits, so those bytes have to survive the gap.
        let mut conns = table();
        assert_eq!(conns.accept_inbound(INBOUND, b"early".to_vec()), None);

        conns.open(LinkRole::Peripheral, INBOUND, 244, false);
        // Still buffered: a link in the table is not a writable link.
        assert_eq!(conns.accept_inbound(INBOUND, b"later".to_vec()), None);

        let established = conns.establish(LinkRole::Peripheral, INBOUND, 244);
        assert_eq!(
            established.flushed,
            vec![b"early".to_vec(), b"later".to_vec()],
            "order must be preserved"
        );

        // From here it flows straight through.
        assert_eq!(
            conns.accept_inbound(INBOUND, b"live".to_vec()),
            Some((established.id, b"live".to_vec()))
        );
    }

    #[test]
    fn data_on_a_visible_but_unwritable_link_is_still_held_back() {
        // The caller holds this id, but has not been told the link is up. A
        // Data event before Connected would name a link it has no state for.
        let mut conns = table();
        conns.open_outbound(PEER, DEFAULT_MAX_WRITE);
        assert_eq!(conns.accept_inbound(PEER, b"early".to_vec()), None);
        assert_eq!(
            conns.establish(LinkRole::Central, PEER, 244).flushed,
            vec![b"early".to_vec()]
        );
    }

    #[test]
    fn the_pre_connected_buffer_is_bounded() {
        // A peer that writes forever without subscribing must not be able to
        // grow this without limit.
        let mut conns = table();
        for n in 0..(PENDING_PACKETS_PER_LINK + 5) {
            conns.accept_inbound(INBOUND, vec![n as u8]);
        }
        let flushed = conns.establish(LinkRole::Peripheral, INBOUND, 244).flushed;
        assert_eq!(flushed.len(), PENDING_PACKETS_PER_LINK);
        // The oldest were dropped, so the newest survive.
        assert_eq!(
            flushed.last(),
            Some(&vec![(PENDING_PACKETS_PER_LINK + 4) as u8])
        );
    }

    #[test]
    fn buffered_data_is_discarded_when_the_connection_goes_away() {
        let mut conns = table();
        conns.accept_inbound(INBOUND, b"orphan".to_vec());
        conns.close(INBOUND);
        assert!(
            conns
                .establish(LinkRole::Peripheral, INBOUND, 244)
                .flushed
                .is_empty()
        );
    }

    #[test]
    fn buffers_are_kept_per_connection() {
        let mut conns = table();
        conns.accept_inbound(INBOUND, b"inbound".to_vec());
        conns.accept_inbound(PEER, b"outbound".to_vec());
        conns.accept_inbound(OTHER, b"other peer".to_vec());
        assert_eq!(
            conns.establish(LinkRole::Peripheral, INBOUND, 244).flushed,
            vec![b"inbound".to_vec()]
        );
    }

    #[test]
    fn max_write_is_per_link_and_never_a_constant() {
        // Two links negotiated at different times routinely disagree.
        let mut conns = table();
        let small = conns.establish(LinkRole::Central, PEER, 20).id;
        let large = conns.establish(LinkRole::Central, OTHER, 512).id;
        assert_eq!(conns.max_write(&small), Some(20));
        assert_eq!(conns.max_write(&large), Some(512));

        conns.set_max_write(&small, 244);
        assert_eq!(conns.max_write(&small), Some(244));
    }

    #[test]
    fn max_write_is_none_for_a_link_that_is_gone() {
        let mut conns = table();
        let id = conns.establish(LinkRole::Central, PEER, 244).id;
        conns.remove(&id);
        assert_eq!(conns.max_write(&id), None);
        assert_eq!(conns.handle(&id), None);
        assert!(conns.target(&id).is_none());
        // Recording a size for a link that no longer exists must not revive it.
        conns.set_max_write(&id, 512);
        assert_eq!(conns.max_write(&id), None);
    }

    #[test]
    fn removing_by_id_also_forgets_what_was_buffered_for_it() {
        let mut conns = table();
        let id = conns.open(LinkRole::Peripheral, INBOUND, 244, true).id;
        conns.accept_inbound(INBOUND, b"orphan".to_vec());
        conns.remove(&id);
        assert!(
            conns
                .establish(LinkRole::Peripheral, INBOUND, 244)
                .flushed
                .is_empty()
        );
    }

    #[test]
    fn draining_returns_every_visible_link_exactly_once() {
        let mut conns = table();
        let a = conns.establish(LinkRole::Central, PEER, 244).id;
        let b = conns.establish(LinkRole::Peripheral, INBOUND, 244).id;
        conns.open(LinkRole::Central, OTHER, 244, false);

        let mut drained = conns.drain();
        drained.sort();
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(drained, expected);
        assert_eq!(conns.len(), 0);
        assert!(conns.drain().is_empty());
    }

    // -- event marshalling -------------------------------------------------

    #[test]
    fn establishing_a_link_announces_it_with_the_negotiated_size() {
        let shared = shared(8);
        shared.establish(LinkRole::Peripheral, INBOUND, 185);
        let events = drain_events(&shared);
        let link = match &events[..] {
            [
                LinkEvent::Connected {
                    link,
                    role,
                    max_write,
                },
            ] => {
                assert_eq!(*role, LinkRole::Peripheral);
                assert_eq!(*max_write, 185);
                link.clone()
            }
            other => panic!("expected one Connected, got {other:?}"),
        };

        shared.close_link(INBOUND);
        assert_eq!(
            drain_events(&shared),
            vec![LinkEvent::Disconnected { link }]
        );
    }

    #[test]
    fn data_never_arrives_before_the_link_it_belongs_to() {
        // The ordering the Connected contract exists to guarantee.
        let shared = shared(8);
        shared.inbound(INBOUND, b"early".to_vec());
        assert!(
            drain_events(&shared).is_empty(),
            "nothing may be emitted for an unknown link"
        );

        shared.establish(LinkRole::Peripheral, INBOUND, 185);
        let events = drain_events(&shared);
        match &events[..] {
            [
                LinkEvent::Connected { link, .. },
                LinkEvent::Data { link: on, bytes },
            ] => {
                assert_eq!(link, on);
                assert_eq!(bytes, b"early");
            }
            other => panic!("expected Connected then Data, got {other:?}"),
        }

        shared.inbound(INBOUND, b"live".to_vec());
        assert!(matches!(
            drain_events(&shared)[..],
            [LinkEvent::Data { .. }]
        ));
    }

    #[test]
    fn an_outbound_attempt_emits_nothing_until_it_is_writable() {
        let shared = shared(8);
        let id = shared.open_central(PEER);
        assert!(
            drain_events(&shared).is_empty(),
            "connect() must not announce a link it cannot write to"
        );

        shared.establish(LinkRole::Central, PEER, 244);
        assert_eq!(
            drain_events(&shared),
            vec![LinkEvent::Connected {
                link: id,
                role: LinkRole::Central,
                max_write: 244
            }]
        );
    }

    #[test]
    fn losing_the_adapter_disconnects_every_link() {
        let shared = shared(16);
        shared.establish(LinkRole::Central, PEER, 244);
        shared.establish(LinkRole::Peripheral, INBOUND, 244);
        drain_events(&shared);

        shared.emit(LinkEvent::AdapterPowerChanged { powered: false });
        shared.drop_all();

        let events = drain_events(&shared);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], LinkEvent::AdapterPowerChanged { powered: false });
        assert!(
            events[1..]
                .iter()
                .all(|event| matches!(event, LinkEvent::Disconnected { .. }))
        );
        assert_eq!(shared.conns().len(), 0);
    }

    // -- the bounded queue -------------------------------------------------

    #[test]
    fn queue_pressure_keeps_every_lifecycle_transition_in_order() {
        let shared = shared(2);
        let link = LinkId::new("central:test#1");
        let connected = LinkEvent::Connected {
            link: link.clone(),
            role: LinkRole::Central,
            max_write: 244,
        };
        let disconnected = LinkEvent::Disconnected { link: link.clone() };
        shared.emit(connected.clone());
        for _ in 0..8 {
            shared.emit(LinkEvent::Data {
                link: link.clone(),
                bytes: vec![1],
            });
        }
        shared.emit(disconnected.clone());
        shared.emit(LinkEvent::Data {
            link: link.clone(),
            bytes: vec![2],
        });
        shared.emit(connected.clone()); // Lifecycle-only overflow is retained too.
        assert_eq!(
            drain_events(&shared),
            vec![connected.clone(), disconnected, connected]
        );
    }

    #[test]
    fn a_full_queue_drops_rather_than_blocking_the_binder_thread() {
        // Blocking here would stall the entire Android Bluetooth stack, and
        // buffering without limit off a radio anyone in range can talk to is an
        // out-of-memory kill waiting to happen.
        let shared = shared(2);
        for rssi in [-40, -50, -60, -70] {
            shared.emit(LinkEvent::Discovered {
                address: PEER.to_string(),
                rssi,
                peer: None,
            });
        }

        let events = drain_events(&shared);
        assert_eq!(events.len(), 2, "the queue must not grow past its bound");
        // Oldest non-lifecycle events are evicted.
        assert_eq!(
            events[0],
            LinkEvent::Discovered {
                address: PEER.to_string(),
                rssi: -60,
                peer: None
            }
        );
        assert_eq!(
            events[1],
            LinkEvent::Discovered {
                address: PEER.to_string(),
                rssi: -70,
                peer: None
            }
        );
    }

    #[test]
    fn events_emitted_after_shutdown_are_dropped_not_queued() {
        let shared = shared(4);
        shared.close_queue();
        assert!(shared.stopped());
        shared.emit(LinkEvent::AdapterPowerChanged { powered: true });
        assert!(drain_events(&shared).is_empty());
    }

    #[test]
    fn a_log_line_for_a_data_event_never_carries_the_payload() {
        // Mesh traffic is other people's ciphertext and logcat is not private.
        let event = LinkEvent::Data {
            link: LinkId::new("central:x#1"),
            bytes: b"secret".to_vec(),
        };
        let text = describe(&event);
        assert!(text.contains("6 bytes"), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn every_event_describes_itself_by_name() {
        let link = LinkId::new("central:x#1");
        for event in [
            LinkEvent::Discovered {
                address: PEER.to_string(),
                rssi: -70,
                peer: None,
            },
            LinkEvent::Connected {
                link: link.clone(),
                role: LinkRole::Central,
                max_write: 244,
            },
            LinkEvent::Disconnected { link: link.clone() },
            LinkEvent::Data {
                link,
                bytes: vec![0; 3],
            },
            LinkEvent::AdapterPowerChanged { powered: true },
        ] {
            assert!(!describe(&event).is_empty());
        }
    }

    // -- the MeshLink surface ---------------------------------------------

    #[test]
    fn events_come_back_in_order() {
        let link = AndroidLink::new();
        link.inner.emit(LinkEvent::Discovered {
            address: PEER.to_string(),
            rssi: -70,
            peer: None,
        });
        link.inner
            .emit(LinkEvent::AdapterPowerChanged { powered: true });

        block_on(async {
            assert_eq!(
                link.next_event().await,
                Some(LinkEvent::Discovered {
                    address: PEER.to_string(),
                    rssi: -70,
                    peer: None
                })
            );
            assert_eq!(
                link.next_event().await,
                Some(LinkEvent::AdapterPowerChanged { powered: true })
            );
        });
    }

    #[test]
    fn stop_drains_the_queue_and_then_ends_it() {
        let link = AndroidLink::new();
        link.inner
            .emit(LinkEvent::AdapterPowerChanged { powered: false });

        block_on(async {
            link.stop().await.expect("stop");
            assert_eq!(
                link.next_event().await,
                Some(LinkEvent::AdapterPowerChanged { powered: false })
            );
            assert_eq!(link.next_event().await, None);
            assert_eq!(link.next_event().await, None, "the end is permanent");
        });
    }

    #[test]
    fn start_after_stop_is_refused_rather_than_silently_dead() {
        let link = AndroidLink::new();
        block_on(async {
            link.stop().await.expect("stop");
            assert!(matches!(link.start().await, Err(LinkError::Backend(_))));
            assert!(matches!(
                link.connect(PEER).await,
                Err(LinkError::Backend(_))
            ));
            assert!(matches!(
                link.set_scanning(true).await,
                Err(LinkError::Backend(_))
            ));
            // The duty cycle calls this unconditionally, including after stop.
            assert!(link.set_scanning(false).await.is_ok());
        });
    }

    #[test]
    fn operations_on_an_unknown_link_say_the_link_is_gone() {
        let link = AndroidLink::new();
        let unknown = LinkId::new("central:AA:BB:CC:DD:EE:FF#99");
        block_on(async {
            assert_eq!(
                link.send(&unknown, b"hello").await,
                Err(error::link_gone(&unknown))
            );
            assert_eq!(
                link.disconnect(&unknown).await,
                Err(error::link_gone(&unknown))
            );
        });
        assert_eq!(link.max_write(&unknown), None);
    }

    #[test]
    fn an_oversized_payload_is_rejected_before_it_reaches_the_controller() {
        // Oversized writes are dropped silently by the controller, so this has
        // to be caught here or the packet vanishes with no error anywhere.
        let link = AndroidLink::new();
        link.inner.establish(LinkRole::Peripheral, INBOUND, 20);
        let id = link
            .inner
            .conns()
            .find(INBOUND)
            .expect("the link was just established");

        block_on(async {
            assert_eq!(
                link.send(&id, &[0u8; 21]).await,
                Err(LinkError::PayloadTooLarge {
                    actual: 21,
                    max: 20
                })
            );
            // Exactly at the limit gets past the size check and on to the
            // bridge, which is not installed in a test, so it fails there.
            assert!(!matches!(
                link.send(&id, &[0u8; 20]).await,
                Err(LinkError::PayloadTooLarge { .. })
            ));
        });
    }

    #[test]
    fn max_write_falls_back_to_the_cached_size_when_kotlin_cannot_be_asked() {
        // No bridge in a unit test, so the table is the only source — and it
        // must not report None for a link that exists.
        let link = AndroidLink::new();
        link.inner.establish(LinkRole::Central, PEER, 244);
        let id = link
            .inner
            .conns()
            .find(PEER)
            .expect("the link was just established");
        assert_eq!(link.max_write(&id), Some(244));
    }

    #[test]
    fn disconnect_announces_the_link_and_forgets_it() {
        let link = AndroidLink::new();
        link.inner.establish(LinkRole::Central, PEER, 244);
        let id = link
            .inner
            .conns()
            .find(PEER)
            .expect("the link was just established");
        drain_events(&link.inner);

        block_on(async {
            // The bridge call fails with no JVM; the local teardown still has
            // to happen, which is what this asserts.
            let _ = link.disconnect(&id).await;
        });
        assert_eq!(
            drain_events(&link.inner),
            vec![LinkEvent::Disconnected { link: id.clone() }]
        );
        assert_eq!(link.max_write(&id), None);
        assert_eq!(link.link_count(), 0);
    }

    #[test]
    fn the_link_is_usable_from_several_tasks_at_once() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AndroidLink>();
        assert_send_sync::<Shared>();
    }
}

fn lifecycle(event: &LinkEvent) -> bool {
    matches!(
        event,
        LinkEvent::Connected { .. } | LinkEvent::Disconnected { .. }
    )
}
