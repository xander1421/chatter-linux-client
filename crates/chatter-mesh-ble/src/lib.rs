//! BlueZ implementation of [`chatter_mesh_link::MeshLink`].
//!
//! Linux only, and gated twice: the `bluer` dependency is target-gated in
//! `Cargo.toml` and the modules are `cfg`-gated here, so an Android or Windows
//! build resolves this crate to an empty shell rather than failing to find a D-Bus
//! client.
//!
//! # Both roles, always
//!
//! Every device advertises a GATT service *and* scans for others. Which role a link
//! ends up using is decided by whoever discovers whom first. A scan-only
//! implementation is invisible to peers scanning for it, which does not merely halve
//! its own connectivity — it removes a potential bridge between two halves of the
//! mesh that could otherwise only reach each other through it.
//!
//! # What is not verifiable here
//!
//! There is no bitchat source to port for this layer (they use CoreBluetooth) and no
//! test vectors exist. It is verified by running it against a real adapter. A single
//! adapter cannot connect to itself, so genuine two-device testing is out of reach
//! on one machine and remains an open gap.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

#[cfg(target_os = "linux")]
pub mod adapter;
#[cfg(target_os = "linux")]
pub mod central;
#[cfg(target_os = "linux")]
pub mod error;
#[cfg(target_os = "linux")]
pub mod link;
#[cfg(target_os = "linux")]
pub mod peripheral;

#[cfg(target_os = "linux")]
pub use link::BlueZLink;
