//! Call signaling service.
//!
//! Handles the Signal Protocol side of call setup: building call key exchange
//! protos and sending them via pairwise DM (sealed sender). This keeps
//! protocol details out of the UI layer.
//!
//! # DM Calls (1:1)
//! Uses ephemeral X25519 key exchange via `CallKeyExchange` protos.
//! Offer → Answer → both derive shared secret → AES key for media.
//!
//! # Group Calls
//! Signalled pairwise over sealed sender (`GroupCallSignal` proto). As in
//! Signal, the ring carries no key: a random AES-256 key is sent only by the
//! call's key authority, only to the participants in the room, on every join
//! and leave (`client/calls.rs`). A participant without one asks for it.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use zeroize::Zeroizing;

#[cfg(feature = "media")]
use crate::crypto::call_key_provider::EphemeralCallKey;
#[cfg(feature = "media")]
use crate::domain::call::CallSetupResult;
#[cfg(feature = "media")]
use crate::domain::call::CallType;
use crate::domain::call::PendingDmCall;

#[cfg(feature = "media")]
use crate::domain::ids::CallId;
use crate::domain::ids::{ChannelId, UserId};
#[cfg(feature = "media")]
use crate::services::call_verification::CallVerification;
#[cfg(feature = "media")]
use crate::services::call_worker::{
    CallEndReason, CallHandle, CallWorkerHandle, OrchestratedCallKind, RING_TIMEOUT,
};
use crate::services::channel::ChannelService;
use crate::services::gateway::{
    CallHangupReason, build_call_hangup_content, build_call_key_answer, build_call_key_offer,
    build_group_call_signal_content,
};
use crate::services::message::MessageService;
#[cfg(feature = "media")]
use crate::services::signal::SignalService;

/// Errors from call signaling operations.
#[derive(Debug, thiserror::Error)]
pub enum CallSignalingError {
    #[error("message send failed: {0}")]
    Send(#[from] crate::services::message::MessageError),
    #[error("channel error: {0}")]
    Channel(#[from] crate::services::channel::ChannelError),
    #[error("no DM peer found for channel {0}")]
    NoPeer(ChannelId),
    #[error("no pending DM call to complete")]
    NoPendingCall,
    #[error("call ID mismatch: expected {expected}, got {actual}")]
    CallIdMismatch { expected: String, actual: String },
    #[error("call answer peer does not match pending DM peer")]
    PeerMismatch,
    #[error("key derivation failed: {0}")]
    KeyDerivationFailed(String),
    #[error("call worker error: {0}")]
    CallWorker(String),
    /// Mobile-path orchestration error (LiveKit setup, gRPC create/end call,
    /// JSON decode of server responses). Kept as a free-form string because
    /// the mobile methods bridge multiple subsystems and their error surfaces
    /// are shaped by the platform SDK rather than our typed domain errors.
    #[error("mobile call: {0}")]
    Mobile(String),
    #[error("{0} timed out")]
    Timeout(&'static str),
}

/// Deadline for each server round trip the group-call key policy makes. That
/// policy runs off LiveKit room events and received keys; a server that never
/// answers must not hold it — or a departure rotation queued behind it — open.
pub(crate) const CALL_POLICY_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Await `fut` for at most [`CALL_POLICY_RPC_TIMEOUT`].
pub(crate) async fn with_policy_deadline<T, E>(
    what: &'static str,
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, CallSignalingError>
where
    CallSignalingError: From<E>,
{
    match tokio::time::timeout(CALL_POLICY_RPC_TIMEOUT, fut).await {
        Ok(result) => result.map_err(CallSignalingError::from),
        Err(_) => Err(CallSignalingError::Timeout(what)),
    }
}

/// Most senders whose keys are held for one call we are joining. Each must be
/// a channel member; the cap bounds what a large channel can make us hold.
#[derive(Clone)]
pub struct CallSignalingService {
    message_service: Arc<MessageService>,
    channel_service: Arc<ChannelService>,
    /// Pending outgoing DM call state (waiting for answer with peer's ephemeral key).
    pending_dm_call: Arc<Mutex<Option<PendingDmCall>>>,
    pub(crate) start_gate: Arc<tokio::sync::Mutex<()>>,
    /// Channel group calls we were rung for, by call id.
    pub(crate) group_call_invites: Arc<GroupCallInvites>,
    /// The call a task is waiting for (and requesting) a key in, so a burst
    /// of lags runs one wait per call, not one each.
    group_key_wait: Arc<Mutex<Option<String>>>,
    /// The last rotated key we sent: call, when, and to whom.
    last_key_sent: Arc<Mutex<Option<KeySent>>>,
    #[cfg(feature = "media")]
    start_groups: Arc<Mutex<std::collections::HashSet<String>>>,
    #[cfg(feature = "media")]
    inflight_start: Arc<Mutex<Option<crate::client::calls::CallStartAttempt>>>,
    #[cfg(feature = "media")]
    start_attempts:
        Arc<Mutex<std::collections::HashMap<String, crate::client::calls::CallStartAttempt>>>,
}

/// The setup gate serializes these owners; dropping one can never remove a
/// replacement's token. Session teardown can cancel it before a call id exists.
#[cfg(feature = "media")]
pub(crate) struct StartAttemptOwner(Arc<Mutex<Option<crate::client::calls::CallStartAttempt>>>);
#[cfg(feature = "media")]
impl Drop for StartAttemptOwner {
    fn drop(&mut self) {
        self.0.lock().take();
    }
}

/// One rotated key's send ([`CallSignalingService::recently_keyed`]).
#[derive(Clone)]
struct KeySent {
    call_id: String,
    at: std::time::Instant,
    to: Vec<UserId>,
}

/// Whether `peer` was sent a rotation for `call_id` within `window` of `now`.
fn sent_recently(
    last: Option<&KeySent>,
    call_id: &str,
    peer: &UserId,
    now: std::time::Instant,
    window: std::time::Duration,
) -> bool {
    last.is_some_and(|k| {
        k.call_id == call_id && now.saturating_duration_since(k.at) < window && k.to.contains(peer)
    })
}

/// Held by the one task waiting for a group-call key in a call
/// ([`CallSignalingService::begin_group_key_wait`]); releases on drop.
pub struct GroupKeyWaitGuard {
    slot: Arc<Mutex<Option<String>>>,
    call_id: String,
}

impl Drop for GroupKeyWaitGuard {
    fn drop(&mut self) {
        let mut slot = self.slot.lock();
        if slot.as_deref() == Some(self.call_id.as_str()) {
            *slot = None;
        }
    }
}

impl CallSignalingService {
    pub fn new(message_service: Arc<MessageService>, channel_service: Arc<ChannelService>) -> Self {
        Self {
            message_service,
            channel_service,
            pending_dm_call: Arc::new(Mutex::new(None)),
            group_call_invites: Arc::new(GroupCallInvites::default()),
            group_key_wait: Arc::new(Mutex::new(None)),
            last_key_sent: Arc::new(Mutex::new(None)),
            start_gate: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(feature = "media")]
            start_groups: Arc::new(Mutex::new(Default::default())),
            #[cfg(feature = "media")]
            inflight_start: Arc::new(Mutex::new(None)),
            #[cfg(feature = "media")]
            start_attempts: Arc::new(Mutex::new(Default::default())),
        }
    }
}

mod dm_messages;
mod dm_setup;
mod ending;
mod group;
mod invites;
mod start_attempts;

pub use invites::PendingGroupCallKey;
use invites::buffer_newest_per_sender;
pub(crate) use invites::{GroupCallInvite, GroupCallInvites};
