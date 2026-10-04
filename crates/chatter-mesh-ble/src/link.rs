//! [`BlueZLink`] — the [`MeshLink`] implementation, and the state both roles share.
//!
//! This module owns two things the rest of the crate borrows:
//!
//! - the **connection table**, mapping [`LinkId`] to the socket we write on, and
//! - the **event queue** that [`MeshLink::next_event`] drains.
//!
//! [`central`](crate::central) and [`peripheral`](crate::peripheral) do the
//! Bluetooth work and report into [`Shared`]; neither of them decides what a
//! link id is or when an event is emitted.
//!
//! # A link exists only once it can carry a write
//!
//! [`LinkEvent::Connected`] promises the link is writable *now*, so a link only
//! enters this table when its outbound socket exists:
//!
//! - **Central**: after `Connect`, service discovery, `AcquireWrite` and
//!   `AcquireNotify` have all succeeded. A BlueZ ACL by itself carries nothing.
//! - **Peripheral**: when the remote central subscribes and BlueZ hands us the
//!   notification socket. Before that there is literally nobody to notify — an
//!   inbound ATT write channel is not a link, because it only goes one way.
//!
//! BlueZ delivers the peripheral role's two channels as independent
//! `AcquireWrite` and `AcquireNotify` requests, in whichever order the peer makes
//! them, and a peer may write before it subscribes. Those early bytes are held in
//! a small per-peer buffer and replayed immediately after `Connected`, so no
//! `Data` event ever names a link the layer above has not been told about, and a
//! packet that arrived a millisecond early is not simply lost — nothing below us
//! retransmits.
//!
//! # Link identity
//!
//! A [`LinkId`] here is `{role}:{device path}#{sequence}`, for example
//! `central:/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF#3`. Three parts, three reasons:
//!
//! - **role**, because two devices running the mesh will often connect to each
//!   other in both directions at once. Those are two links with two independent
//!   send paths, and they must not collide on one id.
//! - **device path**, because an opaque integer is unreadable in a log next to a
//!   `busctl` trace.
//! - **sequence**, because ids must never be reused. Without it a link that drops
//!   and immediately re-establishes would produce `Connected(X)` for an id the
//!   layer above still holds state for, and a late `Disconnected(X)` from the old
//!   incarnation would then destroy the new one's state.
//!
//! None of this makes a link id a peer identity. A link is bound to a peer only
//! after that peer proves who it is with a signed announce, several layers above
//! here; conflating the two is what lets a replayed announce steal a link.
//!
//! # Lifetime of this object
//!
//! [`MeshLink::stop`] is terminal, as its documentation implies: after it,
//! `next_event` drains whatever is left and then resolves to `None` forever.
//! Restarting the radio means constructing a new `BlueZLink`. The duty cycle does
//! *not* go through `stop` — that is [`MeshLink::set_scanning`], which leaves the
//! peripheral role and every established link untouched.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bluer::gatt::CharacteristicWriter;
use bluer::{Adapter, Uuid};
use chatter_mesh_link::{
    BoxFuture, CHARACTERISTIC_UUID, LinkError, LinkEvent, LinkId, LinkRole, MeshLink, SERVICE_UUID,
};
use tokio::sync::{Mutex as AsyncMutex, mpsc, watch};
use tokio::task::JoinHandle;

use crate::adapter::{self, BleAdapter};
use crate::central::{self, Scan};
use crate::error;
use crate::peripheral::PeripheralRole;

/// How many inbound packets to hold for a peer that has opened an ATT write
/// channel but has not subscribed yet.
///
/// The window is normally sub-millisecond — a central subscribes immediately
/// after acquiring its write channel — so this is a safety margin, not a queue.
/// It is bounded because a peer that writes and never subscribes would otherwise
/// grow it without limit; past the bound the oldest packet is dropped, since in a
/// mesh the oldest is the one most likely to be stale already.
const PENDING_PACKETS_PER_PEER: usize = 16;

/// The mainnet service UUID, parsed.
///
/// Fails only if the frozen constant in `chatter-mesh-link` stops being a UUID,
/// which a unit test in this module rules out. It is still returned as an error
/// rather than unwrapped, because a radio that cannot advertise should report
/// that rather than take the process down.
pub(crate) fn service_uuid() -> Result<Uuid, LinkError> {
    Uuid::parse_str(SERVICE_UUID)
        .map_err(|err| LinkError::Backend(format!("service UUID {SERVICE_UUID} is not a UUID: {err}")))
}

/// The mesh characteristic UUID, parsed. See [`service_uuid`].
pub(crate) fn characteristic_uuid() -> Result<Uuid, LinkError> {
    Uuid::parse_str(CHARACTERISTIC_UUID).map_err(|err| {
        LinkError::Backend(format!("characteristic UUID {CHARACTERISTIC_UUID} is not a UUID: {err}"))
    })
}

/// The D-Bus object path BlueZ uses for a device, e.g.
/// `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF`.
///
/// Reconstructed rather than read back from `bluer`, which keeps its path
/// builders private. The format is part of BlueZ's stable D-Bus API.
fn device_path(adapter_name: &str, address: &str) -> String {
    format!("/org/bluez/{}/dev_{}", adapter_name, address.replace(':', "_"))
}

/// Short, stable text for a role, used in link ids.
fn role_tag(role: LinkRole) -> &'static str {
    match role {
        LinkRole::Central => "central",
        LinkRole::Peripheral => "peripheral",
    }
}

/// One live, writable connection.
struct Conn {
    role: LinkRole,
    /// Peer address as `AA:BB:CC:DD:EE:FF`, the same text `Discovered` carries.
    address: String,
    /// Largest single write BlueZ will accept on this link.
    ///
    /// Taken verbatim from the socket BlueZ handed us — see [`Shared::open_link`].
    max_write: usize,
    /// Where writes go.
    ///
    /// `Option` only so that the table can be exercised in unit tests, which
    /// cannot manufacture a `CharacteristicWriter` without a radio. In a running
    /// link layer every entry has one; a `None` here means the link is not
    /// writable, which by this module's contract means it is not a link.
    writer: Option<Arc<CharacteristicWriter>>,
    /// Whether `Connected` has been emitted for this link yet.
    ///
    /// There is a brief moment between inserting a link and announcing it. Data
    /// arriving in that window must not be emitted, or the layer above would see
    /// `Data` for a link it has never heard of.
    announced: bool,
}

/// The `LinkId` → connection map, plus the pre-subscription inbound buffer.
///
/// Deliberately free of any Bluetooth calls: everything here is a pure state
/// transition, which is what makes the lifecycle testable without a radio.
#[derive(Default)]
struct Connections {
    by_id: HashMap<LinkId, Conn>,
    /// Inbound packets held until the link they belong to is announced. See
    /// [`PENDING_PACKETS_PER_PEER`].
    ///
    /// Keyed by role as well as address, because a peer we are linked to in both
    /// directions has two independent inbound channels; replaying one on the
    /// other's link would attribute a packet to a connection it never arrived on.
    pending: HashMap<(&'static str, String), VecDeque<Vec<u8>>>,
    next_seq: u64,
}

impl Connections {
    /// Insert a link, unannounced.
    ///
    /// Returns the new id and, if one already existed for this `(role, address)`
    /// and had been announced, the id it displaced — BlueZ re-acquiring a channel
    /// without closing the old one means the old socket is dead.
    fn open(
        &mut self, role: LinkRole, adapter_name: &str, address: &str, max_write: usize,
        writer: Option<Arc<CharacteristicWriter>>,
    ) -> (LinkId, Option<LinkId>) {
        let mut displaced = None;
        if let Some(old) = self.find(role, address) {
            if let Some(conn) = self.by_id.remove(&old) {
                if conn.announced {
                    displaced = Some(old);
                }
            }
        }

        self.next_seq += 1;
        let id =
            LinkId::new(format!("{}:{}#{}", role_tag(role), device_path(adapter_name, address), self.next_seq));
        self.by_id.insert(
            id.clone(),
            Conn { role, address: address.to_string(), max_write, writer, announced: false },
        );
        (id, displaced)
    }

    /// Mark a link announced and take everything that arrived from that peer
    /// before it was.
    fn announce(&mut self, id: &LinkId) -> Vec<Vec<u8>> {
        let Some(conn) = self.by_id.get_mut(id) else {
            return Vec::new();
        };
        conn.announced = true;
        let key = (role_tag(conn.role), conn.address.clone());
        match self.pending.remove(&key) {
            Some(queue) => queue.into_iter().collect(),
            None => Vec::new(),
        }
    }

    /// The link currently representing `(role, address)`, if any.
    fn find(&self, role: LinkRole, address: &str) -> Option<LinkId> {
        self.by_id
            .iter()
            .find(|(_, conn)| conn.role == role && conn.address == address)
            .map(|(id, _)| id.clone())
    }

    /// Route one inbound payload.
    ///
    /// Returns the link to emit it on, or buffers it when the link is not
    /// announced yet.
    fn accept_inbound(&mut self, role: LinkRole, address: &str, bytes: Vec<u8>) -> Option<(LinkId, Vec<u8>)> {
        if let Some(id) = self.find(role, address) {
            if self.by_id.get(&id).is_some_and(|conn| conn.announced) {
                return Some((id, bytes));
            }
        }

        let queue = self.pending.entry((role_tag(role), address.to_string())).or_default();
        if queue.len() >= PENDING_PACKETS_PER_PEER {
            queue.pop_front();
        }
        queue.push_back(bytes);
        None
    }

    /// Discard anything held for a channel we are no longer expecting to hear on.
    fn forget_pending(&mut self, role: LinkRole, address: &str) {
        self.pending.remove(&(role_tag(role), address.to_string()));
    }

    /// Remove the link for `(role, address)`. Returns its id only if it had been
    /// announced, since a link the layer above never learned about must not
    /// produce a `Disconnected`.
    fn close(&mut self, role: LinkRole, address: &str) -> Option<LinkId> {
        let id = self.find(role, address)?;
        let conn = self.by_id.remove(&id)?;
        conn.announced.then_some(id)
    }

    /// Remove by id. Returns the entry so the caller can act on its address.
    fn remove(&mut self, id: &LinkId) -> Option<Conn> {
        self.by_id.remove(id)
    }

    /// Remove every link, returning the announced ones.
    fn drain(&mut self) -> Vec<LinkId> {
        self.pending.clear();
        self.by_id.drain().filter(|(_, conn)| conn.announced).map(|(id, _)| id).collect()
    }

    fn max_write(&self, id: &LinkId) -> Option<usize> {
        self.by_id.get(id).map(|conn| conn.max_write)
    }

    /// Everything `send` needs, taken under one lock.
    fn target(&self, id: &LinkId) -> Option<(Option<Arc<CharacteristicWriter>>, usize)> {
        self.by_id.get(id).map(|conn| (conn.writer.clone(), conn.max_write))
    }

    fn len(&self) -> usize {
        self.by_id.len()
    }
}

/// The receiving half of the event queue, plus the signal that ends it.
struct EventQueue {
    rx: mpsc::UnboundedReceiver<LinkEvent>,
    shutdown: watch::Receiver<bool>,
}

/// The Bluetooth handles that only exist between `start` and `stop`.
#[derive(Default)]
struct Roles {
    adapter: Option<BleAdapter>,
    peripheral: Option<PeripheralRole>,
    scan: Option<Scan>,
    power: Option<JoinHandle<()>>,
}

/// State shared between the link object and every background task it spawns.
///
/// Note what is *not* here: the [`Roles`] handles. Every role task holds an
/// `Arc<Shared>`, so keeping their `JoinHandle`s inside `Shared` would close a
/// reference cycle — the task keeps `Shared` alive, `Shared` keeps the task
/// alive — and dropping a `BlueZLink` without calling `stop` would leak the
/// whole link layer, still advertising. They live in [`BlueZLink`] instead, and
/// dropping it aborts them.
pub(crate) struct Shared {
    /// The live adapter, kept here rather than beside the role handles so
    /// `connect` and `send` can take a clone without waiting behind a
    /// `start`/`stop`/scan toggle.
    adapter: Mutex<Option<Adapter>>,
    conns: Mutex<Connections>,
    events_tx: mpsc::UnboundedSender<LinkEvent>,
    queue: AsyncMutex<EventQueue>,
    shutdown_tx: watch::Sender<bool>,
}

impl Shared {
    fn conns(&self) -> MutexGuard<'_, Connections> {
        // A panic in a background task must not turn every later lock into a
        // panic of its own; the map's invariants do not depend on unwinding.
        self.conns.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue an event for `next_event`.
    ///
    /// The queue is unbounded on purpose: this is called from the D-Bus and
    /// socket tasks, and applying backpressure there would stall `bluetoothd`
    /// itself rather than the consumer. Bounding it belongs in the policy layer,
    /// which already buffers.
    pub(crate) fn emit(&self, event: LinkEvent) {
        let _ = self.events_tx.send(event);
    }

    /// Register a writable link and announce it.
    ///
    /// `max_write` must be the size BlueZ reported for the socket in `writer`,
    /// not a constant and not a guess: `bluer` has already subtracted BlueZ's
    /// per-operation headroom from the negotiated ATT MTU by the time it hands
    /// over a `CharacteristicWriter`, so `writer.mtu()` *is* the largest payload
    /// this link accepts. Subtracting anything further would silently shrink
    /// every fragment for the life of the link; adding anything would produce
    /// packets the controller drops without an error.
    pub(crate) fn open_link(
        &self, role: LinkRole, adapter_name: &str, address: &str, max_write: usize,
        writer: Option<Arc<CharacteristicWriter>>,
    ) -> LinkId {
        let (id, displaced) = { self.conns().open(role, adapter_name, address, max_write, writer) };
        if let Some(old) = displaced {
            self.emit(LinkEvent::Disconnected { link: old });
        }

        self.emit(LinkEvent::Connected { link: id.clone(), role, max_write });

        // Only now can inbound data be released: announcing sets the flag that
        // stops `accept_inbound` buffering, and drains what it already buffered.
        // Doing it in this order is what guarantees `Data` never precedes
        // `Connected` for a link.
        let buffered = { self.conns().announce(&id) };
        for bytes in buffered {
            self.emit(LinkEvent::Data { link: id.clone(), bytes });
        }
        id
    }

    /// Route one inbound payload, holding it back if the link is not announced.
    pub(crate) fn inbound(&self, role: LinkRole, address: &str, bytes: Vec<u8>) {
        let routed = { self.conns().accept_inbound(role, address, bytes) };
        if let Some((link, bytes)) = routed {
            self.emit(LinkEvent::Data { link, bytes });
        }
    }

    /// The peer's outbound channel closed: the link is over.
    pub(crate) fn close_link(&self, role: LinkRole, address: &str) {
        let gone = { self.conns().close(role, address) };
        if let Some(id) = gone {
            self.emit(LinkEvent::Disconnected { link: id });
        }
    }

    /// Stop holding early packets for a channel that has closed.
    pub(crate) fn forget_pending(&self, role: LinkRole, address: &str) {
        self.conns().forget_pending(role, address);
    }

    pub(crate) fn find_link(&self, role: LinkRole, address: &str) -> Option<LinkId> {
        self.conns().find(role, address)
    }

    /// Drop a link by id and announce it.
    fn drop_link(&self, id: &LinkId) {
        let removed = { self.conns().remove(id) };
        if removed.is_some_and(|conn| conn.announced) {
            self.emit(LinkEvent::Disconnected { link: id.clone() });
        }
    }

    /// Drop every link and announce each one. Used when the adapter dies under us
    /// and on shutdown.
    fn drop_all(&self) {
        let ids = { self.conns().drain() };
        for id in ids {
            self.emit(LinkEvent::Disconnected { link: id });
        }
    }

    fn adapter_handle(&self) -> Option<Adapter> {
        self.adapter.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

/// The BlueZ radio.
///
/// Runs the central and peripheral roles together, as the mesh requires: a
/// device that only scans is invisible to peers scanning for it, and removes a
/// bridge the rest of the mesh may be depending on.
pub struct BlueZLink {
    inner: Arc<Shared>,
    /// Adapter to use, or `None` for the default one.
    adapter_name: Option<String>,
    /// Handles for the running roles.
    ///
    /// Owned here rather than in [`Shared`] so that dropping this object breaks
    /// the reference cycle it would otherwise form with the tasks — see the note
    /// on `Shared`. Dropping these aborts the tasks, unregisters the
    /// advertisement and the GATT application, and stops discovery.
    roles: AsyncMutex<Roles>,
}

impl BlueZLink {
    /// Use the default adapter (`hci0` when present).
    ///
    /// No hardware is touched until [`MeshLink::start`].
    pub fn new() -> Self {
        Self::for_adapter(None)
    }

    fn for_adapter(adapter_name: Option<String>) -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            inner: Arc::new(Shared {
                adapter: Mutex::new(None),
                conns: Mutex::new(Connections::default()),
                events_tx,
                queue: AsyncMutex::new(EventQueue { rx: events_rx, shutdown: shutdown_rx }),
                shutdown_tx,
            }),
            adapter_name,
            roles: AsyncMutex::new(Roles::default()),
        }
    }

    /// Number of links currently established. Diagnostics only.
    pub fn link_count(&self) -> usize {
        self.inner.conns().len()
    }
}

impl Default for BlueZLink {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for BlueZLink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BlueZLink")
            .field("adapter", &self.adapter_name)
            .field("links", &self.link_count())
            .finish()
    }
}

impl MeshLink for BlueZLink {
    fn start(&self) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async move {
            let stopped = { *self.inner.shutdown_tx.borrow() };
            if stopped {
                return Err(LinkError::Backend(
                    "this link layer has been stopped; construct a new BlueZLink to restart".to_string(),
                ));
            }

            let service = service_uuid()?;
            let characteristic = characteristic_uuid()?;

            let mut roles = self.roles.lock().await;
            if roles.adapter.is_some() {
                return Ok(());
            }

            let ble = match &self.adapter_name {
                Some(name) => BleAdapter::acquire_named(name).await?,
                None => BleAdapter::acquire().await?,
            };
            ble.power_on().await?;

            // Set the discovery filter once, up front: BlueZ refuses to change it
            // while a discovery session is running, and the duty cycle starts and
            // stops discovery constantly.
            central::configure_discovery(ble.adapter(), service).await?;

            let peripheral =
                PeripheralRole::start(ble.adapter(), service, characteristic, self.inner.clone()).await?;

            let watcher = self.inner.clone();
            let power = tokio::spawn(adapter::watch_power(ble.adapter().clone(), move |powered| {
                watcher.emit(LinkEvent::AdapterPowerChanged { powered });
                if !powered {
                    // Every ACL died with the controller. Say so once per link
                    // rather than leaving the layer above to discover it one
                    // failed write at a time.
                    watcher.drop_all();
                }
            }));

            {
                let mut handle = self.inner.adapter.lock().unwrap_or_else(PoisonError::into_inner);
                *handle = Some(ble.adapter().clone());
            }
            roles.adapter = Some(ble);
            roles.peripheral = Some(peripheral);
            roles.power = Some(power);
            Ok(())
        })
    }

    fn stop(&self) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async move {
            {
                let mut roles = self.roles.lock().await;
                // Dropping these stops discovery, unregisters the advertisement
                // and the GATT application, and aborts their tasks.
                roles.scan = None;
                roles.peripheral = None;
                if let Some(power) = roles.power.take() {
                    power.abort();
                }
                roles.adapter = None;
            }
            {
                let mut handle = self.inner.adapter.lock().unwrap_or_else(PoisonError::into_inner);
                *handle = None;
            }

            self.inner.drop_all();
            // Ends `next_event` once the queue has drained.
            let _ = self.inner.shutdown_tx.send(true);
            Ok(())
        })
    }

    fn set_scanning(&self, on: bool) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async move {
            let service = service_uuid()?;
            let mut roles = self.roles.lock().await;

            if !on {
                // Dropping the handle aborts the scan task, which drops the
                // discovery session, which calls StopDiscovery.
                roles.scan = None;
                return Ok(());
            }
            if roles.scan.is_some() {
                return Ok(());
            }

            let adapter = roles
                .adapter
                .as_ref()
                .ok_or_else(|| LinkError::AdapterUnavailable("link layer has not been started".to_string()))?
                .adapter()
                .clone();
            roles.scan = Some(central::start_scan(adapter, service, self.inner.clone()).await?);
            Ok(())
        })
    }

    fn connect(&self, address: &str) -> BoxFuture<'_, Result<LinkId, LinkError>> {
        let address = address.to_string();
        Box::pin(async move {
            let service = service_uuid()?;
            let characteristic = characteristic_uuid()?;
            let adapter = self
                .inner
                .adapter_handle()
                .ok_or_else(|| LinkError::AdapterUnavailable("link layer has not been started".to_string()))?;
            central::connect(&adapter, &self.inner, &address, service, characteristic).await
        })
    }

    fn disconnect(&self, link: &LinkId) -> BoxFuture<'_, Result<(), LinkError>> {
        let shared = self.inner.clone();
        let link = link.clone();
        Box::pin(async move {
            let removed = { shared.conns().remove(&link) };
            let Some(conn) = removed else {
                return Err(LinkError::LinkGone(link.to_string()));
            };
            if conn.announced {
                shared.emit(LinkEvent::Disconnected { link });
            }

            // Tearing down the ACL is the only lever BlueZ offers, and it is
            // per-device rather than per-role: if we also hold a link to this
            // peer in the other direction it dies too. That is a property of BLE
            // — one ACL per pair of devices — not a bug here. The surviving
            // link's watcher task notices and reports its own `Disconnected`.
            if let Some(adapter) = shared.adapter_handle() {
                if let Ok(address) = conn.address.parse::<bluer::Address>() {
                    if let Ok(device) = adapter.device(address) {
                        let _ = device.disconnect().await;
                    }
                }
            }
            Ok(())
        })
    }

    fn send(&self, link: &LinkId, bytes: &[u8]) -> BoxFuture<'_, Result<(), LinkError>> {
        let shared = self.inner.clone();
        let link = link.clone();
        let bytes = bytes.to_vec();
        Box::pin(async move {
            let target = { shared.conns().target(&link) };
            let Some((writer, max_write)) = target else {
                return Err(LinkError::LinkGone(link.to_string()));
            };

            if bytes.len() > max_write {
                return Err(LinkError::PayloadTooLarge { actual: bytes.len(), max: max_write });
            }

            // Unreachable in a running link layer — every entry in the table has
            // a socket, which is the whole point of only admitting writable links
            // — but a link with no send path is by definition not usable.
            let Some(writer) = writer else {
                return Err(LinkError::LinkGone(link.to_string()));
            };

            // Deliberately `try_send` and not `send`: waiting for room would hide
            // congestion from the layer whose job it is to queue and retry.
            match writer.try_send(&bytes) {
                Ok(()) => Ok(()),
                Err(err) => {
                    let mapped = error::classify_write(&link, &err);
                    if error::is_terminal(&mapped) {
                        shared.drop_link(&link);
                    }
                    Err(mapped)
                }
            }
        })
    }

    fn max_write(&self, link: &LinkId) -> Option<usize> {
        self.inner.conns().max_write(link)
    }

    fn next_event(&self) -> BoxFuture<'_, Option<LinkEvent>> {
        Box::pin(async move {
            let mut queue = self.inner.queue.lock().await;
            loop {
                // `borrow_and_update` marks the current value seen, so the
                // `changed()` below waits for the *next* transition. Because the
                // receiver is long-lived rather than freshly cloned, a shutdown
                // racing between these two lines still wakes the select.
                let stopped = { *queue.shutdown.borrow_and_update() };
                if stopped {
                    // Hand back whatever is still queued, then end for good.
                    return queue.rx.try_recv().ok();
                }

                let EventQueue { rx, shutdown } = &mut *queue;
                tokio::select! {
                    event = rx.recv() => return event,
                    changed = shutdown.changed() => {
                        if changed.is_err() {
                            return rx.try_recv().ok();
                        }
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADAPTER: &str = "hci0";
    const PEER: &str = "AA:BB:CC:DD:EE:FF";
    const OTHER: &str = "11:22:33:44:55:66";

    fn table() -> Connections {
        Connections::default()
    }

    #[test]
    fn the_frozen_uuids_parse() {
        // Everything else in this module returns an error rather than unwrapping
        // these; this is the test that makes that path unreachable in practice.
        let service = service_uuid().expect("service uuid");
        let characteristic = characteristic_uuid().expect("characteristic uuid");
        assert_eq!(service.to_string().to_uppercase(), SERVICE_UUID);
        assert_eq!(characteristic.to_string().to_uppercase(), CHARACTERISTIC_UUID);
        // Mainnet, not bitchat's debug UUID, which differs only in the last char.
        assert!(!service.to_string().to_uppercase().ends_with("4B5A"));
    }

    #[test]
    fn device_paths_follow_bluez() {
        assert_eq!(device_path("hci0", PEER), "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
        assert_eq!(device_path("hci1", OTHER), "/org/bluez/hci1/dev_11_22_33_44_55_66");
    }

    #[test]
    fn link_ids_name_the_role_the_device_and_a_sequence() {
        let mut conns = table();
        let (id, displaced) = conns.open(LinkRole::Central, ADAPTER, PEER, 244, None);
        assert!(displaced.is_none());
        assert_eq!(id.as_str(), "central:/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF#1");
    }

    #[test]
    fn the_same_peer_in_both_directions_gets_two_distinct_links() {
        // Two devices running the mesh routinely connect to each other at the
        // same time. Those are two links with two send paths.
        let mut conns = table();
        let (central, _) = conns.open(LinkRole::Central, ADAPTER, PEER, 244, None);
        let (peripheral, displaced) = conns.open(LinkRole::Peripheral, ADAPTER, PEER, 244, None);
        assert!(displaced.is_none(), "the peripheral link must not displace the central one");
        assert_ne!(central, peripheral);
        assert_eq!(conns.len(), 2);
    }

    #[test]
    fn a_reestablished_link_never_reuses_an_id() {
        // Reuse would let a late Disconnected from the old incarnation destroy
        // state belonging to the new one.
        let mut conns = table();
        let (first, _) = conns.open(LinkRole::Central, ADAPTER, PEER, 244, None);
        conns.announce(&first);
        assert_eq!(conns.close(LinkRole::Central, PEER).as_ref(), Some(&first));
        let (second, _) = conns.open(LinkRole::Central, ADAPTER, PEER, 244, None);
        assert_ne!(first, second);
    }

    #[test]
    fn reacquiring_a_channel_displaces_the_stale_link() {
        let mut conns = table();
        let (first, _) = conns.open(LinkRole::Peripheral, ADAPTER, PEER, 244, None);
        conns.announce(&first);
        let (second, displaced) = conns.open(LinkRole::Peripheral, ADAPTER, PEER, 185, None);
        assert_eq!(displaced, Some(first));
        assert_eq!(conns.len(), 1);
        assert_eq!(conns.max_write(&second), Some(185));
    }

    #[test]
    fn an_unannounced_link_never_produces_a_disconnected() {
        // The layer above was never told this link existed; telling it the link
        // is gone would name an id it has no state for.
        let mut conns = table();
        conns.open(LinkRole::Peripheral, ADAPTER, PEER, 244, None);
        assert_eq!(conns.close(LinkRole::Peripheral, PEER), None);
        assert_eq!(conns.len(), 0);
        assert!(conns.drain().is_empty());
    }

    #[test]
    fn closing_a_link_that_is_not_there_is_harmless() {
        let mut conns = table();
        assert_eq!(conns.close(LinkRole::Central, PEER), None);
    }

    #[test]
    fn inbound_data_before_the_link_is_announced_is_held_not_dropped() {
        // A central may write on its ATT channel before it subscribes. Nothing
        // below us retransmits, so those bytes have to survive the gap.
        let mut conns = table();
        assert_eq!(conns.accept_inbound(LinkRole::Peripheral, PEER, b"early".to_vec()), None);

        let (id, _) = conns.open(LinkRole::Peripheral, ADAPTER, PEER, 244, None);
        // Still buffered: inserting a link is not announcing it.
        assert_eq!(conns.accept_inbound(LinkRole::Peripheral, PEER, b"later".to_vec()), None);

        let flushed = conns.announce(&id);
        assert_eq!(flushed, vec![b"early".to_vec(), b"later".to_vec()], "order must be preserved");

        // From here it flows straight through.
        assert_eq!(
            conns.accept_inbound(LinkRole::Peripheral, PEER, b"live".to_vec()),
            Some((id, b"live".to_vec()))
        );
    }

    #[test]
    fn the_pre_announcement_buffer_is_bounded() {
        // A peer that writes forever without subscribing must not be able to grow
        // this without limit.
        let mut conns = table();
        for n in 0..(PENDING_PACKETS_PER_PEER + 5) {
            conns.accept_inbound(LinkRole::Peripheral, PEER, vec![n as u8]);
        }
        let (id, _) = conns.open(LinkRole::Peripheral, ADAPTER, PEER, 244, None);
        let flushed = conns.announce(&id);
        assert_eq!(flushed.len(), PENDING_PACKETS_PER_PEER);
        // The oldest were dropped, so the newest survive.
        assert_eq!(flushed.last(), Some(&vec![(PENDING_PACKETS_PER_PEER + 4) as u8]));
    }

    #[test]
    fn buffered_data_is_discarded_when_the_peer_goes_away() {
        let mut conns = table();
        conns.accept_inbound(LinkRole::Peripheral, PEER, b"orphan".to_vec());
        conns.forget_pending(LinkRole::Peripheral, PEER);
        let (id, _) = conns.open(LinkRole::Peripheral, ADAPTER, PEER, 244, None);
        assert!(conns.announce(&id).is_empty());
    }

    #[test]
    fn buffers_are_kept_per_peer_and_per_direction() {
        // A peer we are linked to in both directions has two inbound channels.
        // Replaying one on the other's link would attribute a packet to a
        // connection it never arrived on.
        let mut conns = table();
        conns.accept_inbound(LinkRole::Peripheral, PEER, b"inbound".to_vec());
        conns.accept_inbound(LinkRole::Central, PEER, b"other direction".to_vec());
        conns.accept_inbound(LinkRole::Peripheral, OTHER, b"other peer".to_vec());

        let (id, _) = conns.open(LinkRole::Peripheral, ADAPTER, PEER, 244, None);
        assert_eq!(conns.announce(&id), vec![b"inbound".to_vec()]);
    }

    #[test]
    fn max_write_is_whatever_the_socket_reported_and_differs_per_link() {
        // The one thing that must never be a constant here. Two links negotiated
        // at different times routinely disagree.
        let mut conns = table();
        let (small, _) = conns.open(LinkRole::Central, ADAPTER, PEER, 20, None);
        let (large, _) = conns.open(LinkRole::Central, ADAPTER, OTHER, 512, None);
        assert_eq!(conns.max_write(&small), Some(20));
        assert_eq!(conns.max_write(&large), Some(512));
    }

    #[test]
    fn max_write_is_none_for_a_link_that_is_gone() {
        let mut conns = table();
        let (id, _) = conns.open(LinkRole::Central, ADAPTER, PEER, 244, None);
        conns.remove(&id);
        assert_eq!(conns.max_write(&id), None);
        assert!(conns.target(&id).is_none());
    }

    #[test]
    fn find_distinguishes_peers_and_roles() {
        let mut conns = table();
        conns.open(LinkRole::Central, ADAPTER, PEER, 244, None);
        assert!(conns.find(LinkRole::Central, PEER).is_some());
        assert!(conns.find(LinkRole::Peripheral, PEER).is_none());
        assert!(conns.find(LinkRole::Central, OTHER).is_none());
    }

    #[test]
    fn draining_returns_every_announced_link_exactly_once() {
        let mut conns = table();
        let (a, _) = conns.open(LinkRole::Central, ADAPTER, PEER, 244, None);
        let (b, _) = conns.open(LinkRole::Peripheral, ADAPTER, OTHER, 244, None);
        conns.announce(&a);
        conns.announce(&b);
        let mut drained = conns.drain();
        drained.sort();
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(drained, expected);
        assert_eq!(conns.len(), 0);
        assert!(conns.drain().is_empty());
    }

    #[tokio::test]
    async fn events_come_back_in_order() {
        let link = BlueZLink::new();
        link.inner.emit(LinkEvent::Discovered { address: PEER.to_string(), rssi: -70, peer: None });
        link.inner.emit(LinkEvent::AdapterPowerChanged { powered: true });

        assert_eq!(
            link.next_event().await,
            Some(LinkEvent::Discovered { address: PEER.to_string(), rssi: -70, peer: None })
        );
        assert_eq!(link.next_event().await, Some(LinkEvent::AdapterPowerChanged { powered: true }));
    }

    #[tokio::test]
    async fn opening_a_link_announces_it_with_the_negotiated_size() {
        let link = BlueZLink::new();
        let id = link.inner.open_link(LinkRole::Peripheral, ADAPTER, PEER, 185, None);
        assert_eq!(
            link.next_event().await,
            Some(LinkEvent::Connected { link: id.clone(), role: LinkRole::Peripheral, max_write: 185 })
        );
        assert_eq!(link.max_write(&id), Some(185));

        link.inner.close_link(LinkRole::Peripheral, PEER);
        assert_eq!(link.next_event().await, Some(LinkEvent::Disconnected { link: id }));
    }

    #[tokio::test]
    async fn data_never_arrives_before_the_link_it_belongs_to() {
        // The ordering the Connected contract exists to guarantee.
        let link = BlueZLink::new();
        link.inner.inbound(LinkRole::Peripheral, PEER, b"early".to_vec());

        let id = link.inner.open_link(LinkRole::Peripheral, ADAPTER, PEER, 185, None);
        assert_eq!(
            link.next_event().await,
            Some(LinkEvent::Connected { link: id.clone(), role: LinkRole::Peripheral, max_write: 185 })
        );
        assert_eq!(
            link.next_event().await,
            Some(LinkEvent::Data { link: id.clone(), bytes: b"early".to_vec() })
        );

        link.inner.inbound(LinkRole::Peripheral, PEER, b"live".to_vec());
        assert_eq!(link.next_event().await, Some(LinkEvent::Data { link: id, bytes: b"live".to_vec() }));
    }

    #[tokio::test]
    async fn losing_the_adapter_disconnects_every_link() {
        let link = BlueZLink::new();
        let a = link.inner.open_link(LinkRole::Central, ADAPTER, PEER, 244, None);
        let b = link.inner.open_link(LinkRole::Peripheral, ADAPTER, OTHER, 244, None);

        // Drain the two Connected events.
        link.next_event().await;
        link.next_event().await;

        link.inner.drop_all();
        let mut gone = Vec::new();
        for _ in 0..2 {
            match link.next_event().await {
                Some(LinkEvent::Disconnected { link }) => gone.push(link),
                other => panic!("expected Disconnected, got {other:?}"),
            }
        }
        gone.sort();
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(gone, expected);
        assert_eq!(link.link_count(), 0);
    }

    #[tokio::test]
    async fn stop_drains_the_queue_and_then_ends_it() {
        let link = BlueZLink::new();
        link.inner.emit(LinkEvent::AdapterPowerChanged { powered: false });
        link.stop().await.expect("stop");

        assert_eq!(link.next_event().await, Some(LinkEvent::AdapterPowerChanged { powered: false }));
        assert_eq!(link.next_event().await, None);
        assert_eq!(link.next_event().await, None, "the end is permanent");
    }

    #[tokio::test]
    async fn a_parked_next_event_wakes_when_the_layer_shuts_down() {
        // Without this, a policy loop blocked on next_event would hang forever
        // after stop() instead of unwinding.
        let link = Arc::new(BlueZLink::new());
        let waiter = link.clone();
        let parked = tokio::spawn(async move { waiter.next_event().await });

        tokio::task::yield_now().await;
        link.stop().await.expect("stop");

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), parked)
            .await
            .expect("next_event did not wake on shutdown")
            .expect("task panicked");
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn start_after_stop_is_refused_rather_than_silently_dead() {
        let link = BlueZLink::new();
        link.stop().await.expect("stop");
        // Fails before touching any hardware, so this is safe with no radio.
        assert!(matches!(link.start().await, Err(LinkError::Backend(_))));
    }

    #[tokio::test]
    async fn operations_before_start_report_the_missing_adapter() {
        let link = BlueZLink::new();
        assert!(matches!(link.connect(PEER).await, Err(LinkError::AdapterUnavailable(_))));
        assert!(matches!(link.set_scanning(true).await, Err(LinkError::AdapterUnavailable(_))));
        // Turning scanning off with nothing running is a no-op, not an error:
        // the duty cycle calls it unconditionally.
        assert!(link.set_scanning(false).await.is_ok());
    }

    #[tokio::test]
    async fn sending_on_an_unknown_link_says_the_link_is_gone() {
        let link = BlueZLink::new();
        let unknown = LinkId::new("central:/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF#99");
        assert_eq!(link.send(&unknown, b"hello").await, Err(LinkError::LinkGone(unknown.to_string())));
        assert_eq!(link.disconnect(&unknown).await, Err(LinkError::LinkGone(unknown.to_string())));
        assert_eq!(link.max_write(&unknown), None);
    }

    #[tokio::test]
    async fn an_oversized_payload_is_rejected_before_it_reaches_the_controller() {
        // Oversized writes are dropped silently by the controller, so this has to
        // be caught here or the packet vanishes with no error anywhere.
        let link = BlueZLink::new();
        let id = link.inner.open_link(LinkRole::Peripheral, ADAPTER, PEER, 20, None);
        assert_eq!(link.send(&id, &[0u8; 21]).await, Err(LinkError::PayloadTooLarge { actual: 21, max: 20 }));
        // Exactly at the limit gets past the size check.
        assert!(!matches!(link.send(&id, &[0u8; 20]).await, Err(LinkError::PayloadTooLarge { .. })));
    }

    #[tokio::test]
    async fn disconnect_announces_the_link_and_forgets_it() {
        let link = BlueZLink::new();
        let id = link.inner.open_link(LinkRole::Central, ADAPTER, PEER, 244, None);
        link.next_event().await; // Connected

        // With no adapter acquired this takes the "nothing to tear down" path,
        // so it needs no radio.
        link.disconnect(&id).await.expect("disconnect");
        assert_eq!(link.next_event().await, Some(LinkEvent::Disconnected { link: id.clone() }));
        assert_eq!(link.max_write(&id), None);
    }

    #[test]
    fn the_link_is_usable_from_several_tasks_at_once() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BlueZLink>();
        assert_send_sync::<Shared>();
    }
}
