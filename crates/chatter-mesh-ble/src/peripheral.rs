//! The peripheral role: we advertise, and other devices connect to us.
//!
//! Half the mesh's connectivity lives here. A device that only scans is
//! invisible to peers scanning for it — and worse than that, it removes a bridge
//! that two otherwise-disconnected halves of the mesh might only have had through
//! it. Both roles run at once, always.
//!
//! # What gets published
//!
//! One advertisement carrying the mesh service UUID and one pairing beacon, and one GATT
//! application exposing a single characteristic that carries all traffic in both
//! directions: write-without-response inbound, notify outbound.
//!
//! **No local name.** bitchat deliberately omits it and so do we. A local name is
//! broadcast in the clear to every passive scanner in radio range, and a stable
//! one is a persistent identifier for the device carrying it — exactly the
//! correlator the rest of this protocol goes to some length to avoid handing out.
//!
//! # The IO programming model, not the callback one
//!
//! `bluer` offers both. The callback model's `CharacteristicNotifier` does not
//! carry the subscribing device's address, because BlueZ's `StartNotify` does not
//! include one, so a mesh built on it could not tell which peer it was notifying —
//! it would broadcast every packet to every subscriber. The IO model's
//! `AcquireWrite`/`AcquireNotify` do carry the address and hand over a per-device
//! socket, which is the only shape a per-link mesh can use.
//!
//! # Two channels, one link
//!
//! BlueZ delivers the inbound write channel and the outbound notification channel
//! as separate acquisitions, in whichever order the peer makes them. Only the
//! notification channel makes a link: without it there is nobody to notify, and
//! [`chatter_mesh_link::LinkEvent::Connected`] promises a link that can carry a
//! write right now. So the notification session *is* the link's lifetime, and
//! inbound bytes that arrive before it are held by
//! [`Shared`](crate::link::Shared) and replayed once the link is announced.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bluer::adv::{Advertisement, AdvertisementHandle, SecondaryChannel, Type as AdvertisementType};
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicControl,
    CharacteristicControlEvent, CharacteristicControlHandle, CharacteristicNotify,
    CharacteristicNotifyMethod, CharacteristicWrite, CharacteristicWriteMethod, Service,
    characteristic_control,
};
use bluer::gatt::{CharacteristicReader, CharacteristicWriter};
use bluer::{Adapter, Address, Uuid};
use chatter_mesh_link::{
    InboundLink, LinkError, LinkEvent, LinkRole, PairingBeacon, admission_beacon,
};
use futures::StreamExt;
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
    time,
};

use crate::error;
use crate::link::Shared;

/// Build the advertisement for the mesh service.
///
/// Separated out and unit-tested because the thing most worth checking about it
/// is what it does *not* contain.
const BEACON_SLOT: Duration = Duration::from_secs(1);
const INBOUND_ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);

fn mesh_advertisement(service: Uuid, beacon: Option<PairingBeacon>) -> Advertisement {
    Advertisement {
        // Connectable. `Broadcast` would advertise without accepting inbound
        // connections, which is exactly the invisible-peripheral failure.
        advertisement_type: AdvertisementType::Peripheral,
        service_uuids: [service].into_iter().collect(),
        service_data: beacon
            .into_iter()
            .map(|beacon| (service, beacon.to_vec()))
            .collect(),
        // The service UUID, flags and 13-byte service data exceed a legacy
        // primary advertisement. BlueZ's ordinary ServiceData uses extended LE.
        secondary_channel: beacon.map(|_| SecondaryChannel::OneM),
        // Keep this instance non-discoverable even when the host is discoverable.
        // In BlueZ's limited privacy mode a discoverable, bondable advertiser
        // may use its identity address. It can still accept connections.
        discoverable: Some(false),
        local_name: None,
        ..Default::default()
    }
}

/// Build the GATT application: one primary service, one characteristic.
fn mesh_application(
    service: Uuid,
    characteristic: Uuid,
    control: CharacteristicControlHandle,
) -> Application {
    Application {
        services: vec![Service {
            uuid: service,
            primary: true,
            characteristics: vec![Characteristic {
                uuid: characteristic,
                write: Some(CharacteristicWrite {
                    // Both flags: `write-without-response` is what mesh traffic
                    // actually uses and what BlueZ requires before it will hand
                    // over an `AcquireWrite` socket, while `write` keeps the
                    // characteristic usable by a peer that only implements the
                    // acknowledged form.
                    write: true,
                    write_without_response: true,
                    method: CharacteristicWriteMethod::Io,
                    ..Default::default()
                }),
                notify: Some(CharacteristicNotify {
                    // Notify, not indicate: indications are acknowledged one at a
                    // time, which would serialise the entire outbound path behind
                    // a round trip per packet.
                    notify: true,
                    method: CharacteristicNotifyMethod::Io,
                    ..Default::default()
                }),
                control_handle: control,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// The published peripheral role.
///
/// Dropping this unregisters the advertisement and the GATT application and
/// stops the task that services them. Established links are not torn down here —
/// that is [`Shared`]'s business, and the sockets die on their own when BlueZ
/// closes them.
pub struct PeripheralRole {
    pump: JoinHandle<()>,
    advertisement: JoinHandle<()>,
    _application: ApplicationHandle,
}

impl PeripheralRole {
    /// Advertise the service and start serving the GATT application.
    pub(crate) async fn start(
        adapter: &Adapter,
        service: Uuid,
        characteristic: Uuid,
        shared: Arc<Shared>,
        beacons: watch::Receiver<Vec<PairingBeacon>>,
    ) -> Result<Self, LinkError> {
        let (control, control_handle) = characteristic_control();

        let application = adapter
            .serve_gatt_application(mesh_application(service, characteristic, control_handle))
            .await
            .map_err(|err| error::classify_peripheral(&err))?;

        // Advertise only after the service is actually servable, so a peer that
        // reacts instantly to the first advertising packet does not find an
        // empty GATT database.
        let first = beacons.borrow().first().copied();
        let initial = adapter
            .advertise(mesh_advertisement(service, first))
            .await
            .map_err(|err| error::classify_peripheral(&err))?;
        let advertisement = tokio::spawn(cycle_beacons(
            adapter.clone(),
            service,
            Arc::clone(&shared),
            beacons,
            first,
            initial,
        ));

        let pump = tokio::spawn(serve_control_events(control, adapter.clone(), shared));

        Ok(Self {
            pump,
            advertisement,
            _application: application,
        })
    }
}

async fn cycle_beacons(
    adapter: Adapter,
    service: Uuid,
    shared: Arc<Shared>,
    mut beacons: watch::Receiver<Vec<PairingBeacon>>,
    mut current: Option<PairingBeacon>,
    mut advertisement: AdvertisementHandle,
) {
    let mut slot = 0;
    loop {
        tokio::select! {
            result = beacons.changed() => {
                if result.is_err() {
                    return;
                }
            }
            _ = time::sleep(BEACON_SLOT) => {}
        }
        let next = {
            let slots = beacons.borrow_and_update();
            slot = (slot + 1) % slots.len().max(1);
            slots.get(slot).copied()
        };
        if next == current {
            continue;
        }
        drop(advertisement);
        // bluer unregisters asynchronously. Wait for capacity before registering
        // the next slot so a one-instance controller can finish unregistering.
        let free = matches!(
            time::timeout(Duration::from_millis(250), async {
                loop {
                    match adapter.supported_advertising_instances().await {
                        Ok(0) => time::sleep(Duration::from_millis(25)).await,
                        Ok(_) => return true,
                        Err(_) => return false,
                    }
                }
            })
            .await,
            Ok(true)
        );
        if !free {
            shared.emit(LinkEvent::PeripheralRoleEnded);
            return;
        }
        advertisement = match adapter.advertise(mesh_advertisement(service, next)).await {
            Ok(handle) => handle,
            Err(_) => {
                shared.emit(LinkEvent::PeripheralRoleEnded);
                return;
            }
        };
        current = next;
    }
}

impl Drop for PeripheralRole {
    fn drop(&mut self) {
        self.pump.abort();
        self.advertisement.abort();
    }
}

impl core::fmt::Debug for PeripheralRole {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PeripheralRole")
    }
}

/// Hand the two BlueZ ATT channels the same one-shot admission decision.
struct PendingAdmission {
    write: Option<oneshot::Sender<InboundLink>>,
    notify: Option<oneshot::Receiver<InboundLink>>,
    deadline: time::Instant,
}

impl PendingAdmission {
    fn new() -> Self {
        let (write, notify) = oneshot::channel();
        Self {
            write: Some(write),
            notify: Some(notify),
            deadline: time::Instant::now() + INBOUND_ADMISSION_TIMEOUT,
        }
    }
}

async fn serve_control_events(
    control: CharacteristicControl,
    adapter: Adapter,
    shared: Arc<Shared>,
) {
    let mut control = std::pin::pin!(control);
    let mut pending: HashMap<Address, PendingAdmission> = HashMap::new();
    while let Some(event) = control.next().await {
        pending.retain(|_, gate| gate.deadline > time::Instant::now());
        match event {
            CharacteristicControlEvent::Write(request) => {
                let address = request.device_address();
                let gate = pending.entry(address).or_insert_with(PendingAdmission::new);
                let Some(sender) = gate.write.take() else {
                    continue;
                };
                let deadline = gate.deadline;
                if gate.notify.is_none() {
                    pending.remove(&address);
                }
                if let Ok(reader) = request.accept() {
                    tokio::spawn(inbound(
                        adapter.clone(),
                        shared.clone(),
                        address,
                        reader,
                        sender,
                        deadline,
                    ));
                }
            }
            CharacteristicControlEvent::Notify(writer) => {
                let address = writer.device_address();
                let gate = pending.entry(address).or_insert_with(PendingAdmission::new);
                let Some(receiver) = gate.notify.take() else {
                    continue;
                };
                let deadline = gate.deadline;
                if gate.write.is_none() {
                    pending.remove(&address);
                }
                tokio::spawn(notify_after_admission(
                    adapter.clone(),
                    shared.clone(),
                    address,
                    writer,
                    receiver,
                    deadline,
                ));
            }
        }
    }
    shared.emit(LinkEvent::PeripheralRoleEnded);
}

/// The first ATT write is the only one read until core has admitted the peer.
async fn inbound(
    adapter: Adapter,
    shared: Arc<Shared>,
    remote: Address,
    reader: CharacteristicReader,
    gate: oneshot::Sender<InboundLink>,
    deadline: time::Instant,
) {
    let first = match time::timeout_at(deadline, reader.recv()).await {
        Ok(Ok(bytes)) if !bytes.is_empty() => bytes,
        _ => {
            disconnect_inbound(&adapter, remote).await;
            return;
        }
    };
    let address = remote.to_string();
    let beacon = admission_beacon(&first);
    let incoming = InboundLink {
        address: address.clone(),
        beacon,
    };
    if !shared.admit_inbound(&incoming) || gate.send(incoming).is_err() {
        disconnect_inbound(&adapter, remote).await;
        return;
    }
    if beacon.is_none() {
        shared.inbound(LinkRole::Peripheral, &address, first);
    }
    while let Ok(bytes) = reader.recv().await {
        if bytes.is_empty() {
            break;
        }
        shared.inbound(LinkRole::Peripheral, &address, bytes);
    }
    shared.forget_pending(LinkRole::Peripheral, &address);
}

async fn notify_after_admission(
    adapter: Adapter,
    shared: Arc<Shared>,
    remote: Address,
    writer: CharacteristicWriter,
    gate: oneshot::Receiver<InboundLink>,
    deadline: time::Instant,
) {
    let Ok(Ok(incoming)) = time::timeout_at(deadline, gate).await else {
        disconnect_inbound(&adapter, remote).await;
        return;
    };
    let address = remote.to_string();
    let max_write = writer.mtu();
    let writer = Arc::new(writer);
    shared.open_link(
        LinkRole::Peripheral,
        adapter.name(),
        &address,
        max_write,
        Some(writer.clone()),
        Some(incoming),
    );
    outbound(shared, address, writer).await;
}

async fn disconnect_inbound(adapter: &Adapter, remote: Address) {
    if let Ok(device) = adapter.device(remote) {
        let _ = device.disconnect().await;
    }
}

/// Hold a peer's notification session open, and end the link when it closes.
async fn outbound(shared: Arc<Shared>, address: String, writer: Arc<CharacteristicWriter>) {
    // Resolves when BlueZ closes its end: the peer unsubscribed, or the ACL
    // dropped. An error is the same signal — the socket is unusable either way.
    let _ = writer.closed().await;
    shared.close_link(LinkRole::Peripheral, &address);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatter_mesh_link::{CHARACTERISTIC_UUID, SERVICE_UUID};

    fn service() -> Uuid {
        crate::link::service_uuid().expect("service uuid")
    }

    #[test]
    fn the_advertisement_carries_no_device_identifier() {
        // Peers find the mesh service UUID without an identifier in its payload.
        let advertisement = mesh_advertisement(service(), None);
        assert_eq!(advertisement.local_name, None);
        assert!(
            advertisement.system_includes.is_empty(),
            "'local-name' must not sneak in via Includes"
        );
        assert!(advertisement.manufacturer_data.is_empty());
        assert!(advertisement.advertising_data.is_empty());
        assert_eq!(advertisement.appearance, None);
        assert!(advertisement.service_data.is_empty());
    }

    #[test]
    fn the_unconfigured_smoke_advertisement_fits_in_a_legacy_packet() {
        // Adding service data here is the tempting fix for address-rotation churn
        // and it is the wrong one on this side: it overflows the legacy budget,
        // and BlueZ answers an overflow by forcing extended advertising rather
        // than by spilling into the scan response. A legacy-only scanner then
        // sees nothing at all.
        let advertisement = mesh_advertisement(service(), None);
        let service_uuid_ad = 2 + 16;
        let used: usize = service_uuid_ad
            + advertisement
                .service_data
                .iter()
                .map(|(_, data)| 2 + 16 + data.len())
                .sum::<usize>();
        // Reserve three bytes for BlueZ's flags AD.
        assert!(
            used <= 31 - 3,
            "{used} bytes will not fit a legacy advertisement"
        );
    }

    #[test]
    fn the_extended_advertisement_carries_exactly_one_beacon() {
        let beacon = [7; 13];
        let advertisement = mesh_advertisement(service(), Some(beacon));
        assert_eq!(
            advertisement.secondary_channel,
            Some(SecondaryChannel::OneM)
        );
        assert_eq!(
            advertisement.service_data,
            [(service(), beacon.to_vec())].into()
        );
        assert_eq!(advertisement.discoverable, Some(false));
        assert!(advertisement.local_name.is_none());
    }

    #[test]
    fn the_advertisement_is_connectable_and_carries_the_mainnet_service() {
        let advertisement = mesh_advertisement(service(), None);
        assert_eq!(
            advertisement.advertisement_type,
            AdvertisementType::Peripheral
        );
        assert_eq!(advertisement.discoverable, Some(false));
        assert_eq!(advertisement.service_uuids.len(), 1);
        let advertised = advertisement
            .service_uuids
            .iter()
            .next()
            .expect("one uuid")
            .to_string();
        assert_eq!(advertised.to_uppercase(), SERVICE_UUID);
        // bitchat's debug UUID differs only in the final character; a build on it
        // is invisible to every shipping client while looking perfectly healthy.
        assert!(!advertised.to_uppercase().ends_with("4B5A"));
    }

    #[test]
    fn the_characteristic_supports_write_without_response_and_notify() {
        let (_control, handle) = characteristic_control();
        let characteristic_uuid = crate::link::characteristic_uuid().expect("characteristic uuid");
        let application = mesh_application(service(), characteristic_uuid, handle);

        assert_eq!(application.services.len(), 1);
        let gatt_service = &application.services[0];
        assert!(gatt_service.primary);
        assert_eq!(gatt_service.uuid, service());
        assert_eq!(
            gatt_service.characteristics.len(),
            1,
            "all mesh traffic uses one characteristic"
        );

        let characteristic = &gatt_service.characteristics[0];
        assert_eq!(
            characteristic.uuid.to_string().to_uppercase(),
            CHARACTERISTIC_UUID
        );

        let write = characteristic
            .write
            .as_ref()
            .expect("writes must be accepted");
        assert!(
            write.write_without_response,
            "BlueZ needs this flag before it will grant AcquireWrite"
        );
        assert!(write.write);
        assert!(matches!(write.method, CharacteristicWriteMethod::Io));

        let notify = characteristic
            .notify
            .as_ref()
            .expect("notifications must be offered");
        assert!(notify.notify);
        assert!(
            !notify.indicate,
            "indications would serialise the outbound path behind a round trip each"
        );
        assert!(matches!(notify.method, CharacteristicNotifyMethod::Io));
    }

    #[test]
    fn the_characteristic_is_readable_by_nobody() {
        // There is no value to read: the characteristic is a pipe, and offering a
        // read would let any scanner poll whatever was last written to us.
        let (_control, handle) = characteristic_control();
        let application = mesh_application(
            service(),
            crate::link::characteristic_uuid().expect("uuid"),
            handle,
        );
        assert!(application.services[0].characteristics[0].read.is_none());
    }
}
