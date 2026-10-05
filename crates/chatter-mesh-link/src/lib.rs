//! Link policy and the radio abstraction for the bitchat mesh.
//!
//! Two things live here, and the split is deliberate:
//!
//! - [`link`] defines [`MeshLink`], the trait a platform radio implements. Linux
//!   supplies it from `chatter-mesh-ble` via BlueZ; Android supplies it over JNI.
//! - Everything else is **sans-IO policy**: when to connect, when to scan, which
//!   links an outbound packet goes on, what to do when a write is refused. None of
//!   it touches a radio, so all of it is testable without one.
//!
//! That division is what makes the interesting decisions — the ones that drain a
//! battery or storm a crowded room — reviewable and unit-tested, while the part
//! that genuinely needs hardware stays small enough to verify by running it.
//!
//! Ported from bitchat's `Services/BLE/` policy classes, which are already factored
//! this way in Swift.

#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod announce;
pub mod backpressure;
pub mod duty;
pub mod error;
pub mod fanout;
pub mod guard;
pub mod link;
pub mod planner;
pub mod registry;
pub mod scheduler;

pub use error::LinkError;
pub use link::{
    BoxFuture, CHARACTERISTIC_UUID, LinkEvent, LinkId, LinkRole, MeshLink, SERVICE_UUID,
};
