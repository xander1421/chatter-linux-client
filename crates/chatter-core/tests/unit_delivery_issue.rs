//! A DM that fails to decrypt must not vanish.
//!
//! The inbox acks every polled message regardless of outcome (correct — the
//! queue otherwise never drains and every launch replays the whole history),
//! and the `DecryptionErrorMessage` we send back only makes the SENDER archive
//! its session, which affects the NEXT message. The one that failed is gone.
//!
//! Signal's answer to that on the receiving side is a visible conversation
//! entry: Signal-Android inserts a "Delivery issue" update
//! (`ThreadRecord_delivery_issue`, `BadDecryptLearnMoreDialog_couldnt_be_delivered_individual`
//! — "A message, sticker, reaction, or read receipt couldn't be delivered to
//! you from %s"). These tests pin that behaviour.

use std::sync::Arc;

use chatter_core::domain::message::{ConversationType, MessageContent, SystemMessage};
use chatter_core::domain::{ChannelId, UserId};
use chatter_core::inbox::polling::device::delivery_issue_message;
use chatter_core::storage::repositories::MessageRepository;
use chatter_core::storage::Database;
use tempfile::TempDir;

fn setup_test_db() -> (TempDir, Arc<Database>) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let db = Database::open(&db_path, "test_passphrase").unwrap();
    db.run_migrations().unwrap();
    (temp_dir, Arc::new(db))
}

/// The marker lands in the peer's DM conversation, as a system entry, and says
/// both what was lost and what fixes it.
#[test]
fn delivery_issue_marker_is_a_system_entry_in_the_peers_dm_conversation() {
    let peer = UserId::new();
    let envelope_ts_ns: u64 = 1_777_000_000_000_000_000;

    let msg = delivery_issue_message(peer, envelope_ts_ns);

    assert_eq!(
        msg.conversation_id,
        ChannelId::from(peer),
        "the marker must land in the conversation with the peer whose message was lost"
    );
    assert_eq!(
        msg.conversation_type,
        ConversationType::ServerDm,
        "server DMs are read back as ServerDm; any other type hides the marker"
    );
    assert_eq!(
        msg.sender_id,
        Some(peer),
        "the UI renders system entries as \"{{sender_name}} {{description}}\", so the \
         marker must name the peer or it reads as \"Anonymous\""
    );

    let MessageContent::System(SystemMessage::DeliveryIssue) = &msg.content else {
        panic!("expected a System(DeliveryIssue) entry, got {:?}", msg.content);
    };

    let text = SystemMessage::DeliveryIssue.description();
    assert!(
        text.to_lowercase().contains("deliver"),
        "the marker must say something was not delivered, got {text:?}"
    );
    assert!(
        text.to_lowercase().contains("again"),
        "the marker must give the user the action that fixes it (ask them to send it \
         again), got {text:?}"
    );
}

/// The marker is persisted and shows up when the conversation is read back —
/// this is what makes the gap visible instead of silent.
#[test]
fn delivery_issue_marker_is_persisted_and_read_back() {
    let (_dir, db) = setup_test_db();
    let repo = MessageRepository::new(db);

    let peer = UserId::new();
    let conversation: ChannelId = peer.into();

    repo.insert(&delivery_issue_message(peer, 1_777_000_000_000_000_000))
        .expect("insert delivery-issue marker");

    let listed = repo
        .list_for_conversation(&conversation, &ConversationType::ServerDm, None, 50)
        .expect("list conversation");

    assert_eq!(
        listed.len(),
        1,
        "a decrypt failure left no trace in the conversation the user is looking at"
    );
    assert!(
        matches!(
            listed[0].content,
            MessageContent::System(SystemMessage::DeliveryIssue)
        ),
        "expected the delivery-issue marker, got {:?}",
        listed[0].content
    );
}

/// Replays must not stack up markers. If `acknowledge` fails, the same
/// undecryptable envelope is polled again on the next start and fails again —
/// keying the marker to the envelope's own timestamp makes the existing UNIQUE
/// index (conversation_type, conversation_id, sender_id, sent_at) collapse the
/// repeat instead of printing "couldn't be delivered" once per launch.
#[test]
fn replaying_the_same_undecryptable_envelope_does_not_stack_markers() {
    let (_dir, db) = setup_test_db();
    let repo = MessageRepository::new(db);

    let peer = UserId::new();
    let conversation: ChannelId = peer.into();
    let envelope_ts_ns: u64 = 1_777_000_000_000_000_000;

    // Same envelope, three poll cycles.
    for _ in 0..3 {
        let _ = repo.insert(&delivery_issue_message(peer, envelope_ts_ns));
    }

    let listed = repo
        .list_for_conversation(&conversation, &ConversationType::ServerDm, None, 50)
        .expect("list conversation");

    assert_eq!(
        listed.len(),
        1,
        "the same undecryptable envelope produced {} markers — a replayed queue \
         will spam the conversation",
        listed.len()
    );
}
