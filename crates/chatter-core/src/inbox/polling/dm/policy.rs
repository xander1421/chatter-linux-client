use super::*;

/// Decide what to do with a DM envelope whose decrypt failed.
///
/// The single seam `poll_dm_mailboxes` calls for every failure, so the policy
/// is one place rather than an emergent property of where the ack list is
/// built. Bounded: without the budget a permanently-undecryptable envelope
/// stays on the server to be re-fetched and re-decrypted on every poll for the
/// life of the process.
pub(crate) fn dm_failure_decision(
    peer_id: UserId,
    envelope_ts_ns: u64,
    err: &crate::error::ServiceError,
    message_repo: &Arc<MessageRepository>,
    domain_tx: Option<&broadcast::Sender<DomainEvent>>,
    envelope_id: &str,
    budget: &ack_policy::RetryBudget,
) -> AckDecision {
    ack_policy::decide_after_failure_bounded(
        err,
        ack_policy::dm_target(peer_id, envelope_ts_ns),
        message_repo,
        domain_tx,
        envelope_id,
        budget,
        ack_policy::now_ns(),
    )
}

/// The recipient `(service id, device)` a libsignal address block names:
/// `service_id_fixed_width_binary ++ device_id`, the second half of the
/// 36-byte `SignalMessage.addresses` field (libsignal `protocol.rs`
/// `serialize_addresses`).
pub(super) fn local_recipient_address(
    local_uuid: &UserId,
    local_device_id: u32,
) -> Option<Vec<u8>> {
    let service_id =
        libsignal_core::ServiceId::parse_from_service_id_string(&local_uuid.to_string())?;
    let mut bytes = service_id.service_id_fixed_width_binary().to_vec();
    bytes.push(u8::try_from(local_device_id).ok()?);
    Some(bytes)
}

/// The recipient half of the address block a failed inner ciphertext carries,
/// or `None` when it carries none (a legacy Whisper message) or does not parse.
///
/// The block is inside the MAC'd body, and the MAC is one we cannot check (the
/// session is another device's), so this is an unauthenticated hint. It is
/// only ever used to decide that a message which ALREADY failed was never ours
/// — the mailbox layer already restricts who can put bytes here to the peer.
fn sealed_recipient(msg_type: u8, ciphertext: &[u8]) -> Option<Vec<u8>> {
    use libsignal_protocol::{CiphertextMessageType, PreKeySignalMessage, SignalMessage};

    #[derive(Clone, PartialEq, prost::Message)]
    struct WireAddresses {
        #[prost(bytes = "vec", optional, tag = "6")]
        addresses: Option<Vec<u8>>,
    }

    let signal = if msg_type == CiphertextMessageType::PreKey as u8 {
        PreKeySignalMessage::try_from(ciphertext)
            .ok()?
            .message()
            .clone()
    } else if msg_type == CiphertextMessageType::Whisper as u8 {
        SignalMessage::try_from(ciphertext).ok()?
    } else {
        return None;
    };
    // Serialized as version byte ++ protobuf ++ 8-byte MAC.
    let serialized = signal.serialized();
    let proto = serialized.get(1..serialized.len().checked_sub(8)?)?;
    let addresses = <WireAddresses as prost::Message>::decode(proto)
        .ok()?
        .addresses?;
    (addresses.len() == 36).then(|| addresses[18..].to_vec())
}

/// A mailbox envelope whose inner message was sealed for a SIBLING device of
/// ours, not this one.
///
/// The mailbox conversation is one-peer-device, but a sender can still put a
/// copy for each of our devices into it (the live send once did). libsignal
/// refuses those ("Address mismatch" / "Bad Mac!") and they surface as
/// `SessionDesync` — which is not a failure of a message addressed to us:
/// nothing was lost here, so there is no marker, no notice and no report.
pub(super) fn addressed_to_another_device(
    err: &crate::error::ServiceError,
    local_recipient: Option<&[u8]>,
) -> bool {
    let (
        Some(local),
        crate::error::ServiceError::Crypto(crate::error::CryptoError::SessionDesync {
            msg_type,
            ciphertext,
            ..
        }),
    ) = (local_recipient, err)
    else {
        return false;
    };
    sealed_recipient(*msg_type, ciphertext).is_some_and(|recipient| recipient != local)
}
