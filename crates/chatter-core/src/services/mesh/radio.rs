//! Picking the platform radio.
//!
//! One `cfg` fork, in one place. `BlueZLink` exists only on Linux and
//! `AndroidLink` only on Android — both crates compile to an empty shell
//! elsewhere — so the "which radio" decision cannot be made at runtime and must
//! not be scattered.
//!
//! On every other target there is no radio, and that is a supported
//! configuration rather than a build failure: the service still constructs, the
//! Nostr half still carries geohash channels and gift-wrapped DMs, and every
//! BLE operation reports [`MeshError::NoRadio`](super::error::MeshError::NoRadio).

use std::sync::Arc;

use chatter_mesh_link::MeshLink;

/// The radio for this target, or `None` where there is not one.
#[must_use]
pub fn platform_radio() -> Option<Arc<dyn MeshLink>> {
    build()
}

#[cfg(target_os = "linux")]
fn build() -> Option<Arc<dyn MeshLink>> {
    Some(Arc::new(chatter_mesh_ble::BlueZLink::new()))
}

#[cfg(target_os = "android")]
fn build() -> Option<Arc<dyn MeshLink>> {
    Some(Arc::new(chatter_mesh_android::AndroidLink::new()))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn build() -> Option<Arc<dyn MeshLink>> {
    None
}

/// Whether this build has a radio at all.
///
/// A `const` rather than a probe: it is a property of the target, not of the
/// machine. Whether the adapter is *present and powered* is a separate question,
/// answered by `LinkEvent::AdapterPowerChanged`.
pub const HAS_RADIO: bool = cfg!(any(target_os = "linux", target_os = "android"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_radio_matches_the_target() {
        assert_eq!(platform_radio().is_some(), HAS_RADIO);
    }
}
