//! The radio abstraction every platform implements.
//!
//! One trait, three implementations: `bluer` over BlueZ on Linux, JNI into the
//! Android Bluetooth APIs, and a mock in tests. That is why it is `dyn`-dispatched
//! with boxed futures rather than `async fn` in a trait — the choice is made at
//! runtime, and a generic would force it into every type that touches a radio.
//!
//! Nothing here knows about Bluetooth specifically. A link is something you can
//! write bytes to and read events from; the policy layer above never learns which
//! stack is underneath.

use core::future::Future;
use core::pin::Pin;

use crate::error::LinkError;

/// A future returned across the trait boundary.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Identifies one physical connection for as long as it exists.
///
/// Opaque and backend-assigned: a D-Bus object path on Linux, a `BluetoothDevice`
/// handle on Android. Deliberately **not** a peer ID — a link is bound to a peer
/// identity only after that peer proves it with a signed announce, and conflating
/// the two is what lets a replayed announce steal someone else's link.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LinkId(String);

impl LinkId {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for LinkId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which side of a link we are.
///
/// Every device runs both roles simultaneously. Which one a given link uses depends
/// only on who discovered whom, and the mesh treats the two identically once
/// established — but the send path differs (a central writes to a characteristic, a
/// peripheral notifies subscribers), so the link layer has to track it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkRole {
    /// We connected outward to them.
    Central,
    /// They connected inward to us.
    Peripheral,
}

/// Something the radio noticed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkEvent {
    /// A device advertising our service UUID came into range.
    ///
    /// Not a connection — the scheduler decides whether this is worth spending one
    /// of the scarce central slots on, using `rssi` among other things.
    ///
    /// `peer` is the advertised identity, when the advert carried one. It is the
    /// only thing about a device that survives a private-address rotation, and
    /// therefore the only way to recognise an address as a peer already held
    /// *before* spending a connection to find out. `None` from a peer whose scan
    /// response was not received — treat that as unknown, never as unfamiliar.
    Discovered { address: String, rssi: i16, peer: Option<[u8; 8]> },

    /// A link is up **and writable right now**.
    ///
    /// Writability is part of the contract, not an implication. A central link is
    /// not writable until the characteristic has been discovered, and a peripheral
    /// link is not writable until the remote central has subscribed — both normally
    /// land *after* the connection itself comes up. An implementation must therefore
    /// withhold this event until it can actually carry a write.
    ///
    /// The alternative — emitting on connect and adding a separate "now writable"
    /// event — was considered and rejected: a link the mesh cannot write to is of no
    /// use to the layer above, and the early event would exist only to be ignored.
    /// The failure mode for getting this wrong is silent dropped writes rather than
    /// a compile error, which is why it is spelled out here.
    ///
    /// `max_write` is negotiated per link and must not be assumed: fragment sizing
    /// depends on it, and guessing high means every oversized packet is silently
    /// dropped by the controller.
    Connected {
        link: LinkId,
        role: LinkRole,
        max_write: usize,
    },

    /// A link is gone. Any state keyed on it should be dropped, not retried.
    Disconnected { link: LinkId },

    /// Bytes arrived on a link. One event is one write from the peer, which is not
    /// necessarily one packet — reassembly happens above.
    Data { link: LinkId, bytes: Vec<u8> },

    /// The adapter was powered on or off underneath us, e.g. by a system toggle
    /// or airplane mode. Every link dies with it.
    AdapterPowerChanged { powered: bool },

    /// This device can no longer be *connected to*, though it may still connect out.
    ///
    /// A peripheral role serves inbound subscriptions; when the stack ends that
    /// stream the role is over, and from then on the mesh advertises normally,
    /// scans normally, dials out normally — and silently accepts nobody. Half the
    /// mesh, with no error anywhere, which is why this is an event rather than a
    /// return value: nothing was being called at the moment it happened.
    ///
    /// Observed as a desktop registering zero inbound links across an entire
    /// session while a phone in the same room reported twenty-one outbound
    /// connections to it, every one having subscribed successfully from its own
    /// side. The two facts could not be reconciled from either log.
    PeripheralRoleEnded,
}

/// A bidirectional radio.
///
/// Implementations own their own connection state and event queue. The policy layer
/// drives them: it decides what to connect to and what to send, and reacts to what
/// comes back out of [`MeshLink::next_event`].
pub trait MeshLink: Send + Sync {
    /// Begin advertising and accepting inbound connections.
    ///
    /// Both roles start together. A device that only scans is invisible to peers
    /// scanning for it, which halves the connectivity of the whole mesh rather than
    /// just its own.
    fn start(&self) -> BoxFuture<'_, Result<(), LinkError>>;

    /// Stop advertising, stop scanning, and drop every link.
    fn stop(&self) -> BoxFuture<'_, Result<(), LinkError>>;

    /// Turn scanning on or off without tearing down the peripheral role.
    ///
    /// This is the duty cycle's lever: scanning is the expensive half, so it runs
    /// intermittently while advertising stays up continuously.
    fn set_scanning(&self, on: bool) -> BoxFuture<'_, Result<(), LinkError>>;

    /// Connect outward to a discovered address, taking the central role.
    fn connect(&self, address: &str) -> BoxFuture<'_, Result<LinkId, LinkError>>;

    /// Drop a link we no longer want.
    fn disconnect(&self, link: &LinkId) -> BoxFuture<'_, Result<(), LinkError>>;

    /// Write one payload to one link.
    ///
    /// Returns [`LinkError::WriteRejected`] when the controller queue is full. That
    /// is a routine, retryable condition rather than a failure — there is no
    /// retransmission below us, so a caller that discards on rejection loses the
    /// packet outright.
    fn send(&self, link: &LinkId, bytes: &[u8]) -> BoxFuture<'_, Result<(), LinkError>>;

    /// Largest single write this link accepts, or `None` if it is gone.
    fn max_write(&self, link: &LinkId) -> Option<usize>;

    /// Next event, or `None` once the link layer has shut down for good.
    fn next_event(&self) -> BoxFuture<'_, Option<LinkEvent>>;

    /// Publish this device's identity in the advertisement, so peers can recognise
    /// it across a private-address rotation.
    ///
    /// Defaulted to a no-op: a link with no radio behind it has nothing to
    /// advertise, and a test double should not have to care. Call it before
    /// [`start`](Self::start) — an advertiser already running will not pick it up.
    fn set_local_peer(&self, _peer: [u8; 8]) {}
}

/// bitchat's mainnet GATT service UUID.
///
/// Their debug builds use `…4B5A` instead. We always advertise mainnet: the point is
/// talking to shipping clients, and a build on the testnet UUID is invisible to every
/// real peer while looking completely healthy from the inside.
pub const SERVICE_UUID: &str = "F47B5E2D-4A9E-4C5A-9B3F-8E1D2C3A4B5C";

/// The single characteristic carrying all mesh traffic, both directions.
/// Write-without-response outbound, notify inbound.
pub const CHARACTERISTIC_UUID: &str = "A1B2C3D4-E5F6-4A5B-8C9D-0E1F2A3B4C5D";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_ids_are_opaque_and_comparable() {
        let a = LinkId::new("/org/bluez/hci0/dev_AA_BB");
        let b = LinkId::new("/org/bluez/hci0/dev_AA_BB");
        let c = LinkId::new("/org/bluez/hci0/dev_CC_DD");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.as_str(), "/org/bluez/hci0/dev_AA_BB");
        assert_eq!(a.to_string(), a.as_str());
    }

    #[test]
    fn service_uuid_is_mainnet_not_the_debug_one() {
        // The two differ only in the final character, which is exactly why this
        // is worth pinning: a one-character slip is invisible in review and
        // produces a build no real peer can see.
        assert!(SERVICE_UUID.ends_with("4B5C"));
        assert!(!SERVICE_UUID.ends_with("4B5A"));
        assert_eq!(SERVICE_UUID.len(), 36);
        assert_eq!(CHARACTERISTIC_UUID.len(), 36);
    }
}
