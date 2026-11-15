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

// Modules still reachable from outside the crate, and why:
// - mailbox and util: core integration fixtures construct or inspect
//   these implementation objects directly.
// - app: exposed only to integration fixtures under `testing`.
// - config, local_ai, cloud_ai, ai_policy, and ui: consumers still use
//   constants, AI contracts, and prepared view data from these modules.
// - update: the desktop updater is consumed directly by chatter-ui.
// `services`, `storage`, `network`, `inbox` and `sync` are crate-private;
// the types consumers need are re-exported by name from `client`,
// `events`, `media` and `ui`. Integration tests reach the internals through
// `testing` below.

/// Arbiter between the two AI silos — outside both by design.
pub mod ai_policy;
#[doc(hidden)]
pub mod ai_protocol;
#[cfg(feature = "testing")]
#[doc(hidden)]
pub mod app;
#[cfg(not(feature = "testing"))]
pub(crate) mod app;
/// Off-device inference providers. Siloed from `local_ai`.
pub mod cloud_ai;
#[doc(hidden)]
pub mod config;
pub(crate) mod inbox;
#[cfg(feature = "local-ai")]
pub mod local_ai;
#[doc(hidden)]
pub mod mailbox;
pub(crate) mod network;
pub(crate) mod services;
pub(crate) mod storage;
pub(crate) mod sync;
#[doc(hidden)]
pub mod ui;
pub mod update;
#[doc(hidden)]
pub mod util;

/// Crate internals for integration tests and fixtures (`tests/`, test code
/// in chatter-ui). Not part of the API; only built with `testing`.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub mod testing {
    pub mod inbox {
        pub use crate::inbox::*;
    }
    pub mod network {
        pub use crate::network::*;
    }
    pub mod services {
        pub use crate::services::*;
    }
    pub mod storage {
        pub use crate::storage::*;
    }
}
