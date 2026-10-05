//! AI policy — the arbiter between the two AI silos.
//!
//! Deliberately outside both [`crate::local_ai`] and [`crate::cloud_ai`]:
//! it decides *which silo* a conversation may use, so it must not live inside
//! either one. Neither silo depends on this module; this module depends on
//! neither silo's concrete providers.
//!
//! * [`policy`] — the rules (mesh → nothing, DM → local, channel → either)
//!   and the per-channel file-scoping workspace.
//! * [`channel_ai`] — per-conversation configuration, resolved against those
//!   rules.

pub mod channel_ai;
pub mod policy;

pub use channel_ai::{Resolved, SiloPreference};
pub use policy::{AiAccess, AiWorkspace, Silo, ai_access};
