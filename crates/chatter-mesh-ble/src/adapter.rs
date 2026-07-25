//! Session and adapter acquisition, power state, and power-change watching.
//!
//! Everything else in this crate needs a live [`bluer::Adapter`], and an adapter
//! is only reachable through a [`bluer::Session`] that owns the D-Bus connection
//! to `bluetoothd`. Dropping the session closes that connection and silently
//! invalidates every adapter, device, advertisement and GATT handle derived from
//! it, so [`BleAdapter`] keeps the two together and hands out clones of the
//! adapter rather than letting callers hold the session themselves.
//!
//! # Why power is watched rather than merely read
//!
//! `Powered` can change underneath us at any moment — a desktop toggle, airplane
//! mode, `rfkill`, a suspend/resume cycle. When it goes false every ACL dies at
//! once, and the layer above has to learn that from one event instead of from a
//! dozen simultaneous write failures.

use futures::StreamExt;

use bluer::{Adapter, AdapterEvent, AdapterProperty, Session};
use chatter_mesh_link::LinkError;

use crate::error;

/// A BlueZ session together with the adapter we run the mesh on.
pub struct BleAdapter {
    session: Session,
    adapter: Adapter,
}

impl BleAdapter {
    /// Open a session and take the default adapter (`hci0` when present).
    pub async fn acquire() -> Result<Self, LinkError> {
        let session = Session::new().await.map_err(|err| error::classify_adapter(&err))?;
        let adapter = session.default_adapter().await.map_err(|err| error::classify_adapter(&err))?;
        Ok(Self { session, adapter })
    }

    /// Open a session and take a named adapter, e.g. `hci1` on a machine with
    /// more than one controller.
    pub async fn acquire_named(name: &str) -> Result<Self, LinkError> {
        let session = Session::new().await.map_err(|err| error::classify_adapter(&err))?;
        let adapter = session.adapter(name).map_err(|err| error::classify_adapter(&err))?;
        // `Session::adapter` only builds a proxy; it does not check that the
        // adapter exists. Touch a mandatory property so a typo fails here rather
        // than as a confusing GATT registration error later.
        adapter.address().await.map_err(|err| error::classify_adapter(&err))?;
        Ok(Self { session, adapter })
    }

    /// The adapter. Cheap to clone; clones stay valid while this value lives.
    pub fn adapter(&self) -> &Adapter {
        &self.adapter
    }

    /// The session backing this adapter.
    ///
    /// Held so that the D-Bus connection outlives everything derived from it.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Adapter name, e.g. `hci0`.
    pub fn name(&self) -> &str {
        self.adapter.name()
    }

    /// Is the controller currently powered?
    pub async fn is_powered(&self) -> Result<bool, LinkError> {
        self.adapter.is_powered().await.map_err(|err| error::classify_adapter(&err))
    }

    /// Bring the controller up if it is not already.
    ///
    /// This does change system state the user can see, which is deliberate: BlueZ
    /// refuses to advertise, scan or connect on an unpowered adapter, so the only
    /// alternative is to fail to start at all. `Powered` is also advisory — an
    /// `rfkill` block accepts the write and leaves the property false — so the
    /// result is read back rather than assumed.
    pub async fn power_on(&self) -> Result<(), LinkError> {
        if self.is_powered().await? {
            return Ok(());
        }
        self.adapter.set_powered(true).await.map_err(|err| error::classify_adapter(&err))?;
        if self.is_powered().await? {
            Ok(())
        } else {
            Err(LinkError::AdapterUnavailable(format!(
                "{} stayed powered off after being switched on; it is probably rfkill-blocked",
                self.name()
            )))
        }
    }
}

impl core::fmt::Debug for BleAdapter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BleAdapter").field("adapter", &self.name()).finish()
    }
}

/// Report every power transition of `adapter` to `on_change` until the adapter
/// object disappears from D-Bus.
///
/// Takes the adapter by value so the whole thing can be spawned as a detached
/// task: `Adapter::events` borrows its receiver, so a caller cannot hold the
/// stream and the adapter in different places.
///
/// The adapter object vanishing (`hci0` unbound, the dongle unplugged) is
/// reported as a final `false`. From the mesh's point of view that is
/// indistinguishable from a power-off — the radio is gone and every link with
/// it — and reporting nothing would leave the layer above believing its links
/// were still alive.
///
/// Cancelling the returned future (dropping the task) reports nothing, because a
/// deliberate shutdown is not a power event.
pub async fn watch_power<F>(adapter: Adapter, mut on_change: F)
where
    F: FnMut(bool) + Send,
{
    let events = match adapter.events().await {
        Ok(events) => events,
        // We could not subscribe at all, so we will never see a transition.
        // Saying "powered off" here would be a lie: the adapter may be perfectly
        // healthy and start() has already verified it is powered.
        Err(_) => return,
    };
    // `Box::pin` rather than `futures::pin_mut!`, which expands to an `unsafe`
    // block this crate forbids.
    let mut events = Box::pin(events);

    while let Some(event) = events.next().await {
        if let AdapterEvent::PropertyChanged(AdapterProperty::Powered(powered)) = event {
            on_change(powered);
        }
    }

    on_change(false);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires a real controller. Run with:
    ///
    /// ```text
    /// cargo test -p chatter-mesh-ble -- --ignored acquires_the_default_adapter
    /// ```
    #[tokio::test]
    #[ignore = "needs a Bluetooth adapter and a running bluetoothd"]
    async fn acquires_the_default_adapter() {
        let ble = BleAdapter::acquire().await.expect("no adapter");
        assert!(!ble.name().is_empty());
        ble.power_on().await.expect("could not power on");
        assert!(ble.is_powered().await.expect("power read failed"));
    }
}
