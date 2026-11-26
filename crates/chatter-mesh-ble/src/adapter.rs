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

use std::io::ErrorKind;
use std::time::Duration;

use bluer::{Adapter, AdapterEvent, AdapterProperty, Session};
use chatter_mesh_link::LinkError;
use futures::{Stream, StreamExt};
use tokio::{process::Command, time::timeout};

use crate::error;

const CONTROLLER_INFO_TIMEOUT: Duration = Duration::from_secs(2);
const MESH_ALIAS: &str = "Bluetooth Device";

/// A BlueZ session together with the adapter we run the mesh on.
pub struct BleAdapter {
    session: Session,
    adapter: Adapter,
}

impl BleAdapter {
    /// Open a session and take the default adapter (`hci0` when present).
    pub async fn acquire() -> Result<Self, LinkError> {
        let session = Session::new()
            .await
            .map_err(|err| error::classify_adapter(&err))?;
        let adapter = session
            .default_adapter()
            .await
            .map_err(|err| error::classify_adapter(&err))?;
        Ok(Self { session, adapter })
    }

    /// Open a session and take a named adapter, e.g. `hci1` on a machine with
    /// more than one controller.
    pub async fn acquire_named(name: &str) -> Result<Self, LinkError> {
        let session = Session::new()
            .await
            .map_err(|err| error::classify_adapter(&err))?;
        let adapter = session
            .adapter(name)
            .map_err(|err| error::classify_adapter(&err))?;
        // `Session::adapter` only builds a proxy; it does not check that the
        // adapter exists. Touch a mandatory property so a typo fails here rather
        // than as a confusing GATT registration error later.
        adapter
            .address()
            .await
            .map_err(|err| error::classify_adapter(&err))?;
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
        self.adapter
            .is_powered()
            .await
            .map_err(|err| error::classify_adapter(&err))
    }

    /// Check that mesh can use the controller without changing its settings.
    ///
    /// BlueZ's D-Bus `AddressType` describes the controller's identity address.
    /// A public identity can still advertise from a private address, and a
    /// random identity can be static. Read the kernel's live Privacy setting.
    pub(crate) async fn require_private_radio(&self) -> Result<(), LinkError> {
        if !self.is_powered().await? {
            return Err(self.bluetooth_off());
        }

        // Read Controller Information is an unprivileged management command.
        let mut command = Command::new("btmgmt");
        command
            .args(["--index", self.name(), "info"])
            .env("LC_ALL", "C")
            .kill_on_drop(true);
        let settings = match timeout(CONTROLLER_INFO_TIMEOUT, command.output()).await {
            Ok(Ok(output)) if output.status.success() => std::str::from_utf8(&output.stdout)
                .ok()
                .and_then(current_settings)
                .ok_or("could not read the controller's current LE address privacy setting"),
            Ok(Err(error)) if error.kind() == ErrorKind::NotFound => {
                Err("install the BlueZ btmgmt tool to verify LE address privacy")
            }
            _ => Err("could not verify LE address privacy with btmgmt"),
        }
        .map_err(|reason| self.private_radio_unavailable(reason))?;
        if !settings.powered {
            return Err(self.bluetooth_off());
        }
        if !settings.privacy {
            return Err(self.private_radio_unavailable(
                "LE address privacy is off. Enable Privacy=device in the BlueZ configuration",
            ));
        }
        Ok(())
    }

    fn private_radio_unavailable(&self, reason: &str) -> LinkError {
        LinkError::PeripheralSetup(format!(
            "{}: {reason}. Mesh requires a private Bluetooth address",
            self.name()
        ))
    }

    fn bluetooth_off(&self) -> LinkError {
        LinkError::AdapterUnavailable(format!(
            "Bluetooth is off on {}. Turn it on in system settings, then turn Mesh off and back on",
            self.name()
        ))
    }
}

/// Restore the adapter's name when the radio session ends.
pub(crate) struct MeshAlias {
    adapter: Adapter,
    previous: Option<(String, bool)>,
}

impl MeshAlias {
    pub(crate) async fn install(adapter: &Adapter) -> Result<Self, bluer::Error> {
        let current = adapter.alias().await?;
        let previous = if current == MESH_ALIAS {
            None
        } else {
            let system_fallback = current == adapter.system_name().await?;
            Some((current, system_fallback))
        };
        let alias = Self {
            adapter: adapter.clone(),
            previous,
        };
        if alias.previous.is_some() {
            adapter.set_alias(MESH_ALIAS.to_owned()).await?;
        }
        Ok(alias)
    }

    pub(crate) async fn restore(&mut self) -> Result<(), bluer::Error> {
        if let Some(previous) = &self.previous {
            restore_alias(&self.adapter, previous.clone()).await?;
            self.previous = None;
        }
        Ok(())
    }
}

async fn restore_alias(
    adapter: &Adapter,
    (previous, system_fallback): (String, bool),
) -> Result<(), bluer::Error> {
    // Preserve an alias that the user or another application set during mesh.
    // BlueZ updates its stored alias before the controller reports the change.
    let current = adapter.alias().await?;
    if current == MESH_ALIAS || current == previous {
        // An empty alias restores BlueZ's system-name fallback.
        let previous = if system_fallback {
            String::new()
        } else {
            previous
        };
        adapter.set_alias(previous).await?;
    }
    Ok(())
}

impl Drop for MeshAlias {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let adapter = self.adapter.clone();
            runtime.spawn(async move {
                if let Err(error) = restore_alias(&adapter, previous).await {
                    tracing::warn!(%error, "could not restore the Bluetooth adapter alias");
                }
            });
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ControllerSettings {
    powered: bool,
    privacy: bool,
}

fn current_settings(info: &str) -> Option<ControllerSettings> {
    let mut lines = info
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("current settings:"));
    let settings = lines.next()?.trim();
    if settings.is_empty() || lines.next().is_some() {
        return None;
    }
    let mut settings = settings.split_ascii_whitespace();
    Some(ControllerSettings {
        powered: settings.clone().any(|setting| setting == "powered"),
        privacy: settings.any(|setting| setting == "privacy"),
    })
}

impl core::fmt::Debug for BleAdapter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BleAdapter")
            .field("adapter", &self.name())
            .finish()
    }
}

/// Wait for the controller to power off or disappear from D-Bus.
pub(crate) async fn wait_for_power_loss(events: impl Stream<Item = AdapterEvent>) {
    let mut events = std::pin::pin!(events);
    while let Some(event) = events.next().await {
        if let AdapterEvent::PropertyChanged(AdapterProperty::Powered(false)) = event {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_current_settings_instead_of_capabilities() {
        let info = "hci0: Primary controller\n\tsupported settings: powered le privacy\n\tcurrent settings: powered le";
        assert_eq!(
            current_settings(info),
            Some(ControllerSettings {
                powered: true,
                privacy: false,
            })
        );
        assert_eq!(
            current_settings("current settings: le privacy"),
            Some(ControllerSettings {
                powered: false,
                privacy: true,
            })
        );
        assert_eq!(
            current_settings("supported settings: powered le privacy"),
            None
        );
        assert_eq!(current_settings("current settings:"), None);
        assert_eq!(
            current_settings("current settings: powered le\ncurrent settings: powered le privacy"),
            None
        );
    }

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
        let _ = ble.is_powered().await.expect("power read failed");
    }
}
