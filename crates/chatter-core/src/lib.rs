//! Chatter Core — secure messaging SDK.
//!
//! Start with [`client::ChatterClient`] and the [`prelude`] module.

// The onion + mesh client graph needs this depth to prove auto traits on
// current nightly; retain checked Send derivation rather than a manual impl.
#![recursion_limit = "256"]

// ── Internal macros ────────────────────────────────────────────────────────
//
// `service_accessor!(name, Ty, field)` expands to a single getter on
// `ChatterClient` returning `&Arc<Ty>`. Default visibility is `pub(crate)`
// to keep chatter-ui from reaching past the façade. Enabling the `testing`
// feature widens the accessor to `pub` so integration tests (external to
// this crate) can wire up real services.
#[doc(hidden)]
#[macro_export]
macro_rules! service_accessor {
    ($name:ident, $ty:ty, $field:ident) => {
        #[cfg(feature = "testing")]
        pub fn $name(&self) -> &std::sync::Arc<$ty> {
            &self.services.$field
        }

        #[cfg(not(feature = "testing"))]
        pub(crate) fn $name(&self) -> &std::sync::Arc<$ty> {
            &self.services.$field
        }
    };
}

// ── Public API ──────────────────────────────────────────────────────────────

#[cfg(feature = "media")]
pub mod calls;
pub mod client;
pub mod crypto;
pub mod domain;
pub mod error;
pub mod events;
pub mod media;
pub mod prelude;
pub mod state;
pub mod system;
#[cfg(test)]
pub(crate) mod test_tempdir;
pub mod wire;

// ── Internal modules ────────────────────────────────────────────────────────

/// Arbiter between the two AI silos — outside both by design.
pub mod ai_policy;
#[doc(hidden)]
pub mod app;
/// Off-device inference providers. Siloed from `local_ai`.
pub mod cloud_ai;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod inbox;
#[doc(hidden)]
pub mod local_ai;
#[doc(hidden)]
pub mod mailbox;
#[doc(hidden)]
pub mod network;
#[doc(hidden)]
pub mod proto;
#[doc(hidden)]
pub mod services;
#[doc(hidden)]
pub mod storage;
#[doc(hidden)]
pub mod sync;
#[doc(hidden)]
pub mod ui;
#[doc(hidden)]
pub mod util;
