//! The central role: we scan, and we connect outward.
//!
//! Three jobs, in the order they happen: filter and report what is in range,
//! bring a chosen peer up to a writable link, and then keep that link's inbound
//! notifications flowing until it dies.
//!
//! # Scanning is a lever, not a mode
//!
//! Discovery is the expensive half of BLE — it keeps the receiver on — so it runs
//! intermittently under the policy layer's duty cycle while the peripheral role
//! advertises continuously. [`start_scan`] and dropping its handle are that
//! lever; nothing here decides when to pull it.
//!
//! # No gating happens here
//!
//! Every peer advertising the mesh service is reported with whatever RSSI BlueZ
//! last saw, and the scheduler above decides what is worth one of the six scarce
//! central slots. The discovery filter's own RSSI threshold is therefore set to
//! the bottom of the range: it exists to make BlueZ emit RSSI updates at all, not
//! to filter. A radio that quietly dropped distant peers would defeat the
//! deliberate relaxation the policy layer applies when a device has no peers at
//! all and needs to reach further to join the mesh.
//!
//! # A connect is not finished when the ACL is up
//!
//! [`LinkEvent::Connected`] promises a link that can carry a write immediately,
//! so [`connect`] does not return until the service and characteristic have been
//! discovered, the write socket acquired, and notifications subscribed. Anything
//! less produces a link the mesh will happily plan writes onto and the stack will
//! silently drop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use bluer::gatt::remote::Characteristic as RemoteCharacteristic;
use bluer::gatt::{CharacteristicReader, CharacteristicWriter};
use bluer::{
    Adapter, AdapterEvent, Address, Device, DeviceEvent, DeviceProperty, DiscoveryFilter,
    DiscoveryTransport, Uuid,
};
use chatter_mesh_link::{LinkError, LinkEvent, LinkId, LinkRole};
use futures::stream::BoxStream;
use futures::{StreamExt, stream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::error;
use crate::link::Shared;

/// Ceiling on a whole connect attempt: ACL, service discovery, and both socket
/// acquisitions.
///
/// Generous, because BlueZ's own service resolution is slow on first contact with
/// an unbonded device, but bounded, because a connect that never returns holds
/// one of the six central slots forever and starves the scheduler.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How far a connect got before the clock ran out.
///
/// The stages fail for unrelated reasons and want unrelated responses: an ACL
/// that never forms is a peer out of range, a service resolution that hangs is a
/// GATT database problem, and a notify socket that never arrives is a
/// subscription the far side is still waiting on — which is what a peer logging
/// "connected (awaiting subscription)" and nothing further is telling you.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum ConnectStage {
    /// Opening the ACL.
    Acl,
    /// Resolving the GATT database and finding our characteristic.
    ServiceDiscovery,
    /// Acquiring the write socket, which is what settles the MTU.
    WriteSocket,
    /// Acquiring the notify socket — the subscription the peer waits for.
    NotifySocket,
}

impl ConnectStage {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::ServiceDiscovery,
            2 => Self::WriteSocket,
            3 => Self::NotifySocket,
            _ => Self::Acl,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Acl => "opening the connection",
            Self::ServiceDiscovery => "finding the mesh service",
            Self::WriteSocket => "acquiring the write socket",
            Self::NotifySocket => "subscribing for notifications",
        }
    }
}

/// RSSI threshold handed to BlueZ, in dBm.
///
/// Effectively "no threshold": it is below the noise floor of any real
/// controller. Setting it at all is what makes BlueZ emit `RSSI` property
/// changes for devices it already knows about, which is where [`start_scan`]
/// gets the number it reports.
const RSSI_FLOOR: i16 = -127;

// Must sit below any real controller's noise floor, so the filter never gates on
// signal strength. Checked at compile time: a runtime assert over a constant is
// folded away and would never observe a bad edit.
const _: () = assert!(RSSI_FLOOR <= -127);

/// The discovery filter for the mesh.
///
/// Deliberately carries **no UUID list**, even though filtering by our service
/// is exactly what it looks like this should do.
///
/// Setting `uuids` makes BlueZ stop populating the `RSSI` property. Not for
/// non-matching devices — for every device, including the matching one. Measured
/// on hci0 with a phone advertising the mesh service: with no UUID list the
/// devices report −48, −72, −74, −76, −78; with the list set, the same devices
/// in the same room report `None`. [`start_scan`] treats a missing RSSI as
/// "BlueZ knows this device but is not currently seeing it" and skips it, so
/// with the filter installed **every mesh peer was dropped, always** — the
/// symptom being a mesh that advertises correctly, scans correctly, and never
/// discovers anybody.
///
/// Nothing is lost by removing it: [`start_scan`] already re-checks the
/// advertised UUIDs itself, and has to, because BlueZ merges discovery filters
/// across every client on the bus — a second program scanning unfiltered widens
/// ours regardless of what we ask for. The filter was never load-bearing; it was
/// only ever an optimisation, and it cost the whole feature.
fn discovery_filter(_service: Uuid) -> DiscoveryFilter {
    DiscoveryFilter {
        // LE only. `Auto` would also run BR/EDR inquiry, which is slower, louder,
        // and cannot find anything we care about.
        transport: DiscoveryTransport::Le,
        rssi: Some(RSSI_FLOOR),
        // Without this BlueZ coalesces repeated advertisements from a device it
        // has already reported, and the RSSI we hand the scheduler goes stale.
        duplicate_data: true,
        ..Default::default()
    }
}

/// Install the discovery filter.
///
/// Called once at startup rather than per scan: BlueZ refuses to change the
/// filter while a discovery session is running, and the duty cycle starts and
/// stops discovery constantly.
pub(crate) async fn configure_discovery(adapter: &Adapter, service: Uuid) -> Result<(), LinkError> {
    adapter.set_discovery_filter(discovery_filter(service)).await.map_err(|err| error::classify_adapter(&err))
}

/// A running scan. Dropping it stops discovery.
pub struct Scan {
    task: JoinHandle<()>,
}

impl Drop for Scan {
    fn drop(&mut self) {
        // Aborting drops the discovery stream, which drops `bluer`'s session
        // token, which calls StopDiscovery. There is no separate stop to make.
        self.task.abort();
    }
}

impl core::fmt::Debug for Scan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Scan")
    }
}

/// Begin scanning, reporting every mesh peer in range as
/// [`LinkEvent::Discovered`].
///
/// Returns only once BlueZ has actually accepted `StartDiscovery`, so a failure
/// to scan is reported to the caller rather than disappearing into a detached
/// task. The adapter is taken by value because the discovery stream borrows it
/// for its whole life and the two have to live in the same place.
pub(crate) async fn start_scan(
    adapter: Adapter, service: Uuid, shared: Arc<Shared>,
) -> Result<Scan, LinkError> {
    let (ready_tx, ready_rx) = oneshot::channel();

    let task = tokio::spawn(async move {
        let discovery = match adapter.discover_devices_with_changes().await {
            Ok(discovery) => {
                if ready_tx.send(Ok(())).is_err() {
                    // Nobody is waiting any more: the caller went away between
                    // asking to scan and us starting. Stop rather than scan for
                    // an owner that no longer exists.
                    return;
                }
                discovery
            }
            Err(err) => {
                let _ = ready_tx.send(Err(error::classify_adapter(&err)));
                return;
            }
        };
        // `Box::pin` rather than `futures::pin_mut!`, which expands to an
        // `unsafe` block this crate forbids.
        let mut discovery = Box::pin(discovery);

        while let Some(event) = discovery.next().await {
            let AdapterEvent::DeviceAdded(address) = event else {
                continue;
            };
            let Ok(device) = adapter.device(address) else {
                continue;
            };

            // BlueZ merges discovery filters across every client on the bus, so
            // another program scanning without a UUID filter will widen ours.
            // Check for the service ourselves rather than trusting the filter.
            let advertised = device.uuids().await.ok().flatten().unwrap_or_default();
            if !advertised.contains(&service) {
                continue;
            }

            // `None` means BlueZ knows this device but is not currently seeing
            // it — a stale object from a previous session, replayed to us when
            // discovery started. Reporting it would send the scheduler chasing a
            // peer that is not there.
            let Ok(Some(rssi)) = device.rssi().await else {
                continue;
            };

            // The advertised peer id, when the scan response arrived with it.
            // `None` is "not known yet", not "not a peer": BlueZ fills service
            // data in from whichever packets it has seen, and the scan response
            // often lands a beat after the advertisement. The policy layer treats
            // the unknown case as dialable, so a peer whose id we never learn
            // behaves exactly as it did before this existed.
            let peer = device
                .service_data()
                .await
                .ok()
                .flatten()
                .and_then(|data| data.get(&service).cloned())
                .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_slice()).ok());

            shared.emit(LinkEvent::Discovered { address: address.to_string(), rssi, peer });
        }
    });

    match ready_rx.await {
        Ok(Ok(())) => Ok(Scan { task }),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(LinkError::Backend("discovery task ended before it started".to_string())),
    }
}

/// Connect outward and bring the link all the way up to writable.
pub(crate) async fn connect(
    adapter: &Adapter, shared: &Arc<Shared>, address: &str, service: Uuid, characteristic: Uuid,
) -> Result<LinkId, LinkError> {
    let peer: Address = address
        .parse()
        .map_err(|_| error::connect_failed(address, "not a Bluetooth address"))?;

    // A second link to the same peer in the same direction would be a second
    // ACL, which BLE does not have: BlueZ would hand back the same connection
    // and the two links would fight over one socket.
    if let Some(existing) = shared.find_link(LinkRole::Central, address) {
        return Ok(existing);
    }

    let device = adapter.device(peer).map_err(|err| error::classify_connect(address, &err))?;

    // Which step is outstanding, for the timeout branch to report. A single
    // bound covers four of them and they fail for unrelated reasons; naming the
    // one that hung is the difference between a diagnosis and a shrug.
    let stage = Arc::new(AtomicU8::new(ConnectStage::Acl as u8));
    // Whether this attempt is what opened the ACL, and so whether it is ours to
    // close on the way out. See `establish`.
    let opened_here = Arc::new(AtomicBool::new(false));
    let outcome = timeout(
        CONNECT_TIMEOUT,
        establish(&device, service, characteristic, Arc::clone(&stage), Arc::clone(&opened_here)),
    )
    .await;
    let (writer, reader) = match outcome {
        Ok(Ok(pair)) => pair,
        // A failure *after* the ACL opened leaves it open. Nothing owns it: no
        // link was published, so no session drops it and nothing ever writes to
        // it. The peer meanwhile holds a connected central that never
        // subscribes — which is exactly what a device logging
        // `connected (awaiting subscription)` and nothing further is looking at.
        // Only the timeout branch used to clean up, and a socket acquisition can
        // fail outright well inside twenty seconds.
        Ok(Err(error)) => {
            if opened_here.load(Ordering::Relaxed) {
                let _ = device.disconnect().await;
            }
            return Err(error);
        }
        Err(_) => {
            // Leave nothing half-open behind: a dangling ACL keeps the peer's
            // connection slot occupied as well as ours — but only if this attempt
            // is what opened it. See `establish`.
            if opened_here.load(Ordering::Relaxed) {
                let _ = device.disconnect().await;
            }
            // Reported as a timeout rather than a plain failure so the scheduler
            // applies its timeout window. The distinction matters most here:
            // `disconnect()` on a connect that is still in flight is what makes
            // BlueZ abort it, and the abort surfaces on the *next* attempt as
            // `le-connection-abort-by-local` — so a fast retry turns one slow
            // peer into a run of failures.
            return Err(LinkError::ConnectTimedOut {
                address: address.to_string(),
                stage: ConnectStage::from_u8(stage.load(Ordering::Relaxed))
                    .as_str()
                    .to_string(),
            });
        }
    };

    // `bluer` has already subtracted BlueZ's per-operation headroom from the
    // negotiated ATT MTU, so this is the real payload ceiling for this link and
    // this link only. It is read from the socket rather than assumed because
    // fragment sizing above depends on it and an oversized packet is dropped by
    // the controller without an error.
    let max_write = writer.mtu();

    let link =
        shared.open_link(LinkRole::Central, adapter.name(), address, max_write, Some(Arc::new(writer)));

    tokio::spawn(session(shared.clone(), address.to_string(), device, reader));

    Ok(link)
}

/// Everything between "we have a device object" and "we can write to it".
async fn establish(
    device: &Device, service: Uuid, characteristic: Uuid, stage: Arc<AtomicU8>,
    opened_here: Arc<AtomicBool>,
) -> Result<(CharacteristicWriter, CharacteristicReader), LinkError> {
    let address = device.address().to_string();

    // Each step records itself before starting, so whichever one is outstanding
    // when the caller's clock expires is the one it names.
    stage.store(ConnectStage::Acl as u8, Ordering::Relaxed);
    let connected = device.is_connected().await.map_err(|err| error::classify_connect(&address, &err))?;
    if !connected {
        // Only now do we own this ACL, and only an ACL we opened may we close.
        // A device already connected is one the *peer* dialled: BLE has a single
        // ACL per pair of controllers, so it is the same connection carrying our
        // inbound peripheral link, and hanging it up on a failed outbound attempt
        // would drop a link that is working. Nothing upstream prevents this
        // pairing — `find_link` checks only the central role, inbound links never
        // enter `address_of`, and an advert whose scan response has not arrived
        // yet carries no peer id for the policy layer to recognise.
        opened_here.store(true, Ordering::Relaxed);
        device.connect().await.map_err(|err| error::classify_connect(&address, &err))?;
    }

    stage.store(ConnectStage::ServiceDiscovery as u8, Ordering::Relaxed);
    let characteristic = find_characteristic(device, service, characteristic).await?;

    // Write first, then notify. Either order works, but acquiring the write
    // socket is what forces BlueZ to finish the MTU exchange, so the MTU the
    // notify socket reports afterwards is the settled one.
    stage.store(ConnectStage::WriteSocket as u8, Ordering::Relaxed);
    let writer = characteristic.write_io().await.map_err(|err| error::classify_connect(&address, &err))?;

    stage.store(ConnectStage::NotifySocket as u8, Ordering::Relaxed);
    let reader = characteristic.notify_io().await.map_err(|err| error::classify_connect(&address, &err))?;

    Ok((writer, reader))
}

/// Walk the peer's GATT database for the mesh characteristic.
async fn find_characteristic(
    device: &Device, service: Uuid, characteristic: Uuid,
) -> Result<RemoteCharacteristic, LinkError> {
    let address = device.address().to_string();

    // Waits for BlueZ to finish resolving services, which is the slow part of a
    // first connection to an unbonded peer.
    let services = device.services().await.map_err(|err| error::classify_connect(&address, &err))?;

    for candidate in services {
        if !matches!(candidate.uuid().await, Ok(uuid) if uuid == service) {
            continue;
        }
        let characteristics =
            candidate.characteristics().await.map_err(|err| error::classify_connect(&address, &err))?;
        for found in characteristics {
            if matches!(found.uuid().await, Ok(uuid) if uuid == characteristic) {
                return Ok(found);
            }
        }
    }

    // The peer advertised the mesh service but does not serve it. Not our link to
    // keep, and not a fault of the adapter.
    Err(error::connect_failed(&address, "peer does not expose the mesh characteristic"))
}

/// Pump one central link's inbound notifications until the link dies.
///
/// Watches two things at once because either can be the first to notice: the
/// notification socket closing, and BlueZ reporting the device disconnected. On
/// a clean teardown they arrive together; when the peer drops out of range, which
/// of them lands first is not predictable.
async fn session(shared: Arc<Shared>, address: String, device: Device, reader: CharacteristicReader) {
    let mut events: BoxStream<'_, DeviceEvent> = match device.events().await {
        Ok(events) => events.boxed(),
        // We can still serve the link from its socket alone; we just lose the
        // second way of noticing it died.
        Err(_) => stream::pending().boxed(),
    };

    loop {
        tokio::select! {
            received = reader.recv() => match received {
                // Zero-length datagram: BlueZ closed its end of the socket.
                Ok(bytes) if bytes.is_empty() => break,
                Ok(bytes) => shared.inbound(LinkRole::Central, &address, bytes),
                Err(_) => break,
            },
            event = events.next() => match event {
                Some(DeviceEvent::PropertyChanged(DeviceProperty::Connected(false))) => break,
                Some(_) => (),
                // The device object was removed from D-Bus entirely.
                None => break,
            },
        }
    }

    drop(events);
    shared.forget_pending(LinkRole::Central, &address);
    shared.close_link(LinkRole::Central, &address);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> Uuid {
        crate::link::service_uuid().expect("service uuid")
    }

    #[test]
    fn the_filter_asks_bluez_for_le_only_and_never_names_a_uuid() {
        // The UUID list is the regression this pins. Setting it makes BlueZ stop
        // reporting RSSI for *every* device, and `start_scan` drops a peer with
        // no RSSI — so a filter that looks like an optimisation silently made
        // the mesh unable to discover anyone at all. `start_scan` matches the
        // service itself, which it must anyway: BlueZ merges filters across bus
        // clients, so ours is never the only one in effect.
        let filter = discovery_filter(service());
        assert_eq!(filter.transport, DiscoveryTransport::Le);
        assert!(
            filter.uuids.is_empty(),
            "a UUID list here suppresses RSSI and costs us every peer"
        );
    }

    #[test]
    fn the_filter_does_no_rssi_gating_of_its_own() {
        // Gating is the scheduler's job, and it deliberately relaxes its
        // threshold for a device with no peers so that it can still reach far
        // enough to join. A filter that dropped distant peers here would make
        // that relaxation do nothing.
        let filter = discovery_filter(service());
        assert_eq!(filter.rssi, Some(RSSI_FLOOR));
        assert_eq!(filter.pathloss, None, "BlueZ rejects a filter that sets both rssi and pathloss");
    }

    #[test]
    fn the_filter_keeps_advertisement_updates_flowing() {
        // Without duplicate data the RSSI handed to the scheduler goes stale the
        // moment a device is first reported.
        assert!(discovery_filter(service()).duplicate_data);
    }

    #[test]
    fn the_filter_does_not_make_us_discoverable_as_a_side_effect() {
        // Discoverability belongs to the advertisement, which is built to carry
        // no local name. Letting the discovery filter turn it on would publish
        // the adapter's own alias instead.
        assert!(!discovery_filter(service()).discoverable);
        assert_eq!(discovery_filter(service()).pattern, None);
    }

    #[test]
    fn a_malformed_address_is_a_connect_failure_not_a_panic() {
        let parsed: Result<Address, _> = "not-an-address".parse();
        assert!(parsed.is_err());
        let err = error::connect_failed("not-an-address", "not a Bluetooth address");
        assert!(matches!(err, LinkError::ConnectFailed { .. }));
    }

    #[test]
    fn peer_addresses_round_trip_through_the_form_discovery_reports() {
        // `Discovered` hands out `Address::to_string`, and `connect` parses it
        // straight back. If these two ever disagreed, every connect would fail
        // with a malformed-address error for peers we had just found.
        let address = Address::new([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        let text = address.to_string();
        assert_eq!(text, "AA:BB:CC:DD:EE:FF");
        assert_eq!(text.parse::<Address>().expect("round trip"), address);
    }

    #[test]
    fn the_connect_timeout_is_bounded_but_allows_for_slow_service_resolution() {
        assert!(CONNECT_TIMEOUT >= Duration::from_secs(10));
        assert!(CONNECT_TIMEOUT <= Duration::from_secs(60));
    }
}
