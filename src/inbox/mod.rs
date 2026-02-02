//! Device Inbox (Signal-style message queue).
//!
//! All incoming encrypted messages flow through the device inbox.

pub mod provider;
pub mod poller;
pub mod processor;
pub mod handlers;
pub mod queue;
