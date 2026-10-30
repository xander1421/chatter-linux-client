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

mod android;
mod mtu;
mod registration;
#[cfg(test)]
mod tests;

pub use android::AndroidLink;
pub use mtu::{
    max_write_for_mtu, sanitise_max_write, ATT_WRITE_OVERHEAD, DEFAULT_MAX_WRITE,
    LARGEST_MAX_WRITE, MAX_ATT_MTU, MIN_ATT_MTU,
};
pub(crate) use registration::{dispatch, on_connected, on_data, on_disconnected};

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
fn lifecycle(event: &LinkEvent) -> bool {
    matches!(
        event,
        LinkEvent::Connected { .. } | LinkEvent::Disconnected { .. }
    )
}
