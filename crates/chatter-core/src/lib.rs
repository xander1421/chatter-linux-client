//! Chatter Core — secure messaging SDK.
//!
//! Start with [`client::ChatterClient`] and the [`prelude`] module.

// ── Public API ──────────────────────────────────────────────────────────────

pub mod client;
pub mod prelude;
pub mod domain;
pub mod events;
pub mod state;
pub mod error;
pub mod service_types;
pub mod wire;
pub mod media;
#[cfg(feature = "media")]
pub mod calls;
pub mod crypto;
pub mod system;

// ── Internal modules ────────────────────────────────────────────────────────

#[doc(hidden)]
pub mod app;
#[doc(hidden)]
pub mod storage;
#[doc(hidden)]
pub mod network;
#[doc(hidden)]
pub mod services;
#[doc(hidden)]
pub mod sync;
#[doc(hidden)]
pub mod inbox;
#[doc(hidden)]
pub mod mailbox;
#[doc(hidden)]
pub mod ui;
#[cfg(feature = "local-ai")]
#[doc(hidden)]
pub mod local_ai;
#[doc(hidden)]
pub mod util;
#[doc(hidden)]
pub mod proto;
#[doc(hidden)]
pub mod config;
