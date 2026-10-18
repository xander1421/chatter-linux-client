//! Transport selection: how a private message actually reaches someone.
//!
//! Ported from bitchat's `Services/MessageRouter.swift`.
//!
//! # The decision tree
//!
//! Four outcomes, tried in order. The differences between them are the whole point
//! of the module, and they fall out of the four reachability predicates on
//! [`RouterTransport`], which mean genuinely different things:
//!
//! 1. **Connected *and* [`can_deliver_securely`] — send, done.** A live link that
//!    can complete an encrypted delivery is a strong enough signal to trust
//!    outright, so nothing is retained.
//! 2. **Connected, no secure session — send, retain, *and* deposit with couriers.**
//!    "Connected" without an established session is forgeable, so the send may be
//!    stalling on a handshake the peer at the other end can never complete.
//! 3. **Reachable but not connected — send and retain.** Reachability is a freshness
//!    heuristic, so the send can silently go nowhere. If the transport also cannot
//!    [`can_deliver_promptly`], deposit with couriers too: the send only joined a
//!    queue waiting on internet that may never arrive.
//! 4. **Nothing — retain with zero attempts and deposit with couriers.**
//!
//! The same four cases drive [`flush_outbox`](MessageRouter::flush_outbox), where
//! they are expressed as [`LinkState`] and the per-message consequences belong to
//! [`OutboxStore::flush`]. This module owns the *choice*; the outbox owns what the
//! choice does to retained mail.
//!
//! [`can_deliver_securely`]: RouterTransport::can_deliver_securely
//! [`can_deliver_promptly`]: RouterTransport::can_deliver_promptly
use chatter_mesh::ids::PeerId;

use crate::error::StoreError;
use crate::outbox::{DroppedMessage, LinkState, OutboxStore};
use crate::transport::RouterTransport;

mod courier;
mod outbox;
mod route;
mod send;
#[cfg(test)]
mod tests;

/// Identity lookups the router needs to address couriered mail.
///
/// Injected rather than reached for: peer keys live in the app's mesh state, and
/// this crate is sans-IO. Whether a peer may carry mail is read off the transport
/// snapshot (`is_verified`), not from here.
pub trait CourierDirectory {
    /// The recipient's Noise static key, when we know one.
    ///
    /// A peer whose key we do not have cannot be addressed offline at all — nothing
    /// can be sealed to them — so `None` disables the whole courier path for them.
    fn noise_key(&self, peer: &PeerId) -> Option<[u8; 32]>;
}

/// Parks a sealed copy on relays as a courier drop, so delivery stops requiring a
/// physical courier encounter.
///
/// Injected because `bridge_drop.rs` owns the relay side. Called with the message
/// content, its ID, and the recipient's static key. Must return true only once a
/// relay has *accepted* the event: a socket write followed by a rejection would
/// otherwise show the sender's message as carried when nothing holds it.
///
/// Runs alongside mesh couriers rather than instead of them; receivers dedup by
/// message ID.
pub type BridgeDeposit = Box<dyn Fn(&str, &str, &[u8; 32]) -> bool>;

/// Something the caller has to react to.
///
/// Returned rather than invoked as a callback: the router is sans-IO, and a
/// `&mut self` method that also calls back into the app is how re-entrancy bugs
/// start. bitchat's `onMessageDropped` / `onMessageCarried` map onto these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouterEvent {
    /// Retained mail left the outbox without a delivery ack. The UI must surface a
    /// failure rather than leaving the message on "sending" forever — see
    /// [`DroppedMessage::reason`] for which bound was hit.
    Dropped(DroppedMessage),

    /// The message was handed to at least one courier, or parked as a bridge drop.
    /// Delivery stays best-effort: the retained copy is only cleared by an ack.
    Carried { message_id: String, peer: PeerId },

    /// Retained mail could not be read from or written to durable storage. Never
    /// fatal: losing the durable copy is bad, refusing to send is worse. The
    /// in-memory copy survives the session, and [`OutboxStore`] still owes the
    /// write.
    StorageFailed { error: StoreError },
}

/// The result of a send: which branch of the tree was taken, and what the caller
/// must react to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendOutcome {
    /// The route the message took. [`LinkState::Secure`] is the only one that does
    /// not retain a copy.
    pub link: LinkState,
    pub events: Vec<RouterEvent>,
}

/// Routes private messages across the available transports.
pub struct MessageRouter {
    transports: Vec<Box<dyn RouterTransport>>,
    directory: Box<dyn CourierDirectory>,
    outbox: OutboxStore,
    bridge_deposit: Option<BridgeDeposit>,
}

impl MessageRouter {
    pub fn new(
        transports: Vec<Box<dyn RouterTransport>>,
        directory: Box<dyn CourierDirectory>,
        outbox: OutboxStore,
    ) -> Self {
        Self {
            transports,
            directory,
            outbox,
            bridge_deposit: None,
        }
    }

    /// Wire the relay-side courier drop. Without it the router still couriers over
    /// the mesh; the bridge simply never fires.
    pub fn set_bridge_deposit(&mut self, deposit: BridgeDeposit) {
        self.bridge_deposit = Some(deposit);
    }

    pub fn outbox(&self) -> &OutboxStore {
        &self.outbox
    }

    pub fn outbox_mut(&mut self) -> &mut OutboxStore {
        &mut self.outbox
    }
}
