//! Integration tests for the content builder functions.
//!
//! Validates the proto message construction used in "Message Send Flow" (section 3):
//!
//!   Content (protobuf) → Sealed Sender Encrypt → Ephemeral Mailbox Delivery
//!
//! Each content type (text, typing, receipt, reaction, pin, edit, delete, call)
//! has builder functions that construct the correct protobuf envelope.
//! These are pure functions with no dependencies.

use chatter_core::proto::content_v1::{self, content};
use chatter_core::services::{
    CallHangupReason, PADDING_GROUP_CONTROL, PADDING_LARGE, PADDING_MESSAGE,
    build_call_hangup_content, build_call_key_answer, build_call_key_offer,
    build_data_message_content, build_delete_for_everyone_content, build_delete_for_me_content,
    build_delivery_receipt_content, build_edit_content, build_edit_content_with_channel,
    build_group_call_signal_content, build_pin_content, build_reaction_content,
    build_reaction_content_with_channel, build_read_receipt_content,
    build_read_receipt_content_with_channel, build_reply_content, build_saved_message_sync_content,
    build_text_content, build_typing_content, build_typing_content_with_channel, generate_padding,
};

// ---------------------------------------------------------------------------
// Padding
// ---------------------------------------------------------------------------

/// Test that generate_padding produces correct-sized output.
///
/// Architecture reference:
///   Padding buckets: 256 bytes (typing/receipts), 512 bytes (short messages),
///   1024 bytes (typical messages), 4096 bytes (messages with attachments)
///   Client MUST pad to the nearest bucket size before encryption.
#[test]
fn test_generate_padding_sizes() {
    // Padding constants are defined for traffic analysis protection
    assert!(
        PADDING_GROUP_CONTROL > 0,
        "Group control padding should be non-zero"
    );
    assert!(PADDING_MESSAGE > 0, "Message padding should be non-zero");
    assert!(PADDING_LARGE > 0, "Large padding should be non-zero");

    let p1 = generate_padding(PADDING_GROUP_CONTROL);
    assert_eq!(p1.len(), PADDING_GROUP_CONTROL);

    let p2 = generate_padding(PADDING_MESSAGE);
    assert_eq!(p2.len(), PADDING_MESSAGE);

    let p3 = generate_padding(PADDING_LARGE);
    assert_eq!(p3.len(), PADDING_LARGE);

    // Padding should be random (two calls produce different bytes)
    let a = generate_padding(32);
    let b = generate_padding(32);
    assert_ne!(a, b, "Padding should be random");
}

/// Test zero-length padding.
#[test]
fn test_generate_padding_zero() {
    let p = generate_padding(0);
    assert!(p.is_empty());
}

// ---------------------------------------------------------------------------
// Text / DataMessage content
// ---------------------------------------------------------------------------

/// Test building a simple text message content.
///
/// Architecture reference:
///   User clicks Send → Content protobuf constructed with text body
///   Content wraps a DataMessage for chat messages.
#[test]
fn test_build_text_content() {
    let content = build_text_content("Hello, world!", 1234567890);

    assert!(content.content.is_some(), "Content should have a type");

    match content.content.unwrap() {
        content::Content::DataMessage(dm) => {
            assert_eq!(dm.body, "Hello, world!");
            assert_eq!(dm.timestamp, 1234567890);
        }
        other => panic!("Expected DataMessage, got {:?}", other),
    }

    // Padding may or may not be present depending on the builder
    // (some builders add it, others delegate to the encryption layer)
}

/// Test building a DataMessage with attachments.
///
/// Architecture reference:
///   Messages can include file attachments (pointer to encrypted CDN blob).
#[test]
fn test_build_data_message_with_attachments() {
    let attachment = content_v1::AttachmentPointer {
        cdn_key: "test-cdn-key-123".to_string(),
        content_type: "image/png".to_string(),
        plaintext_size: 12345,
        ..Default::default()
    };

    let content = build_data_message_content("Check this out", vec![attachment.clone()], 99999);

    match content.content.unwrap() {
        content::Content::DataMessage(dm) => {
            assert_eq!(dm.body, "Check this out");
            assert_eq!(dm.attachments.len(), 1);
            assert_eq!(dm.attachments[0].cdn_key, "test-cdn-key-123");
            assert_eq!(dm.attachments[0].content_type, "image/png");
        }
        other => panic!("Expected DataMessage, got {:?}", other),
    }
}

/// Test building a reply content with quote.
///
/// Architecture reference:
///   Replies include a Quote with the original message's author and timestamp.
#[test]
fn test_build_reply_content() {
    let content = build_reply_content(
        "Great point!",
        vec![],
        "author-uuid-123",
        1000000000,
        "Original message text",
        vec![], // quote_attachments
        2000000000,
    );

    match content.content.unwrap() {
        content::Content::DataMessage(dm) => {
            assert_eq!(dm.body, "Great point!");
            assert_eq!(dm.timestamp, 2000000000);
            // Quote should be present
            let quote = dm.quote.expect("Reply should have a quote");
            assert_eq!(quote.author_aci, "author-uuid-123");
            assert_eq!(quote.id, 1000000000);
        }
        other => panic!("Expected DataMessage, got {:?}", other),
    }
}

/// Test building empty text (body can be empty for attachment-only messages).
#[test]
fn test_build_text_content_empty_body() {
    let content = build_text_content("", 12345);
    match content.content.unwrap() {
        content::Content::DataMessage(dm) => {
            assert_eq!(dm.body, "");
        }
        _ => panic!("Expected DataMessage"),
    }
}

// ---------------------------------------------------------------------------
// Typing indicators
// ---------------------------------------------------------------------------

/// Test building typing indicator content.
///
/// Architecture reference:
///   base_anon dispatcher → DomainEvent::TypingStarted/TypingStopped
#[test]
fn test_build_typing_started() {
    let content = build_typing_content(true);
    match content.content.unwrap() {
        content::Content::Typing(tm) => {
            // action: 0=Unspecified, 1=Started, 2=Stopped
            assert_eq!(tm.action, 1, "started=true should be action 1 (STARTED)");
        }
        other => panic!("Expected Typing, got {:?}", other),
    }
}

#[test]
fn test_build_typing_stopped() {
    let content = build_typing_content(false);
    match content.content.unwrap() {
        content::Content::Typing(tm) => {
            assert_eq!(tm.action, 2, "started=false should be action 2 (STOPPED)");
        }
        other => panic!("Expected Typing, got {:?}", other),
    }
}

/// Test typing with channel scope.
#[test]
fn test_build_typing_with_channel() {
    let content = build_typing_content_with_channel(true, Some("ch-123"));
    match content.content.unwrap() {
        content::Content::Typing(tm) => {
            assert_eq!(tm.channel_id, "ch-123");
        }
        other => panic!("Expected Typing, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Receipts
// ---------------------------------------------------------------------------

/// Test building read receipt content.
///
/// Architecture reference:
///   base_anon dispatcher → DomainEvent::MessagesRead
///   Read receipts carry timestamps of the messages that were read.
#[test]
fn test_build_read_receipt() {
    let timestamps = vec![100, 200, 300];
    let content = build_read_receipt_content(timestamps.clone());

    match content.content.unwrap() {
        content::Content::Receipt(rm) => {
            assert_eq!(rm.r#type, 2, "Type 2 = READ receipt");
            assert_eq!(rm.timestamps, timestamps);
        }
        other => panic!("Expected Receipt, got {:?}", other),
    }
}

/// Test building delivery receipt content.
///
/// Architecture reference:
///   InboxEventProcessor sends delivery receipt on message received.
#[test]
fn test_build_delivery_receipt() {
    let timestamps = vec![400, 500];
    let content = build_delivery_receipt_content(timestamps.clone());

    match content.content.unwrap() {
        content::Content::Receipt(rm) => {
            assert_eq!(rm.r#type, 1, "Type 1 = DELIVERY receipt");
            assert_eq!(rm.timestamps, timestamps);
        }
        other => panic!("Expected Receipt, got {:?}", other),
    }
}

/// Test receipt with channel scope.
#[test]
fn test_build_read_receipt_with_channel() {
    let content = build_read_receipt_content_with_channel(vec![100], Some("ch-456"));
    match content.content.unwrap() {
        content::Content::Receipt(rm) => {
            assert_eq!(rm.channel_id, "ch-456");
        }
        other => panic!("Expected Receipt, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Reactions
// ---------------------------------------------------------------------------

/// Test building an emoji reaction.
///
/// Architecture reference:
///   base_anon dispatcher → DomainEvent::ReactionAdded/ReactionRemoved
#[test]
fn test_build_reaction_add() {
    let content = build_reaction_content("👍", false, "author-abc", 12345);

    match content.content.unwrap() {
        content::Content::Reaction(r) => {
            assert_eq!(r.emoji, "👍");
            assert_eq!(r.remove, false);
            assert_eq!(r.target_author_aci, "author-abc");
            assert_eq!(r.target_sent_timestamp, 12345);
        }
        other => panic!("Expected Reaction, got {:?}", other),
    }
}

/// Test building a reaction removal.
#[test]
fn test_build_reaction_remove() {
    let content = build_reaction_content("👍", true, "author-abc", 12345);

    match content.content.unwrap() {
        content::Content::Reaction(r) => {
            assert_eq!(r.remove, true);
        }
        other => panic!("Expected Reaction, got {:?}", other),
    }
}

/// Test reaction with channel scope.
/// The channel_id is stored on the Reaction message's channel_id field if present,
/// otherwise check the emoji to verify the reaction was created correctly.
#[test]
fn test_build_reaction_with_channel() {
    let content =
        build_reaction_content_with_channel("❤️", false, "author-xyz", 99, Some("ch-789"));
    match content.content.unwrap() {
        content::Content::Reaction(r) => {
            assert_eq!(r.emoji, "❤️");
            // Channel ID is encoded in the target_author_aci field as prefix
            assert!(r.target_author_aci.contains("author-xyz"));
            assert!(r.target_author_aci.contains("ch-789"));
            assert_eq!(r.target_sent_timestamp, 99);
        }
        other => panic!("Expected Reaction, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Pin/Unpin
// ---------------------------------------------------------------------------

/// Test building a pin message.
///
/// Architecture reference:
///   base_anon dispatcher → DomainEvent::MessagePinned/MessageUnpinned
#[test]
fn test_build_pin() {
    let content = build_pin_content("author-abc", 12345, false);
    match content.content.unwrap() {
        content::Content::Pin(p) => {
            assert_eq!(p.target_author_aci, "author-abc");
            assert_eq!(p.target_sent_timestamp, 12345);
            assert_eq!(p.unpin, false);
        }
        other => panic!("Expected Pin, got {:?}", other),
    }
}

/// Test building an unpin message.
#[test]
fn test_build_unpin() {
    let content = build_pin_content("author-abc", 12345, true);
    match content.content.unwrap() {
        content::Content::Pin(p) => {
            assert_eq!(p.unpin, true);
        }
        other => panic!("Expected Pin, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Edit
// ---------------------------------------------------------------------------

/// Test building an edit message.
#[test]
fn test_build_edit() {
    let content = build_edit_content(12345, "Updated text");
    match content.content.unwrap() {
        content::Content::Edit(e) => {
            assert_eq!(e.target_sent_timestamp, 12345);
            assert_eq!(e.data_message.unwrap().body, "Updated text");
        }
        other => panic!("Expected Edit, got {:?}", other),
    }
}

/// Test edit with channel scope.
#[test]
fn test_build_edit_with_channel() {
    let content = build_edit_content_with_channel(12345, "New text", Some("ch-111"));
    match content.content.unwrap() {
        content::Content::Edit(e) => {
            let dm = e.data_message.unwrap();
            assert_eq!(dm.channel_id, "ch-111");
        }
        other => panic!("Expected Edit, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// Test building a delete-for-me message (multi-device sync).
///
/// Architecture reference:
///   DeleteForMeSync → synced across user's own devices
#[test]
fn test_build_delete_for_me() {
    let content = build_delete_for_me_content("conv-id-123", "author-abc", 12345);
    match content.content.unwrap() {
        content::Content::DeleteForMeSync(d) => {
            assert_eq!(d.conversation_id, "conv-id-123");
            assert_eq!(d.target_author_aci, "author-abc");
            assert_eq!(d.target_sent_timestamp, 12345);
        }
        other => panic!("Expected DeleteForMeSync, got {:?}", other),
    }
}

/// Test building a delete-for-everyone message.
#[test]
fn test_build_delete_for_everyone() {
    let content = build_delete_for_everyone_content(12345);
    match content.content.unwrap() {
        content::Content::DataMessage(dm) => {
            // Delete-for-everyone is a DataMessage with a delete field
            assert!(dm.delete.is_some());
            let delete = dm.delete.unwrap();
            assert_eq!(delete.target_sent_timestamp, 12345);
        }
        other => panic!("Expected DataMessage with delete, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Saved message sync
// ---------------------------------------------------------------------------

/// Test building a saved message sync content.
#[test]
fn test_build_saved_message_sync() {
    let content = build_saved_message_sync_content(
        "msg-id-123",
        "Important note",
        9999,
        vec!["att-1".to_string(), "att-2".to_string()],
    );

    // Saved message sync wraps text and attachment IDs for multi-device sync
    assert!(content.content.is_some(), "Should have content");
}

// ---------------------------------------------------------------------------
// Call key exchange
// ---------------------------------------------------------------------------

/// Test building a call key offer.
///
/// Architecture reference:
///   Call E2EE key exchange:
///   CallKeyOffer { call_id, ephemeral_public_key, call_type, channel_id }
#[test]
fn test_build_call_key_offer() {
    let ephemeral_pk = [42u8; 32];
    let content = build_call_key_offer("call-123", ephemeral_pk, "video", "ch-456");

    match content.content.unwrap() {
        content::Content::CallKey(ck) => {
            assert_eq!(ck.call_id, "call-123");
            assert_eq!(ck.key_material.as_slice(), &ephemeral_pk[..]);
            assert_eq!(ck.call_type, "video");
            assert_eq!(ck.channel_id, "ch-456");
        }
        other => panic!("Expected CallKey, got {:?}", other),
    }
}

/// Test building a call key answer.
#[test]
fn test_build_call_key_answer() {
    let ephemeral_pk = [99u8; 32];
    let content = build_call_key_answer("call-123", ephemeral_pk);

    match content.content.unwrap() {
        content::Content::CallKey(ck) => {
            assert_eq!(ck.call_id, "call-123");
            assert_eq!(ck.key_material.as_slice(), &ephemeral_pk[..]);
            // Answer has no call_type or channel_id (they were in the offer)
        }
        other => panic!("Expected CallKey, got {:?}", other),
    }
}

/// A group-call key rotation carries its key and epoch; a ring and a key
/// request carry no key at all (§124 1k).
#[test]
fn test_build_group_call_signal() {
    use chatter_core::domain::call::GroupCallSignal;
    use chatter_core::proto::content_v1::group_call_signal::Signal;
    let key = [55u8; 32];
    let signal_of = |s| match build_group_call_signal_content("group-call-789", "ch-999", s)
        .content
        .unwrap()
    {
        content::Content::GroupCallSignal(gcs) => {
            assert_eq!(gcs.call_id, "group-call-789");
            assert_eq!(gcs.channel_id, "ch-999");
            gcs.signal.expect("signal set")
        }
        other => panic!("Expected GroupCallSignal, got {:?}", other),
    };
    match signal_of(GroupCallSignal::Key { key, epoch: 3 }) {
        Signal::Key(k) => {
            assert_eq!(k.key_material.as_slice(), &key[..]);
            assert_eq!(k.key_epoch, 3);
        }
        other => panic!("Expected Key, got {:?}", other),
    }
    assert!(
        matches!(signal_of(GroupCallSignal::Ring), Signal::Ring(_)),
        "a ring carries no key"
    );
    assert!(matches!(
        signal_of(GroupCallSignal::KeyRequest),
        Signal::KeyRequest(_)
    ));
}

/// Test building a call hangup notification.
#[test]
fn test_build_call_hangup() {
    let content = build_call_hangup_content("call-123", CallHangupReason::Normal);
    match content.content.unwrap() {
        content::Content::CallHangup(ch) => {
            assert_eq!(ch.call_id, "call-123");
        }
        other => panic!("Expected CallHangup, got {:?}", other),
    }
}

/// Test all hangup reasons.
#[test]
fn test_call_hangup_reasons() {
    let reasons = [
        CallHangupReason::Normal,
        CallHangupReason::Declined,
        CallHangupReason::Busy,
        CallHangupReason::Timeout,
        CallHangupReason::NetworkError,
    ];

    for reason in reasons {
        let content = build_call_hangup_content("call-999", reason);
        assert!(
            content.content.is_some(),
            "Hangup with {:?} should produce content",
            reason
        );
    }
}

// ---------------------------------------------------------------------------
// Proto serialization roundtrip
// ---------------------------------------------------------------------------

/// Test that Content can be serialized and deserialized via prost.
///
/// Architecture reference:
///   Content (protobuf) → Sealed Sender Encrypt → Ephemeral Mailbox Delivery
///   The Content must survive serialization for the encryption layer.
#[test]
fn test_content_proto_roundtrip() {
    use prost::Message;

    let original = build_text_content("roundtrip test", 42);

    // Serialize
    let bytes = original.encode_to_vec();
    assert!(!bytes.is_empty(), "Serialized content should not be empty");

    // Deserialize
    let decoded =
        content_v1::Content::decode(bytes.as_slice()).expect("Should decode successfully");

    // Verify
    assert_eq!(original, decoded);
}

/// Test that content with attachments survives serialization.
#[test]
fn test_content_with_attachment_roundtrip() {
    use prost::Message;

    let attachment = content_v1::AttachmentPointer {
        cdn_key: "cdn-key-xyz".to_string(),
        content_type: "video/mp4".to_string(),
        plaintext_size: 1024 * 1024,
        encryption_key: vec![0u8; 32],
        digest: vec![1u8; 32],
        ..Default::default()
    };

    let original = build_data_message_content("video message", vec![attachment], 555);

    let bytes = original.encode_to_vec();
    let decoded = content_v1::Content::decode(bytes.as_slice()).unwrap();

    match decoded.content.unwrap() {
        content::Content::DataMessage(dm) => {
            assert_eq!(dm.attachments.len(), 1);
            assert_eq!(dm.attachments[0].cdn_key, "cdn-key-xyz");
            assert_eq!(dm.attachments[0].plaintext_size, 1024 * 1024);
        }
        _ => panic!("Expected DataMessage"),
    }
}
