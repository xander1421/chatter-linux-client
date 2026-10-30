//! Content builders for constructing Content proto messages.
//!
//! These functions create properly formatted Content proto messages
//! for various message types (text, typing, receipts, reactions, etc.).
//!
//! All builders are **pure functions** — no DB or network access.
//! This is the single source of truth for Content/DataMessage construction.

use rand::RngCore;

use crate::proto::content_v1::{
    AttachmentPointer, Content, DataMessage, Delete, DeleteForMeSync, EditMessage, PinMessage,
    Quote, QuotedAttachment, Reaction, ReceiptMessage, SavedMessageSync, TypingMessage, content,
    quote, receipt_message, typing_message,
};
use crate::util::time::TimestampNanos;

use super::CallHangupReason;

// =============================================================================
// NDSS '21 Padding
// =============================================================================

/// Group control messages (membership, leave, kick, ephemeral).
pub const PADDING_GROUP_CONTROL: usize = 160;

/// Standard messages, DM wrappers, ephemeral DM messages.
pub const PADDING_MESSAGE: usize = 256;

/// Profile updates, call signaling (larger payloads).
pub const PADDING_LARGE: usize = 512;

/// Generate random NDSS '21 padding.
///
/// Padding prevents message-length traffic analysis by adding random bytes
/// to each encrypted envelope. Use the `PADDING_*` constants for standard
/// bucket sizes.
pub fn generate_padding(size: usize) -> Vec<u8> {
    if size == 0 {
        return vec![];
    }
    let mut padding = vec![0u8; size];
    rand::rng().fill_bytes(&mut padding);
    padding
}

/// Create a Content proto for a text message.
/// `sent_at` should be the SAME timestamp used for Message.sent_at (nanoseconds).
pub fn build_text_content(body: &str, sent_at: u64) -> Content {
    Content {
        timestamp: sent_at,
        content: Some(content::Content::DataMessage(DataMessage {
            body: body.to_string(),
            timestamp: sent_at,
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Create a Content proto for a message with text and/or attachments.
/// `sent_at` should be the SAME timestamp used for Message.sent_at (nanoseconds).
pub fn build_data_message_content(
    body: &str,
    attachments: Vec<AttachmentPointer>,
    sent_at: u64,
) -> Content {
    tracing::debug!(
        sent_at = sent_at,
        body_len = body.len(),
        attachment_count = attachments.len(),
        "build_data_message_content: creating Content proto"
    );
    Content {
        timestamp: sent_at,
        content: Some(content::Content::DataMessage(DataMessage {
            body: body.to_string(),
            timestamp: sent_at,
            attachments,
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Create a Content proto for a reply message with Quote.
/// Uses Signal-style message identification via sender ACI + timestamp.
pub fn build_reply_content(
    body: &str,
    attachments: Vec<AttachmentPointer>,
    quote_author_id: &str,
    quote_timestamp: u64,
    quote_text: &str,
    quote_attachments: Vec<QuotedAttachment>,
    sent_at: u64,
) -> Content {
    tracing::debug!(
        sent_at = sent_at,
        body_len = body.len(),
        attachment_count = attachments.len(),
        quote_author = quote_author_id,
        quote_ts = quote_timestamp,
        quote_attachments = quote_attachments.len(),
        "build_reply_content: creating Content proto with Quote"
    );

    Content {
        timestamp: sent_at,
        content: Some(content::Content::DataMessage(DataMessage {
            body: body.to_string(),
            timestamp: sent_at,
            attachments,
            quote: Some(Quote {
                id: quote_timestamp, // Signal uses timestamp as ID
                text: quote_text.to_string(),
                attachments: quote_attachments,
                author_aci: quote_author_id.to_string(),
                r#type: quote::Type::Normal as i32,
                author_aci_binary: vec![], // We use string format
            }),
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Create a Content proto for a message with optional quote.
///
/// This is the general-purpose DataMessage builder used by anon DM and
/// anon group services. When `quote` is `None`, behaves like
/// `build_data_message_content`.
pub fn build_data_message_content_with_quote(
    body: &str,
    attachments: Vec<AttachmentPointer>,
    quote: Option<Quote>,
    sent_at: u64,
) -> Content {
    Content {
        timestamp: sent_at,
        content: Some(content::Content::DataMessage(DataMessage {
            body: body.to_string(),
            timestamp: sent_at,
            attachments,
            quote,
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Create a Content proto for typing indicator.
pub fn build_typing_content(started: bool) -> Content {
    build_typing_content_with_channel(started, None)
}

/// Create a Content proto for typing indicator with optional channel context.
pub fn build_typing_content_with_channel(started: bool, channel_id: Option<&str>) -> Content {
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::Typing(TypingMessage {
            action: if started {
                typing_message::Action::Started as i32
            } else {
                typing_message::Action::Stopped as i32
            },
            timestamp: now,
            channel_id: channel_id.unwrap_or_default().to_string(),
        })),
        ..Default::default()
    }
}

/// Create a Content proto for read receipt.
pub fn build_read_receipt_content(message_timestamps: Vec<u64>) -> Content {
    build_read_receipt_content_with_channel(message_timestamps, None)
}

/// Create a Content proto for read receipt with optional channel context.
pub fn build_read_receipt_content_with_channel(
    message_timestamps: Vec<u64>,
    channel_id: Option<&str>,
) -> Content {
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::Receipt(ReceiptMessage {
            r#type: receipt_message::Type::Read as i32,
            timestamps: message_timestamps,
            timestamp: now,
            channel_id: channel_id.unwrap_or_default().to_string(),
        })),
        ..Default::default()
    }
}

/// Create a Content proto for delivery receipt.
pub fn build_delivery_receipt_content(message_timestamps: Vec<u64>) -> Content {
    build_delivery_receipt_content_with_channel(message_timestamps, None)
}

/// Create a Content proto for delivery receipt with optional channel context.
pub fn build_delivery_receipt_content_with_channel(
    message_timestamps: Vec<u64>,
    channel_id: Option<&str>,
) -> Content {
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::Receipt(ReceiptMessage {
            r#type: receipt_message::Type::Delivery as i32,
            timestamps: message_timestamps,
            timestamp: now,
            channel_id: channel_id.unwrap_or_default().to_string(),
        })),
        ..Default::default()
    }
}

/// Create a Content proto for reaction.
pub fn build_reaction_content(
    emoji: &str,
    remove: bool,
    target_author_aci: &str,
    target_sent_timestamp: u64,
) -> Content {
    build_reaction_content_with_channel(
        emoji,
        remove,
        target_author_aci,
        target_sent_timestamp,
        None,
    )
}

/// Create a Content proto for reaction with optional channel context.
pub fn build_reaction_content_with_channel(
    emoji: &str,
    remove: bool,
    target_author_aci: &str,
    target_sent_timestamp: u64,
    channel_id: Option<&str>,
) -> Content {
    build_reaction_content_with_channel_at(
        emoji,
        remove,
        target_author_aci,
        target_sent_timestamp,
        channel_id,
        TimestampNanos::now().as_u64(),
    )
}

/// Build an immutable reaction action. Dispatch, local persistence and copies
/// must retain this allocated clock, including across delayed completion.
pub fn build_reaction_content_with_channel_at(
    emoji: &str,
    remove: bool,
    target_author_aci: &str,
    target_sent_timestamp: u64,
    channel_id: Option<&str>,
    action_ts: u64,
) -> Content {
    let now = action_ts;
    // Note: Reaction proto doesn't have channel_id field - we encode it in target_author_aci
    // as "channel:{channel_id}:{author_aci}" for channels, or just author_aci for DMs
    let encoded_author = if let Some(ch_id) = channel_id {
        format!("channel:{}:{}", ch_id, target_author_aci)
    } else {
        target_author_aci.to_string()
    };
    Content {
        timestamp: now,
        content: Some(content::Content::Reaction(Reaction {
            emoji: emoji.to_string(),
            remove,
            target_author_aci: encoded_author,
            target_sent_timestamp,
            timestamp: now,
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Create a Content proto for pin/unpin.
pub fn build_pin_content(
    target_author_aci: &str,
    target_sent_timestamp: u64,
    unpin: bool,
) -> Content {
    build_pin_content_with_channel(target_author_aci, target_sent_timestamp, unpin, None)
}

/// Create a Content proto for pin/unpin with optional channel context.
pub fn build_pin_content_with_channel(
    target_author_aci: &str,
    target_sent_timestamp: u64,
    unpin: bool,
    channel_id: Option<&str>,
) -> Content {
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::Pin(PinMessage {
            target_author_aci: target_author_aci.to_string(),
            target_sent_timestamp,
            unpin,
            timestamp: now,
            channel_id: channel_id.unwrap_or_default().to_string(),
        })),
        ..Default::default()
    }
}

/// Create a Content proto for message edit.
pub fn build_edit_content(target_sent_timestamp: u64, new_body: &str) -> Content {
    build_edit_content_with_channel(target_sent_timestamp, new_body, None)
}

/// Create a Content proto for message edit with optional channel context.
pub fn build_edit_content_with_channel(
    target_sent_timestamp: u64,
    new_body: &str,
    channel_id: Option<&str>,
) -> Content {
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::Edit(EditMessage {
            target_sent_timestamp,
            data_message: Some(DataMessage {
                body: new_body.to_string(),
                timestamp: now,
                channel_id: channel_id.unwrap_or_default().to_string(),
                ..Default::default()
            }),
        })),
        ..Default::default()
    }
}

/// Create a Content proto for delete-for-me sync.
pub fn build_delete_for_me_content(
    conversation_id: &str,
    target_author_aci: &str,
    target_sent_timestamp: u64,
) -> Content {
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::DeleteForMeSync(DeleteForMeSync {
            conversation_id: conversation_id.to_string(),
            target_author_aci: target_author_aci.to_string(),
            target_sent_timestamp,
            timestamp: now,
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Create a Content proto for saved message sync (Note to Self multi-device).
///
/// `sync` is the full snapshot of one Saved record (see the proto): creation,
/// edit and pin all send this. Signal Protocol device sessions provide the
/// encryption — no application-level encryption needed. The envelope clock
/// is the snapshot's newest part, so each change is a distinct transcript.
pub fn build_saved_message_sync_content(sync: SavedMessageSync) -> Content {
    Content {
        timestamp: sync.sent_at.max(sync.edited_at).max(sync.pin_changed_at),
        content: Some(content::Content::SavedMessageSync(sync)),
        ..Default::default()
    }
}

/// Delete-for-everyone, routed to the exact target author.
pub fn build_targeted_delete_content(
    target_sent_timestamp: u64,
    target_author_aci: Option<&str>,
    channel_id: Option<&str>,
) -> Content {
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::DataMessage(DataMessage {
            timestamp: now,
            channel_id: channel_id.unwrap_or_default().to_string(),
            delete: Some(Delete {
                target_sent_timestamp,
                target_author_aci: target_author_aci.map(str::to_owned),
                destination_user_id: None,
                target_call_id: None,
            }),
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Build a scoped DM delete from a stored record.
pub(crate) fn build_stored_dm_delete_content(
    target: &crate::domain::message::Message,
) -> Result<Content, crate::error::StorageError> {
    use crate::storage::repositories::message_modifier_repo::canonical_call_record_id;
    let call_id = canonical_call_record_id(target);
    if matches!(
        target.content,
        crate::domain::message::MessageContent::Call(_)
    ) && call_id.is_none()
    {
        return Err(crate::error::StorageError::InvalidData(
            "Noncanonical DM call record".into(),
        ));
    }
    let author = target.sender_id.ok_or_else(|| {
        crate::error::StorageError::InvalidData("Delete target has no author".into())
    })?;
    let mut content =
        build_targeted_delete_content(target.sent_at, Some(&author.to_string()), None);
    if let Some(content::Content::DataMessage(data)) = &mut content.content {
        data.delete.as_mut().unwrap().target_call_id = call_id.map(|id| id.to_string());
    }
    Ok(content)
}

// =============================================================================
// DM Call Key Exchange (Ephemeral keys for PFS)
// =============================================================================

/// Create a Content proto for call key offer (caller initiating DM call).
///
/// Sends caller's ephemeral X25519 public key. The callee will respond with
/// their ephemeral key in an answer message.
///
/// # Arguments
/// * `call_id` - Unique call identifier
/// * `ephemeral_public_key` - Caller's ephemeral X25519 public key (32 bytes)
/// * `call_type` - "Voice Call" or "Video Call"
/// * `channel_id` - DM channel ID for context
pub fn build_call_key_offer(
    call_id: &str,
    ephemeral_public_key: [u8; 32],
    call_type: &str,
) -> Content {
    use crate::proto::content_v1::CallKeyExchange;
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::CallKey(CallKeyExchange {
            call_id: call_id.to_string(),
            key_material: ephemeral_public_key.to_vec(),
            timestamp: now,
            is_offer: true,
            call_type: call_type.to_string(),
        })),
        ..Default::default()
    }
}

/// Create a Content proto for call key answer (callee responding to offer).
///
/// Sends callee's ephemeral X25519 public key in response to an offer.
/// Both parties can then derive the call key using ECDH + HKDF.
///
/// # Arguments
/// * `call_id` - Call ID from the offer
/// * `ephemeral_public_key` - Callee's ephemeral X25519 public key (32 bytes)
pub fn build_call_key_answer(call_id: &str, ephemeral_public_key: [u8; 32]) -> Content {
    use crate::proto::content_v1::CallKeyExchange;
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::CallKey(CallKeyExchange {
            call_id: call_id.to_string(),
            key_material: ephemeral_public_key.to_vec(),
            timestamp: now,
            is_offer: false,
            call_type: String::new(),
        })),
        ..Default::default()
    }
}

// =============================================================================
// Group Call Signaling (pairwise sealed sender)
// =============================================================================

/// Create a Content proto carrying one channel group-call signal for a single
/// member (client-side fanout: the server cannot correlate the copies — NDSS
/// '21). A ring carries no key; only a [`GroupCallSignal::Key`] does.
pub fn build_group_call_signal_content(
    call_id: &str,
    channel_id: &str,
    signal: crate::domain::call::GroupCallSignal,
) -> Content {
    use crate::domain::call::GroupCallSignal;
    use crate::proto::content_v1::{
        GroupCallKeyRequest, GroupCallKeyUpdate, GroupCallRing, group_call_signal,
    };
    let now = TimestampNanos::now().as_u64();
    let signal = match signal {
        GroupCallSignal::Ring => group_call_signal::Signal::Ring(GroupCallRing {}),
        GroupCallSignal::Key { key, epoch } => group_call_signal::Signal::Key(GroupCallKeyUpdate {
            key_material: key.to_vec(),
            key_epoch: epoch,
        }),
        GroupCallSignal::KeyRequest => {
            group_call_signal::Signal::KeyRequest(GroupCallKeyRequest {})
        }
    };
    Content {
        timestamp: now,
        content: Some(content::Content::GroupCallSignal(
            crate::proto::content_v1::GroupCallSignal {
                call_id: call_id.to_string(),
                channel_id: channel_id.to_string(),
                timestamp: now,
                signal: Some(signal),
            },
        )),
        ..Default::default()
    }
}

/// Create a Content proto for call hangup notification.
///
/// This is sent via sealed sender to notify the peer that the call has ended.
pub fn build_call_hangup_content(call_id: &str, reason: CallHangupReason) -> Content {
    use crate::proto::content_v1::{CallHangup, call_hangup};
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::CallHangup(CallHangup {
            call_id: call_id.to_string(),
            reason: match reason {
                CallHangupReason::Normal => call_hangup::Reason::Normal as i32,
                CallHangupReason::Declined => call_hangup::Reason::Declined as i32,
                CallHangupReason::Busy => call_hangup::Reason::Busy as i32,
                CallHangupReason::Timeout => call_hangup::Reason::Timeout as i32,
                CallHangupReason::NetworkError => call_hangup::Reason::NetworkError as i32,
                CallHangupReason::AcceptedElsewhere => {
                    call_hangup::Reason::AcceptedElsewhere as i32
                }
                CallHangupReason::DeclinedElsewhere => {
                    call_hangup::Reason::DeclinedElsewhere as i32
                }
            },
            timestamp: now,
            device_id: 0,
        })),
        ..Default::default()
    }
}

/// The copy of an outgoing 1:1 call message our OWN other devices get.
///
/// Signal's `CallMessage.Hangup` ACCEPTED / DECLINED with `deviceId`: when this
/// device answers (sends the key answer) or declines a call, our other devices
/// — which are ringing for the same call id — are told to stop. Every other
/// call message is for the peer's devices only (a synced offer would ring us
/// with a call from ourselves), so it gets `None`.
pub fn build_call_self_sync(content: &Content, our_device_id: u32) -> Option<Content> {
    use crate::proto::content_v1::{CallHangup, call_hangup};
    let (call_id, reason) = match &content.content {
        Some(content::Content::CallKey(key)) if !key.is_offer => {
            (&key.call_id, call_hangup::Reason::AcceptedElsewhere)
        }
        Some(content::Content::CallHangup(h))
            if h.reason == call_hangup::Reason::Declined as i32 =>
        {
            (&h.call_id, call_hangup::Reason::DeclinedElsewhere)
        }
        _ => return None,
    };
    Some(Content {
        timestamp: content.timestamp,
        content: Some(content::Content::CallHangup(CallHangup {
            call_id: call_id.clone(),
            reason: reason as i32,
            timestamp: content.timestamp,
            device_id: our_device_id,
        })),
        ..Default::default()
    })
}

#[cfg(test)]
mod call_self_sync_tests {
    use super::*;
    use crate::proto::content_v1::call_hangup::Reason;

    fn synced(c: &Content) -> Option<(String, i32, u32)> {
        build_call_self_sync(c, 6).map(|s| match s.content {
            Some(content::Content::CallHangup(h)) => (h.call_id, h.reason, h.device_id),
            other => panic!("expected CallHangup, got {other:?}"),
        })
    }

    /// Answering / declining tells our other devices; nothing else rings them.
    #[test]
    fn only_answer_and_decline_reach_our_other_devices() {
        let answer = build_call_key_answer("c1", [1u8; 32]);
        assert_eq!(
            synced(&answer),
            Some(("c1".into(), Reason::AcceptedElsewhere as i32, 6))
        );
        let decline = build_call_hangup_content("c2", CallHangupReason::Declined);
        assert_eq!(
            synced(&decline),
            Some(("c2".into(), Reason::DeclinedElsewhere as i32, 6))
        );

        assert_eq!(synced(&build_call_key_offer("c3", [1u8; 32], "")), None);
        assert_eq!(
            synced(&build_call_hangup_content("c4", CallHangupReason::Normal)),
            None
        );
        assert_eq!(
            synced(&build_call_hangup_content("c5", CallHangupReason::Busy)),
            None
        );
    }
}
