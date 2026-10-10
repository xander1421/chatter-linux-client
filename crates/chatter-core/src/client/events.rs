//! Typed event streams exposed by [`ChatterClient`].
//!
//! Each type in this module is a narrow, UI-friendly projection of
//! [`DomainEvent`](crate::events::DomainEvent) — consumers subscribe to a
//! dedicated broadcast channel per family, so the UI never sees the full
//! enum and cannot accidentally depend on unrelated variants.
//!
//! The matching `subscribe_*` helpers on [`ChatterClient`](super::ChatterClient)
//! spawn a small filter task (once per subscription) that reads the underlying
//! [`EventBus`](crate::events::EventBus) and fans only the relevant variants
//! out as the typed payload.
//!
//! Keeping these adapters here (rather than in the UI crate) preserves the
//! rule that `chatter-ui` must never import `DomainEvent` / `EventBus` /
//! `subscribe_domain()` directly.
//!
//! # Wiring
//!
//! ```text
//!                                 +---------------------------+
//!                                 | EventBus (raw DomainEvent)|
//!                                 +------------+--------------+
//!                                              |
//!                 +----------------+----------+----------+------------------+
//!                 |                |                     |                  |
//!     subscribe_anon_dm_requests   |   subscribe_call_key_events   subscribe_remote_call_hangups
//!           |                      |                     |                  |
//!           v                      v                     v                  v
//!   AnonDmRequestEvent   AnonGroupInviteEvent      CallKeyEvent       RemoteCallHangup
//! ```

use crate::domain::anon_dm::AnonDmId;
use crate::domain::anon_group::AnonGroupId;
use crate::domain::ids::UserId;

/// Typed payloads for anonymous DM request / acceptance / init events.
///
/// Produced by [`ChatterClient::subscribe_anon_dm_requests`](super::ChatterClient::subscribe_anon_dm_requests).
#[derive(Debug, Clone)]
pub enum AnonDmRequestEvent {
    /// Incoming request from a peer (client-side anon DM flow).
    Received {
        conversation_id: AnonDmId,
        sender_id: UserId,
        sender_mailbox_pk: [u8; 32],
        intro_message: Option<String>,
        profile_key: Option<[u8; 32]>,
        sender_display_name: Option<String>,
        sender_avatar: Option<Vec<u8>>,
    },
    /// A previously-sent outgoing request was accepted by the peer.
    Accepted {
        conversation_id: AnonDmId,
        peer_id: UserId,
        peer_mailbox_pk: [u8; 32],
        profile_key: Option<[u8; 32]>,
        peer_display_name: String,
        peer_avatar: Option<Vec<u8>>,
    },
    /// NDSS'21 ECIES init message received (private friend contact).
    Init {
        conversation_id: AnonDmId,
        sender_id: UserId,
        sender_display_name: String,
        sender_avatar: Option<Vec<u8>>,
        peer_mailbox_pk: [u8; 32],
        peer_identity_key: [u8; 33],
        peer_delivery_token: [u8; 12],
        intro_message: String,
        server_timestamp: u64,
    },
}

/// Typed payload for an incoming anon-group invite.
///
/// Produced by [`ChatterClient::subscribe_anon_group_invites`](super::ChatterClient::subscribe_anon_group_invites).
#[derive(Debug, Clone)]
pub struct AnonGroupInviteEvent {
    pub group_id: AnonGroupId,
    pub group_name: String,
    pub inviter_id: UserId,
    /// The inviter's device the invite came from (pass back on accept).
    pub inviter_device_id: u32,
    pub inviter_mailbox_pk: [u8; 32],
    pub profile_key: Option<[u8; 32]>,
    pub inviter_display_name: Option<String>,
    pub inviter_avatar: Option<Vec<u8>>,
}

/// Typed payloads for call-key exchange events (answer + group key).
///
/// Produced by [`ChatterClient::subscribe_call_key_events`](super::ChatterClient::subscribe_call_key_events).
#[derive(Debug, Clone)]
pub enum CallKeyEvent {
    /// Callee's ephemeral public key arrived — completes PFS exchange.
    AnswerReceived {
        call_id: String,
        peer_id: UserId,
        ephemeral_public_key: [u8; 32],
    },
}

/// Typed payload for a remote call hangup.
///
/// Produced by [`ChatterClient::subscribe_remote_call_hangups`](super::ChatterClient::subscribe_remote_call_hangups).
#[derive(Debug, Clone)]
pub struct RemoteCallHangup {
    pub call_id: String,
    pub peer_id: UserId,
    pub reason: crate::services::gateway::CallHangupReason,
}
