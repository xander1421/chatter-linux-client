//! Domain event handling for UI updates.
//!
//! All UI updates are driven by DomainEvent subscriptions in `domain_handler`.
//! Protocol concerns (receipts, convergence, sender keys) are handled by
//! core's `InboxEventProcessor` before events reach the UI.
//!
//! # Architecture
//!
//! ```text
//! Polling → InboxEventProcessor (core) → DomainEvent → domain_handler (UI only)
//! ```

pub mod attachment_download;
pub mod domain_handler;
pub mod typing;
pub(crate) mod unified_actions;
pub mod utils;
