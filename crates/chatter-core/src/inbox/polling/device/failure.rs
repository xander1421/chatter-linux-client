use super::*;

/// Build the conversation entry that marks a DM we could not decrypt.
///
/// The inbox acks every polled envelope whatever the outcome, so an
/// undecryptable message is deleted from the server queue and never comes
/// back. The `DecryptionErrorMessage` we return heals the SESSION (the sender
/// archives it, so the next message decrypts) but does nothing for the message
/// that failed. Without this entry the user sees an unexplained gap.
///
/// Signal does the same thing on the receiving side: Signal-Android inserts a
/// "Delivery issue" conversation update on a bad decrypt.
///
/// `envelope_ts_ns` is the envelope's own server timestamp, used as `sent_at`
/// for two reasons: the marker sorts where the lost message would have been,
/// and the existing UNIQUE index on
/// (conversation_type, conversation_id, sender_id, sent_at) collapses repeats
/// when a failed `acknowledge` causes the same envelope to be replayed at the
/// next start.
#[must_use]
pub fn delivery_issue_message(peer_id: UserId, envelope_ts_ns: u64) -> Message {
    system_dm_marker(
        peer_id,
        envelope_ts_ns,
        crate::domain::message::SystemMessage::DeliveryIssue,
    )
}

/// Build the conversation entry that marks the message which surfaced a peer
/// identity change.
///
/// The SECOND marker, not a reuse of [`delivery_issue_message`], because the two
/// failures are not the same failure. A delivery issue is a desynced session:
/// the `DecryptionErrorMessage` we ship back heals it and a resend works, which
/// is what that marker tells the user to ask for. An identity change is a
/// changed pinned key: `classify_inner_failure`
/// (`crypto/signal/sealed_sender.rs`) deliberately refuses to call it a desync,
/// no report is sent, and a resend re-presents the same new key and fails
/// identically forever. Telling the user to ask for a resend here would send
/// them round a loop that cannot terminate.
///
/// `attributed` is [`crate::error::IdentityChangeSource::is_authenticated`] —
/// it decides whether the entry may state who this was, not how it is styled.
#[must_use]
pub fn identity_changed_message(peer_id: UserId, envelope_ts_ns: u64, attributed: bool) -> Message {
    system_dm_marker(
        peer_id,
        envelope_ts_ns,
        crate::domain::message::SystemMessage::IdentityChanged { attributed },
    )
}

/// Decide what to do with a device-inbox envelope whose processing failed.
///
/// `peer_id` is `Some` only when the failure authenticated a sender first. A
/// device-inbox failure that authenticated nobody has no conversation to land
/// in — the envelope is what failed. Fall back to a marker in the peer's DM
/// when we have one, and to a bare retain/ack when we do not: writing "someone
/// sent you something unreadable" into an arbitrary conversation would be worse
/// than the log line.
#[allow(clippy::too_many_arguments)]
pub(crate) fn device_failure_decision(
    peer_id: Option<UserId>,
    envelope_ts_ns: u64,
    err: &ServiceError,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    envelope_id: &str,
    budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::{self, AckDecision};
    let Some(peer) = peer_id else {
        return match ack_policy::classify_service(err) {
            ack_policy::Durability::Transient => ack_policy::retain_within_budget(
                // No conversation to mark, so the target only carries the
                // budget's timestamp. If the budget runs out the marker write
                // is attempted against this synthetic conversation and fails
                // to be useful — so give up quietly instead.
                ack_policy::dm_target(UserId::from_uuid(uuid::Uuid::nil()), envelope_ts_ns),
                message_repo,
                domain_tx,
                envelope_id,
                budget,
                ack_policy::now_ns(),
                &err.to_string(),
            ),
            ack_policy::Durability::Permanent => {
                tracing::warn!(
                    error = %err,
                    "[INBOX] permanent failure with no authenticated sender — draining \
                     the envelope with no marker, because there is no conversation to \
                     put one in"
                );
                budget.forget(envelope_id);
                AckDecision::Ack
            }
        };
    };
    ack_policy::decide_after_failure_bounded(
        err,
        ack_policy::dm_target(peer, envelope_ts_ns),
        message_repo,
        domain_tx,
        envelope_id,
        budget,
        ack_policy::now_ns(),
    )
}

/// The conversation a channel/group failure marker belongs in.
///
/// A sender-key envelope is a CHANNEL message, so its marker goes in the
/// channel — not in a DM with the sender, which is a different conversation the
/// user would read as "I lost a private message".
fn channel_marker_target(
    channel_repo: &Arc<ChannelRepository>,
    channel_id: ChannelId,
    peer_id: Option<UserId>,
    envelope_ts_ns: u64,
) -> crate::inbox::ack_policy::MarkerTarget {
    let conversation_type = crate::services::message::conversion::conversation_type_for_channel(
        channel_repo,
        &channel_id,
    );
    crate::inbox::ack_policy::MarkerTarget {
        conversation_id: channel_id,
        conversation_type,
        peer_id,
        envelope_ts_ns,
    }
}

/// Where a pairwise decrypt failure is shown. A channel control fanned out
/// pairwise names its channel in the sealed-sender group id, so it is marked
/// there, as Signal-Desktop places a failure carrying a `groupId` in that
/// group. Only a channel we hold, and that names the sender a member, may be
/// marked: anything else (no group id, an unknown channel, a non-member) is
/// the DM with the sender.
pub(in crate::inbox::polling) fn desync_marker_target(
    channel_repo: &Arc<ChannelRepository>,
    channel_id: Option<ChannelId>,
    sender_id: UserId,
    envelope_ts_ns: u64,
) -> crate::inbox::ack_policy::MarkerTarget {
    match channel_id {
        Some(channel_id)
            if matches!(
                channel_repo.get_membership(&channel_id, &sender_id),
                Ok(Some(_))
            ) =>
        {
            channel_marker_target(channel_repo, channel_id, Some(sender_id), envelope_ts_ns)
        }
        _ => crate::inbox::ack_policy::dm_target(sender_id, envelope_ts_ns),
    }
}

/// Decide what to do with a sender-key (channel) envelope whose routing failed.
///
/// `channel_id` is `Some` only when the failure got far enough to name one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sender_key_failure_decision(
    channel_id: Option<ChannelId>,
    sender_id: UserId,
    envelope_ts_ns: u64,
    err: &ServiceError,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    envelope_id: &str,
    budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::{self, AckDecision};
    // Without a channel there is nowhere honest to put the marker: the sender's
    // DM is a different conversation, and "you lost a private message" is not
    // what happened. Classify only.
    let Some(channel_id) = channel_id else {
        return match ack_policy::classify_service(err) {
            ack_policy::Durability::Transient => ack_policy::retain_within_budget(
                ack_policy::dm_target(sender_id, envelope_ts_ns),
                message_repo,
                domain_tx,
                envelope_id,
                budget,
                ack_policy::now_ns(),
                &err.to_string(),
            ),
            ack_policy::Durability::Permanent => {
                tracing::warn!(
                    sender_id = %sender_id,
                    error = %err,
                    "[INBOX_SK] permanent failure on an envelope whose channel could \
                     not be recovered — draining it with no marker, because the only \
                     conversation we could write to is the wrong one"
                );
                budget.forget(envelope_id);
                AckDecision::Ack
            }
        };
    };
    ack_policy::decide_after_failure_bounded(
        err,
        channel_marker_target(channel_repo, channel_id, Some(sender_id), envelope_ts_ns),
        message_repo,
        domain_tx,
        envelope_id,
        budget,
        ack_policy::now_ns(),
    )
}

/// Decide what to do when a channel message arrives before the sender key that
/// opens it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sender_key_missing_decision(
    channel_id: ChannelId,
    sender_id: UserId,
    envelope_ts_ns: u64,
    message_repo: &Arc<MessageRepository>,
    channel_repo: &Arc<ChannelRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    envelope_id: &str,
    budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy;
    // The policy names this case itself: "the SKDM … may land on a later poll
    // and then the same bytes decrypt". The path emitted `SenderKeyNeeded`,
    // logged "will succeed on retry after SKDM arrives" — and then acked, so
    // the server deleted the message before the key it was waiting for could
    // arrive. Nothing buffers the ciphertext on this path either;
    // `defer_pending_group_decrypt` is only reached from the channel-history
    // path in `client/channels.rs`.
    ack_policy::retain_within_budget(
        channel_marker_target(channel_repo, channel_id, Some(sender_id), envelope_ts_ns),
        message_repo,
        domain_tx,
        envelope_id,
        budget,
        ack_policy::now_ns(),
        "waiting on the sender key that opens this channel message",
    )
}

/// Why an ECIES init envelope could not be opened.
#[derive(Debug)]
pub(crate) enum EciesInitFailure<'a> {
    /// We could not read our own identity key — our state, not their bytes.
    IdentityKeyUnavailable(&'a ServiceError),
    /// The envelope, the HPKE open, or the inner init message would not parse.
    Malformed(&'a str),
}

/// Decide what to do with an ECIES init envelope that could not be opened.
///
/// There is no conversation yet — an ECIES init IS the first contact — so no
/// marker can be written either way; the decision is purely whether to keep the
/// envelope for a later attempt.
pub(crate) fn ecies_init_decision(
    failure: &EciesInitFailure<'_>,
) -> crate::inbox::ack_policy::AckDecision {
    use crate::inbox::ack_policy::{AckDecision, Durability, classify_service};
    match failure {
        // OUR identity key. A storage error or a registration that has not
        // finished writing it are both answered by a later poll; the stranger's
        // contact request is intact and will open then.
        EciesInitFailure::IdentityKeyUnavailable(e) => match classify_service(e) {
            Durability::Transient => AckDecision::Retain,
            Durability::Permanent => AckDecision::Ack,
        },
        // The envelope, the HPKE open against a key we did read, or the inner
        // init message. All pure functions of the bytes, so retaining replays
        // forever — and there is no conversation yet to mark, because an ECIES
        // init IS the first contact.
        EciesInitFailure::Malformed(reason) => {
            tracing::warn!(
                reason,
                "[INBOX_ECIES] malformed private-contact request — draining it; \
                 there is no conversation yet in which to record the loss"
            );
            AckDecision::Ack
        }
    }
}

/// Decide what to do with an envelope the gateway refused because its
/// authenticated sender is blocked.
///
/// Split out from the match arms so both device-inbox paths — sealed sender and
/// identified sender — answer identically, and so a test can pin the two
/// properties that matter without standing up a gateway: the envelope IS
/// drained, and NOTHING is written into the conversation.
///
/// Note what this function does not take: no `event_tx`, no `stats`, no
/// `message_repo` to write to. There is no way for it to surface the message,
/// which is the point.
pub(crate) fn blocked_sender_decision(
    sender_id: UserId,
    envelope_id: &str,
    budget: &crate::inbox::ack_policy::RetryBudget,
) -> crate::inbox::ack_policy::AckDecision {
    tracing::info!(
        sender_id = %sender_id,
        envelope_id,
        "[INBOX] message from a blocked sender — dropped after decryption"
    );
    crate::inbox::ack_policy::drop_and_ack("blocked sender", envelope_id, budget)
}

/// Shared shape for the system entries the device inbox writes into a DM.
///
/// Delegates to the one builder in [`crate::inbox::ack_policy`] so the four
/// loops cannot drift into writing four differently-shaped markers.
fn system_dm_marker(
    peer_id: UserId,
    envelope_ts_ns: u64,
    system: crate::domain::message::SystemMessage,
) -> Message {
    use crate::inbox::ack_policy::{self, FailureMarker};
    let marker = match system {
        crate::domain::message::SystemMessage::IdentityChanged { attributed } => {
            FailureMarker::IdentityChanged { attributed }
        }
        _ => FailureMarker::DeliveryIssue,
    };
    ack_policy::marker_message(ack_policy::dm_target(peer_id, envelope_ts_ns), marker)
}
