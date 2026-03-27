//! Content builders for constructing Content proto messages.
//!
//! These functions create properly formatted Content proto messages
//! for various message types (text, typing, receipts, reactions, etc.).
//!
//! All builders are **pure functions** — no DB or network access.
//! This is the single source of truth for Content/DataMessage construction.

use rand::RngCore;

use crate::proto::content_v1::{
    AttachmentPointer, Content, DataMessage, Delete, DeleteForMeSync, EditMessage,
    PinMessage, Reaction, ReceiptMessage, SavedMessageSync, TypingMessage, Quote,
    content, typing_message, receipt_message, quote,
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
pub fn build_data_message_content(body: &str, attachments: Vec<AttachmentPointer>, sent_at: u64) -> Content {
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
    sent_at: u64,
) -> Content {
    tracing::debug!(
        sent_at = sent_at,
        body_len = body.len(),
        attachment_count = attachments.len(),
        quote_author = quote_author_id,
        quote_ts = quote_timestamp,
        "build_reply_content: creating Content proto with Quote"
    );
    
    Content {
        timestamp: sent_at,
        content: Some(content::Content::DataMessage(DataMessage {
            body: body.to_string(),
            timestamp: sent_at,
            attachments,
            quote: Some(Quote {
                id: quote_timestamp,  // Signal uses timestamp as ID
                text: quote_text.to_string(),
                attachments: vec![],  // TODO: Add quoted attachment support if needed
                author_aci: quote_author_id.to_string(),
                r#type: quote::Type::Normal as i32,
                author_aci_binary: vec![],  // We use string format
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

/// Create a Content proto for a channel message with channel_id.
///
/// Used for group fanout delivery where the channel_id must be embedded
/// in the DataMessage so recipients can route the decrypted content.
pub fn build_data_message_content_with_channel(
    body: &str,
    attachments: Vec<AttachmentPointer>,
    channel_id: &str,
    sent_at: u64,
) -> Content {
    Content {
        timestamp: sent_at,
        content: Some(content::Content::DataMessage(DataMessage {
            body: body.to_string(),
            timestamp: sent_at,
            channel_id: channel_id.to_string(),
            attachments,
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
            action: if started { typing_message::Action::Started as i32 } else { typing_message::Action::Stopped as i32 },
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
pub fn build_read_receipt_content_with_channel(message_timestamps: Vec<u64>, channel_id: Option<&str>) -> Content {
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
pub fn build_delivery_receipt_content_with_channel(message_timestamps: Vec<u64>, channel_id: Option<&str>) -> Content {
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
    build_reaction_content_with_channel(emoji, remove, target_author_aci, target_sent_timestamp, None)
}

/// Create a Content proto for reaction with optional channel context.
pub fn build_reaction_content_with_channel(
    emoji: &str,
    remove: bool,
    target_author_aci: &str,
    target_sent_timestamp: u64,
    channel_id: Option<&str>,
) -> Content {
    let now = TimestampNanos::now().as_u64();
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
        })),
        ..Default::default()
    }
}

/// Create a Content proto for saved message sync (Note to Self multi-device).
///
/// Follows Signal's approach: the message content is sent as a sync message
/// to the user's own devices. Signal Protocol device sessions provide encryption —
/// no application-level encryption needed.
pub fn build_saved_message_sync_content(
    message_id: &str,
    text: &str,
    sent_at: u64,
    attachment_ids: Vec<String>,
) -> Content {
    Content {
        timestamp: sent_at,
        content: Some(content::Content::SavedMessageSync(SavedMessageSync {
            text: text.to_string(),
            message_id: message_id.to_string(),
            sent_at,
            attachment_ids,
        })),
        ..Default::default()
    }
}

/// Create a Content proto for delete-for-everyone.
/// This sends a DataMessage with a Delete field to notify the peer.
pub fn build_delete_for_everyone_content(target_sent_timestamp: u64) -> Content {
    build_delete_for_everyone_content_with_channel(target_sent_timestamp, None)
}

/// Create a Content proto for delete-for-everyone with optional channel context.
pub fn build_delete_for_everyone_content_with_channel(
    target_sent_timestamp: u64,
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
            }),
            ..Default::default()
        })),
        ..Default::default()
    }
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
    channel_id: &str,
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
            channel_id: channel_id.to_string(),
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
pub fn build_call_key_answer(
    call_id: &str,
    ephemeral_public_key: [u8; 32],
) -> Content {
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
            channel_id: String::new(),
        })),
        ..Default::default()
    }
}

// =============================================================================
// Group Call Key Distribution (Pairwise sealed sender)
// =============================================================================

/// Create a Content proto for group call key distribution.
///
/// Distributes the symmetric call key to a single group member via pairwise
/// sealed sender. Must be called once for each member (client-side fanout).
///
/// Per NDSS '21: Server cannot correlate these independent messages to a group.
///
/// # Arguments
/// * `call_id` - Unique call identifier
/// * `key` - AES-256 encryption key (32 bytes) for LiveKit E2EE
/// * `epoch` - Key epoch (0 for initial, increments on rotation)
/// * `channel_id` - Group channel ID
pub fn build_group_call_key_content(
    call_id: &str,
    key: [u8; 32],
    epoch: u32,
    channel_id: &str,
) -> Content {
    use crate::proto::content_v1::GroupCallKey;
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::GroupCallKey(GroupCallKey {
            call_id: call_id.to_string(),
            key_material: key.to_vec(),
            key_epoch: epoch,
            timestamp: now,
            channel_id: channel_id.to_string(),
        })),
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
            },
            timestamp: now,
        })),
        ..Default::default()
    }
}

// =============================================================================
// Anonymous Call Signaling (Anon DM + Anon Group)
// =============================================================================

/// Generate a new anonymous call room ID and name.
///
/// Returns `(room_id_bytes, room_name, encryption_key)`.
/// The room name has format `"anon-{uuid}"`.
pub fn generate_anon_call_room() -> ([u8; 16], String, Vec<u8>) {
    let call_room_id = uuid::Uuid::new_v4();
    let room_id_bytes: [u8; 16] = *call_room_id.as_bytes();
    let room_name = format!("anon-{}", call_room_id);

    // Generate random 32-byte E2EE key
    let mut encryption_key = vec![0u8; 32];
    rand::rng().fill_bytes(&mut encryption_key);

    (room_id_bytes, room_name, encryption_key)
}

/// Build an `AnonCallToken` proto from a `CallRoomToken`.
pub fn build_anon_call_token(token: &crate::mailbox::CallRoomToken) -> crate::proto::content_v1::AnonCallToken {
    crate::proto::content_v1::AnonCallToken {
        public_key: token.public_key.clone(),
        signature: token.signature.clone(),
        message_randomizer: token.randomizer.clone().unwrap_or_default(),
    }
}

/// Create a Content proto for an anonymous call offer.
///
/// Used by both anon DM and anon group call initiation.
pub fn build_anon_call_offer_content(
    call_room_id: &[u8; 16],
    room_name: &str,
    token: &crate::mailbox::CallRoomToken,
    video: bool,
    display_name: &str,
    encryption_key: &[u8],
) -> Content {
    use crate::proto::content_v1::{AnonCallOffer, anon_call_offer::CallType};

    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        padding: generate_padding(PADDING_LARGE),
        content: Some(content::Content::AnonCallOffer(AnonCallOffer {
            call_room_id: call_room_id.to_vec(),
            room_name: room_name.to_string(),
            caller_token: Some(build_anon_call_token(token)),
            call_type: if video { CallType::Video.into() } else { CallType::Voice.into() },
            timestamp: now,
            caller_display_name: display_name.to_string(),
            call_encryption_key: encryption_key.to_vec(),
        })),
        ..Default::default()
    }
}

/// Create a Content proto for an anonymous call answer.
///
/// Used by both anon DM and anon group call acceptance/decline.
pub fn build_anon_call_answer_content(
    call_room_id: &[u8; 16],
    accept: bool,
    token: Option<&crate::mailbox::CallRoomToken>,
    decline_reason: i32,
    display_name: &str,
) -> Content {
    use crate::proto::content_v1::AnonCallAnswer;

    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        padding: generate_padding(PADDING_LARGE),
        content: Some(content::Content::AnonCallAnswer(AnonCallAnswer {
            call_room_id: call_room_id.to_vec(),
            accepted: accept,
            callee_token: if accept { token.map(build_anon_call_token) } else { None },
            timestamp: now,
            callee_display_name: display_name.to_string(),
            decline_reason,
        })),
        ..Default::default()
    }
}

/// Create a Content proto for an anonymous call hangup.
///
/// Used by both anon DM and anon group call termination.
pub fn build_anon_call_hangup_content(
    call_room_id: &[u8; 16],
    reason: i32,
) -> Content {
    use crate::proto::content_v1::AnonCallHangup;

    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        padding: generate_padding(PADDING_MESSAGE),
        content: Some(content::Content::AnonCallHangup(AnonCallHangup {
            call_room_id: call_room_id.to_vec(),
            reason,
            timestamp: now,
        })),
        ..Default::default()
    }
}
