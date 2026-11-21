//! Call orchestration (DM / group) plus call history and lifecycle.
//!
//! Secret chats (anonymous DMs and groups) have no calls: LiveKit media would
//! leave Tor and expose the user's IP.

use super::{ChatterClient, ClientError};
#[cfg(feature = "media")]
use crate::services::call_worker::CallWorkerHandle;

#[cfg(feature = "media")]
impl ChatterClient {
    pub fn call_needs_shield_confirmation(&self) -> bool {
        self.anon_mode_enabled()
            || self.services.grpc_pool.transport_mode()
                == crate::network::grpc::client_pool::TransportMode::Onion
    }

    fn require_shield_call_confirmation(&self, confirmed: bool) -> Result<(), ClientError> {
        if self.call_needs_shield_confirmation() && !confirmed {
            return Err(ClientError::ShieldCallConfirmationRequired);
        }
        Ok(())
    }
}

mod group;
mod history;
mod incoming;
mod lifecycle;
mod media;
mod outgoing;

#[cfg(feature = "media")]
use group::{KEY_REQUEST_AFTER, spawn_group_key_wait};

#[cfg(feature = "media")]
pub(crate) use group::JoinRotationGate;
pub use history::{CallHistoryCursor, CallHistoryPage};
#[cfg(feature = "media")]
pub use incoming::AcceptedIncomingCall;
#[cfg(feature = "media")]
pub use media::{CallRoomSnapshot, room_roster_keeps_screen};
#[cfg(feature = "media")]
pub use crate::services::call_signaling::CallStartAttempt;
// Call-worker values the call façade hands out (orchestrator events, end
// reasons, the incoming-call presentation snapshot) and the timeouts the
// platform ring/notification code must agree with.
pub use crate::services::gateway::CallHangupReason;
#[cfg(feature = "media")]
pub use crate::services::call_worker::{
    CONNECTION_LOST, CallEncryptionVerification, CallEndReason, CallOrchestratorEvent,
    IncomingCallPresentationState, OrchestratedCallKind, RING_TIMEOUT,
};
pub use outgoing::{OutgoingCallStart, RingingPeer};
