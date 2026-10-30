//! The peripheral role: we advertise, and other devices connect to us.
//!
//! Half the mesh's connectivity lives here. A device that only scans is
//! invisible to peers scanning for it — and worse than that, it removes a bridge
//! that two otherwise-disconnected halves of the mesh might only have had through
//! it. Both roles run at once, always.
//!
//! # What gets published
//!
//! One advertisement carrying only the mesh service UUID, and one GATT
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

use std::sync::Arc;

use bluer::adv::{Advertisement, AdvertisementHandle, Type as AdvertisementType};
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicControl,
    CharacteristicControlEvent, CharacteristicControlHandle, CharacteristicNotify,
    CharacteristicNotifyMethod, CharacteristicWrite, CharacteristicWriteMethod, Service,
    characteristic_control,
};
use bluer::gatt::{CharacteristicReader, CharacteristicWriter};
use bluer::{Adapter, Uuid};
use chatter_mesh_link::{LinkError, LinkEvent, LinkRole};
use futures::StreamExt;
use tokio::task::JoinHandle;

use crate::error;
use crate::link::Shared;

/// Build the advertisement for the mesh service.
///
/// Separated out and unit-tested because the thing most worth checking about it
/// is what it does *not* contain.
fn mesh_advertisement(service: Uuid) -> Advertisement {
    Advertisement {
        // Connectable. `Broadcast` would advertise without accepting inbound
        // connections, which is exactly the invisible-peripheral failure.
        advertisement_type: AdvertisementType::Peripheral,
        service_uuids: [service].into_iter().collect(),
        discoverable: Some(true),
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
    _advertisement: AdvertisementHandle,
    _application: ApplicationHandle,
}

impl PeripheralRole {
    /// Advertise the service and start serving the GATT application.
    pub(crate) async fn start(
        adapter: &Adapter,
        service: Uuid,
        characteristic: Uuid,
        shared: Arc<Shared>,
    ) -> Result<Self, LinkError> {
        let (control, control_handle) = characteristic_control();

        let application = adapter
            .serve_gatt_application(mesh_application(service, characteristic, control_handle))
            .await
            .map_err(|err| error::classify_peripheral(&err))?;

        // Advertise only after the service is actually servable, so a peer that
        // reacts instantly to the first advertising packet does not find an
        // empty GATT database.
        let advertisement = adapter
            .advertise(mesh_advertisement(service))
            .await
            .map_err(|err| error::classify_peripheral(&err))?;

        let pump = tokio::spawn(serve_control_events(
            control,
            adapter.name().to_string(),
            shared,
        ));

        Ok(Self {
            pump,
            _advertisement: advertisement,
            _application: application,
        })
    }
}

impl Drop for PeripheralRole {
    fn drop(&mut self) {
        self.pump.abort();
    }
}

impl core::fmt::Debug for PeripheralRole {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PeripheralRole")
    }
}

/// Service the characteristic's control stream for as long as the role lives.
///
/// This must stay responsive: `bluer` delivers these events over a channel of
/// depth one from inside its D-Bus method handlers, so anything slow here blocks
/// `bluetoothd`'s call into us. Every branch therefore does the minimum and hands
/// the socket to its own task.
async fn serve_control_events(
    control: CharacteristicControl,
    adapter_name: String,
    shared: Arc<Shared>,
) {
    // `Box::pin` rather than `futures::pin_mut!`: this crate forbids unsafe code
    // and that macro expands to an `unsafe` block. One allocation, once per
    // session, for a stream that lives as long as the role does.
    let mut control = Box::pin(control);

    // The loop below ends only when the stack closes the control stream, and that
    // has to be reported: from that moment this device accepts no inbound
    // connections at all, while every other part of the radio carries on looking
    // healthy. Returning quietly is what made a half-deaf mesh indistinguishable
    // from a quiet room.
    while let Some(event) = control.next().await {
        match event {
            CharacteristicControlEvent::Write(request) => {
                let address = request.device_address().to_string();
                // Accepting hands BlueZ one end of a socket pair. A failure here
                // means the socket pair could not be created at all; there is no
                // link to report against and nothing useful to say, so the peer's
                // write simply fails and it may retry.
                if let Ok(reader) = request.accept() {
                    tokio::spawn(inbound(shared.clone(), address, reader));
                }
            }
            CharacteristicControlEvent::Notify(writer) => {
                let address = writer.device_address().to_string();
                // `bluer` has already reduced this from the negotiated ATT MTU by
                // BlueZ's per-operation headroom, so it is the largest payload a
                // single notification can carry. Reporting anything else — a
                // constant, or this minus another guess at ATT overhead — either
                // wastes bytes on every fragment for the life of the link or
                // produces packets the controller silently discards.
                let max_write = writer.mtu();
                let writer = Arc::new(writer);

                shared.open_link(
                    LinkRole::Peripheral,
                    &adapter_name,
                    &address,
                    max_write,
                    Some(writer.clone()),
                );
                tokio::spawn(outbound(shared.clone(), address, writer));
            }
        }
    }

    shared.emit(LinkEvent::PeripheralRoleEnded);
}

/// Read one peer's inbound writes until its channel closes.
async fn inbound(shared: Arc<Shared>, address: String, reader: CharacteristicReader) {
    loop {
        match reader.recv().await {
            // A zero-length datagram on a SEQPACKET socket is end of stream, and
            // a zero-length ATT write carries nothing either way.
            Ok(bytes) if bytes.is_empty() => break,
            Ok(bytes) => shared.inbound(LinkRole::Peripheral, &address, bytes),
            Err(_) => break,
        }
    }

    // The peer's write channel is gone, so nothing more is coming that could be
    // replayed on a future subscription. Its notification channel — and therefore
    // the link, if there is one — is tracked separately and is unaffected: a peer
    // that has stopped writing may still be listening.
    shared.forget_pending(LinkRole::Peripheral, &address);
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
        // The advertisement says "the mesh is here", and about *who* is here it
        // says only the mesh peer id — never anything tied to the hardware. A
        // local name would hand a stable device identifier to every passive
        // scanner in range, and unlike the peer id it would survive reinstalling
        // the app and identify the machine rather than the identity.
        let advertisement = mesh_advertisement(service());
        assert_eq!(advertisement.local_name, None);
        assert!(
            advertisement.system_includes.is_empty(),
            "'local-name' must not sneak in via Includes"
        );
        assert!(advertisement.manufacturer_data.is_empty());
        assert!(advertisement.advertising_data.is_empty());
        assert_eq!(advertisement.appearance, None);
        // Nothing to say when the runtime has not set an identity yet.
        assert!(advertisement.service_data.is_empty());
    }

    #[test]
    fn the_advertisement_stays_inside_one_legacy_packet() {
        // Adding service data here is the tempting fix for address-rotation churn
        // and it is the wrong one on this side: it overflows the legacy budget,
        // and BlueZ answers an overflow by forcing extended advertising rather
        // than by spilling into the scan response. A legacy-only scanner then
        // sees nothing at all. Android carries the peer id instead, where the
        // platform gives a real scan response to put it in.
        let advertisement = mesh_advertisement(service());
        let service_uuid_ad = 2 + 16;
        let used: usize = service_uuid_ad
            + advertisement
                .service_data
                .iter()
                .map(|(_, data)| 2 + 16 + data.len())
                .sum::<usize>();
        // 31 less the three-byte flags AD that BlueZ prepends for a discoverable
        // peripheral.
        assert!(
            used <= 31 - 3,
            "{used} bytes will not fit a legacy advertisement"
        );
    }

    #[test]
    fn the_advertisement_is_connectable_and_carries_the_mainnet_service() {
        let advertisement = mesh_advertisement(service());
        assert_eq!(
            advertisement.advertisement_type,
            AdvertisementType::Peripheral
        );
        assert_eq!(advertisement.discoverable, Some(true));
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
