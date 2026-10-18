//! Call orchestration (DM / group) plus call history and lifecycle.
//!
//! Secret chats (anonymous DMs and groups) have no calls: LiveKit media would
//! leave Tor and expose the user's IP.

use super::{ChatterClient, ClientError};
#[cfg(feature = "media")]
use crate::services::call_worker::CallWorkerHandle;

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
pub use outgoing::CallStartAttempt;
#[cfg(feature = "media")]
pub(crate) use outgoing::START_ATTEMPT;
pub(crate) use outgoing::start_cancelled;
pub use outgoing::{OutgoingCallStart, RingingPeer};
