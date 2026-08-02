//! One acknowledgement policy, shared by every inbox loop.
//!
//! Acking an envelope makes the SERVER DELETE IT (`AcknowledgeMessages` is
//! documented in the proto as "Acknowledge receipt of messages (deletes from
//! queue)"). So "do we ack?" is "do we destroy the only remaining copy?", and
//! four loops answering it four different ways is four different data-loss or
//! replay bugs. This module is the single answer.
//!
//! # The policy
//!
//! | outcome | ack? | marker? |
//! |---|---|---|
//! | decrypted | yes | no |
//! | permanently failed | yes, **after** the marker is durably stored | yes |
//! | transiently failed | **no** — let it redeliver | no |
//! | duplicate / self-echo / blocked | yes | no (nothing was lost) |
//!
//! Marker first, then ack, never the reverse. If the marker cannot be written
//! the failure is downgraded to [`AckDecision::Retain`]: a message we can
//! neither show nor explain must not also be deleted.
//!
//! # Why a permanent failure is acked at all
//!
//! Because not acking is its own bug. A dead ciphertext handed to us again
//! fails identically, and on the device inbox each replay also re-sent a
//! `DecryptionErrorMessage` — measured at 201 inbound reports and 12 outbound
//! in four minutes between two idle devices. `polling::anon_dm` still has the
//! other half of that: it never acks a failure, so a peer on an old envelope
//! format redelivers forever. Acking terminates it; the marker is what keeps
//! the termination from being silent.
//!
//! # How transient is decided
//!
//! Transient means *a later attempt on the same bytes can succeed*. That is a
//! statement about what the failure depended on, not about how bad it was:
//!
//! - **Storage and network errors are transient.** They describe OUR machine,
//!   not the ciphertext. Also load-bearing: a storage error is exactly the case
//!   where writing the marker would fail too, so acking would delete the
//!   message AND leave no trace.
//! - **Missing key material is transient.** `SessionNotFound`, `KeyNotFound`,
//!   `NoIdentity` — the SKDM, the bootstrap response, or our own identity may
//!   land on a later poll and then the same bytes decrypt.
//! - **Bad bytes are permanent.** A failed proto decode, a bad MAC, an
//!   unparseable sender id, an invalid certificate — the outcome is a pure
//!   function of the ciphertext, so redelivery reproduces it exactly.
//! - **A wedged session is permanent** for *this* message even though the
//!   session heals: `SessionDesync`'s ratchet state is already gone.
//! - **Unknown is transient.** See [`classify_crypto`] — the two directions of
//!   this mistake are not equally bad.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::domain::ids::{ChannelId, MessageId};
use crate::domain::message::{ConversationType, Message, MessageContent, SystemMessage};
use crate::domain::UserId;
use crate::error::{CryptoError, ServiceError, StorageError};
use crate::inbox::handlers::DomainEvent;
use crate::storage::MessageRepository;

/// What the loop must do with the server's copy of this envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckDecision {
    /// Drain it from the server queue.
    Ack,
    /// Leave it queued so it is delivered again.
    Retain,
}

impl AckDecision {
    /// True when the caller should add this message id to its ack batch.
    #[must_use]
    pub fn should_ack(self) -> bool {
        matches!(self, Self::Ack)
    }
}

/// Whether a later delivery of the same bytes could succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// This will fail the same way every time. Ack it, with a marker.
    Permanent,
    /// A later attempt can succeed. Do not ack.
    Transient,
}

/// Which of the two markers a permanent failure deserves.
///
/// There are exactly two because there are exactly two resolutions. A desync is
/// answered by the `DecryptionErrorMessage` we ship back — the peer archives its
/// session and a resend works, which is what [`SystemMessage::DeliveryIssue`]
/// tells the user to ask for. An identity change is not: no report is sent,
/// resending re-presents the same pinned-key mismatch and fails identically
/// forever, and only a human accepting the new key clears it. Routing an
/// identity change to `DeliveryIssue` would send the user round a loop that
/// cannot terminate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureMarker {
    /// Session desync, or any other dead ciphertext. Resend works.
    DeliveryIssue,
    /// The peer's pinned identity key changed. Resend does not work.
    ///
    /// `attributed` is [`crate::error::IdentityChangeSource::is_authenticated`]:
    /// false means the peer id was asserted by the server and proved by nobody,
    /// so the entry must not name them as fact.
    IdentityChanged { attributed: bool },
}

/// Where a marker goes, and who it names.
///
/// The conversation is supplied by the caller because the four loops read
/// different ones: a device-inbox or DM failure belongs in that peer's DM, a
/// group-mailbox failure in the group's channel, an anon-DM failure in the
/// anonymous conversation.
#[derive(Debug, Clone)]
pub struct MarkerTarget {
    /// Conversation the marker is inserted into.
    pub conversation_id: ChannelId,
    /// Conversation type, so the UI's read-back query finds it.
    pub conversation_type: ConversationType,
    /// Peer the entry names. `None` when the failure destroyed the sealed
    /// sender envelope itself and nobody was authenticated — the entry still
    /// gets written, it just cannot say who.
    pub peer_id: Option<UserId>,
    /// The envelope's own server timestamp. Sorts the marker where the lost
    /// message would have been, and lets the UNIQUE index on
    /// (conversation_type, conversation_id, sender_id, sent_at) collapse the
    /// repeats a failed `acknowledge` produces.
    pub envelope_ts_ns: u64,
}

/// Classify a crypto failure.
///
/// The default arm is `Transient`, and that is a deliberate asymmetry. Getting
/// it wrong in the permanent direction deletes mail with no way back. Getting it
/// wrong in the transient direction costs one extra decrypt attempt per poll on
/// a message that stays queued — visible in the logs as the same id retained
/// over and over, and recoverable by classifying it properly later. Only
/// failures whose permanence is *established* by the variant are acked.
#[must_use]
pub fn classify_crypto(e: &CryptoError) -> Durability {
    use CryptoError as C;
    match e {
        // ---- Permanent: the outcome is a function of the ciphertext ----
        // A changed pinned key. No session-layer action changes which key is
        // pinned, so every redelivery reproduces this.
        C::UntrustedIdentity { .. } => Durability::Permanent,
        // The ratchet state this inner message needed is already gone. The
        // report heals the session for the NEXT message, never this one.
        C::SessionDesync { .. } => Durability::Permanent,
        // Bad or unreadable bytes. Deterministic in the envelope.
        C::DecryptionFailed(_)
        | C::InvalidSealedSenderMessage(_)
        | C::InvalidSenderCertificate(_)
        | C::UnknownServerCertificate(_)
        | C::InvalidSignature
        | C::InvalidKeyFormat
        | C::InvalidKeyLength { .. }
        | C::SealedSenderSelfSend => Durability::Permanent,

        // ---- Transient: the outcome depended on our local state ----
        // Our disk, not their ciphertext — and the case where the marker write
        // would fail too.
        C::Storage(_) => Durability::Transient,
        // Key material that a later poll can supply: the SKDM, the bootstrap
        // response, our own identity after registration finishes.
        C::SessionNotFound(_)
        | C::SessionUnusable { .. }
        | C::SessionExpired(_)
        | C::KeyNotFound(_)
        | C::NoIdentity => Durability::Transient,

        // `Protocol` is the catch-all every libsignal error is stuffed into
        // (`crypto/signal/*.rs` maps ~40 distinct causes onto it with
        // `e.to_string()`), so it covers both a corrupt ciphertext and a
        // failed store read. It cannot be called permanent without deleting
        // the store-read half.
        C::Protocol(_) => Durability::Transient,

        // Everything else — key derivation, attachment crypto, blind
        // signatures, validation. Not established as permanent here.
        _ => Durability::Transient,
    }
}

/// Classify a service-layer failure by unwrapping to the crypto cause.
#[must_use]
pub fn classify_service(e: &ServiceError) -> Durability {
    match e {
        ServiceError::Crypto(c) => classify_crypto(c),
        // Ours, not theirs.
        ServiceError::Storage(_) | ServiceError::Network(_) => Durability::Transient,
        // `Validation` is raised on malformed input, which is deterministic in
        // the bytes; `Internal` and `NotLoggedIn` are local state.
        ServiceError::Validation(_) => Durability::Permanent,
        ServiceError::Internal(_) | ServiceError::NotLoggedIn => Durability::Transient,
    }
}

/// Which marker a permanent crypto failure deserves.
#[must_use]
pub fn marker_for_crypto(e: &CryptoError) -> FailureMarker {
    match e {
        CryptoError::UntrustedIdentity { attribution, .. } => FailureMarker::IdentityChanged {
            attributed: attribution.is_authenticated(),
        },
        _ => FailureMarker::DeliveryIssue,
    }
}

/// Which marker a permanent service failure deserves.
#[must_use]
pub fn marker_for_service(e: &ServiceError) -> FailureMarker {
    match e {
        ServiceError::Crypto(c) => marker_for_crypto(c),
        _ => FailureMarker::DeliveryIssue,
    }
}

/// Build the system entry that stands in for the message we lost.
#[must_use]
pub fn marker_message(target: MarkerTarget, marker: FailureMarker) -> Message {
    let system = match marker {
        FailureMarker::DeliveryIssue => SystemMessage::DeliveryIssue,
        FailureMarker::IdentityChanged { attributed } => {
            SystemMessage::IdentityChanged { attributed }
        }
    };
    let now = chrono::Utc::now();
    Message {
        id: MessageId::new(),
        conversation_id: target.conversation_id,
        conversation_type: target.conversation_type,
        // Drives the UI's name lookup — system entries render as
        // "{sender_name} {description}".
        sender_id: target.peer_id,
        device_id: None,
        content: MessageContent::System(system),
        encrypted_content: None,
        thread_id: None,
        reply_to: None,
        reply_to_author_id: None,
        reply_to_text: None,
        reply_to_thumbnail: None,
        reply_to_thumbnail_mime: None,
        attachments: Vec::new(),
        is_pinned: false,
        is_edited: false,
        is_deleted: false,
        is_read: false,
        sent_at: target.envelope_ts_ns,
        created_at: now,
        updated_at: now,
        edited_at: None,
        reaction_counts: Vec::new(),
        current_user_reactions: Vec::new(),
        mentioned_integrations: Vec::new(),
        targets_ai_bot: false,
        delivery_status: String::new(),
    }
}

/// Write the marker, then decide. **Never the reverse.**
///
/// Returns [`AckDecision::Ack`] only once the marker is durably in the database
/// (or was already there, from a replay of the same envelope). A write failure
/// returns [`AckDecision::Retain`] — the server's copy is then the only record
/// that anything arrived, and deleting it would erase the message and the
/// evidence together.
pub fn record_marker_then_ack(
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    target: MarkerTarget,
    marker: FailureMarker,
) -> AckDecision {
    let conversation_id = target.conversation_id;
    let msg = marker_message(target, marker);
    match message_repo.insert_or_ignore(&msg) {
        // News. Announce it so an open conversation shows the gap without
        // waiting for a reload.
        Ok(true) => {
            if let Some(tx) = domain_tx {
                let _ = tx.send(DomainEvent::MessageReceived { message: msg });
            }
            AckDecision::Ack
        }
        // Already there: a replayed envelope after a failed `acknowledge`.
        // Saying it twice would spam the conversation.
        Ok(false) => AckDecision::Ack,
        Err(error) => {
            tracing::error!(
                %conversation_id,
                ?marker,
                %error,
                "[INBOX] could not record the failure marker — NOT acking, because \
                 deleting the server's copy now would lose the message and the trace \
                 of it at once"
            );
            AckDecision::Retain
        }
    }
}

/// The whole policy for a failed envelope, in one call.
///
/// Transient failures retain. Permanent failures write their marker and only
/// then ack.
pub fn decide_after_failure(
    err: &ServiceError,
    target: MarkerTarget,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) -> AckDecision {
    match classify_service(err) {
        Durability::Transient => {
            tracing::warn!(
                conversation_id = %target.conversation_id,
                error = %err,
                "[INBOX] transient failure — leaving the envelope queued for redelivery"
            );
            AckDecision::Retain
        }
        Durability::Permanent => {
            let marker = marker_for_service(err);
            tracing::warn!(
                conversation_id = %target.conversation_id,
                error = %err,
                ?marker,
                "[INBOX] permanent failure — recording a marker, then draining the envelope"
            );
            record_marker_then_ack(message_repo, domain_tx, target.clone(), marker)
        }
    }
}

/// A permanent failure that carries no [`ServiceError`] — a proto that will not
/// decode, a sender id that will not parse. Deterministic in the bytes, so the
/// same envelope fails the same way forever.
pub fn decide_after_malformed(
    reason: &str,
    target: MarkerTarget,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
) -> AckDecision {
    tracing::warn!(
        conversation_id = %target.conversation_id,
        reason,
        "[INBOX] malformed envelope — deterministic in the bytes, so recording a \
         marker and draining it rather than redelivering it forever"
    );
    record_marker_then_ack(
        message_repo,
        domain_tx,
        target.clone(),
        FailureMarker::DeliveryIssue,
    )
}

/// Convenience: the marker target for a 1:1 server DM with `peer`.
#[must_use]
pub fn dm_target(peer: UserId, envelope_ts_ns: u64) -> MarkerTarget {
    MarkerTarget {
        conversation_id: peer.into(),
        conversation_type: ConversationType::ServerDm,
        peer_id: Some(peer),
        envelope_ts_ns,
    }
}

/// Storage errors are the one class the marker path itself can hit; keep the
/// mapping visible so a future variant is classified rather than defaulted.
#[must_use]
pub fn classify_storage(_e: &StorageError) -> Durability {
    Durability::Transient
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::IdentityChangeSource;

    #[test]
    fn a_changed_identity_key_is_permanent_and_gets_its_own_marker() {
        let e = CryptoError::UntrustedIdentity {
            peer_id: UserId::new().to_string(),
            device_id: 1,
            new_identity_key: None,
            attribution: IdentityChangeSource::SenderCertificate,
        };
        assert_eq!(classify_crypto(&e), Durability::Permanent);
        assert_eq!(
            marker_for_crypto(&e),
            FailureMarker::IdentityChanged { attributed: true },
            "a resend re-presents the same key — routing this to DeliveryIssue \
             would advise a resend that cannot ever work"
        );
    }

    #[test]
    fn an_unattributed_identity_change_keeps_its_hedge_through_the_policy() {
        let e = CryptoError::UntrustedIdentity {
            peer_id: UserId::new().to_string(),
            device_id: 1,
            new_identity_key: None,
            attribution: IdentityChangeSource::ServerClaimed,
        };
        assert_eq!(
            marker_for_crypto(&e),
            FailureMarker::IdentityChanged { attributed: false }
        );
    }

    #[test]
    fn a_desync_is_permanent_for_this_message_and_gets_the_delivery_issue_marker() {
        let e = CryptoError::SessionDesync {
            sender_uuid: UserId::new().to_string(),
            sender_device_id: 1,
            msg_type: 2,
            ciphertext: vec![1, 2, 3],
            reason: "bad mac".into(),
        };
        assert_eq!(classify_crypto(&e), Durability::Permanent);
        assert_eq!(marker_for_crypto(&e), FailureMarker::DeliveryIssue);
    }

    #[test]
    fn storage_failures_are_transient_so_a_full_disk_cannot_delete_the_queue() {
        let e = CryptoError::Storage(StorageError::ConnectionPool("disk I/O error".into()));
        assert_eq!(
            classify_crypto(&e),
            Durability::Transient,
            "a storage error is also exactly when the marker write fails, so acking \
             would lose the message and the trace together"
        );
    }

    #[test]
    fn missing_key_material_is_transient_because_a_later_poll_can_supply_it() {
        for e in [
            CryptoError::SessionNotFound("a".into()),
            CryptoError::KeyNotFound("b".into()),
            CryptoError::NoIdentity,
        ] {
            assert_eq!(
                classify_crypto(&e),
                Durability::Transient,
                "{e} must redeliver — the SKDM or bootstrap response may not have \
                 landed yet"
            );
        }
    }

    #[test]
    fn dead_ciphertext_is_permanent_so_it_cannot_redeliver_forever() {
        for e in [
            CryptoError::DecryptionFailed("mac".into()),
            CryptoError::InvalidSealedSenderMessage("v2 envelope".into()),
            CryptoError::InvalidSenderCertificate("expired".into()),
            CryptoError::InvalidSignature,
        ] {
            assert_eq!(
                classify_crypto(&e),
                Durability::Permanent,
                "{e} is a pure function of the bytes; retaining it replays forever"
            );
        }
    }

    #[test]
    fn an_unrecognised_libsignal_error_redelivers_rather_than_being_deleted() {
        let e = CryptoError::Protocol("something new from libsignal".into());
        assert_eq!(
            classify_crypto(&e),
            Durability::Transient,
            "Protocol is the catch-all for ~40 distinct libsignal causes including \
             store reads; calling it permanent deletes mail we merely failed to \
             understand"
        );
    }
}
