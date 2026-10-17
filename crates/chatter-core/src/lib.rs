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

// Temporary external-access allowlist while the client façade is expanded:
// - app, mailbox, proto, and util: core integration fixtures construct or
//   inspect these implementation objects directly.
// - network, services, and storage: current client signatures and platform/UI
//   adapters still expose transport, service, and persistence types.
// - config, inbox, local_ai, cloud_ai, ai_policy, and ui: consumers still use
//   constants, event/AI contracts, and prepared view data from these modules.
// - update: the desktop updater is consumed directly by chatter-ui.
// Keep additions here explicit; `sync` has no external caller and is private.

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
pub(crate) mod sync;
pub mod update;
#[doc(hidden)]
pub mod ui;
#[doc(hidden)]
pub mod util;
