//! Content builders for constructing Content proto messages.
//!
//! These functions create properly formatted Content proto messages
//! for various message types (text, typing, receipts, reactions, etc.).

use crate::proto::content_v1::{
    AttachmentPointer, Content, DataMessage, Delete, DeleteForMeSync, EditMessage,
    PinMessage, Reaction, ReceiptMessage, TypingMessage, content, typing_message, receipt_message,
};
use crate::util::time::TimestampNanos;

use super::CallHangupReason;

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
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::Receipt(ReceiptMessage {
            r#type: receipt_message::Type::Read as i32,
            timestamps: message_timestamps,
            timestamp: now,
        })),
        ..Default::default()
    }
}

/// Create a Content proto for delivery receipt.
pub fn build_delivery_receipt_content(message_timestamps: Vec<u64>) -> Content {
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::Receipt(ReceiptMessage {
            r#type: receipt_message::Type::Delivery as i32,
            timestamps: message_timestamps,
            timestamp: now,
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

/// Create a Content proto for call key exchange (incoming call notification).
///
/// This is sent via sealed sender to notify the peer of an incoming call
/// and share the E2EE key material for the call.
///
/// Note: call_type and channel_id are included in the API for consistency but
/// are not part of the proto - the receiver derives these from conversation context.
pub fn build_call_key_content(
    call_id: &str,
    key_material: Vec<u8>,
    _call_type: &str,
    _channel_id: &str,
) -> Content {
    use crate::proto::content_v1::CallKeyExchange;
    let now = TimestampNanos::now().as_u64();
    Content {
        timestamp: now,
        content: Some(content::Content::CallKey(CallKeyExchange {
            call_id: call_id.to_string(),
            key_material,
            timestamp: now,
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
