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

// ============================================================================
// The SECOND marker: a peer identity change.
//
// A delivery issue self-heals — the `DecryptionErrorMessage` makes the sender
// archive its session and the next message decrypts, which is why that marker
// says "ask them to send it again". An identity change does not: nothing was
// sent back, resending re-presents the same new key, and only a human accepting
// it clears the wedge. Reusing `DeliveryIssue` here would print that advice
// under a failure it cannot fix.
// ============================================================================

use chatter_core::inbox::polling::device::identity_changed_message;

/// The marker lands in the same place with the same shape as `DeliveryIssue`,
/// so the existing conversation plumbing renders it — but as its OWN variant.
#[test]
fn identity_change_marker_is_a_distinct_system_entry_in_the_peers_dm() {
    let peer = UserId::new();
    let msg = identity_changed_message(peer, 1_777_000_000_000_000_000, true);

    assert_eq!(msg.conversation_id, ChannelId::from(peer));
    assert_eq!(msg.conversation_type, ConversationType::ServerDm);
    assert_eq!(
        msg.sender_id,
        Some(peer),
        "system entries render as \"{{sender_name}} {{description}}\""
    );

    let MessageContent::System(SystemMessage::IdentityChanged { attributed }) = &msg.content else {
        panic!("expected System(IdentityChanged), got {:?}", msg.content);
    };
    assert!(*attributed);
}

/// The two markers must not say the same thing. Telling the user to ask for a
/// resend after an identity change sends them round a loop that cannot
/// terminate: `ensure_session` refuses for as long as the change is parked.
#[test]
fn the_identity_change_marker_does_not_advise_a_resend() {
    let text = SystemMessage::IdentityChanged { attributed: true }.description();

    assert!(
        text.to_lowercase().contains("safety number"),
        "the marker must name what changed, got {text:?}"
    );
    assert!(
        !text.to_lowercase().contains("send it again"),
        "resending fails identically forever — that is `DeliveryIssue`'s advice, \
         not this one's. Got {text:?}"
    );
    assert_ne!(
        text,
        SystemMessage::DeliveryIssue.description(),
        "the two failures have different resolutions and must not share copy"
    );
}

/// An unattributed change was asserted by the SERVER and proved by nobody, so
/// the entry must not state as fact that this was the peer.
#[test]
fn an_unattributed_identity_change_marker_hedges() {
    let attributed = SystemMessage::IdentityChanged { attributed: true }.description();
    let claimed = SystemMessage::IdentityChanged { attributed: false }.description();

    assert_ne!(
        attributed, claimed,
        "collapsing the two would let a malicious server present its own key \
         change as though it had been cryptographically attributed"
    );
    assert!(
        claimed.to_lowercase().contains("may have"),
        "the unattributed wording must hedge, got {claimed:?}"
    );
    assert!(
        claimed.to_lowercase().contains("server"),
        "it must say who made the claim, got {claimed:?}"
    );
}

/// Both markers can coexist in one conversation, and neither collapses the
/// other: they are different rows keyed by different envelope timestamps.
#[test]
fn identity_change_and_delivery_issue_markers_coexist() {
    let (_dir, db) = setup_test_db();
    let repo = MessageRepository::new(db);

    let peer = UserId::new();
    let conversation: ChannelId = peer.into();

    repo.insert(&delivery_issue_message(peer, 1_777_000_000_000_000_000))
        .expect("insert delivery-issue marker");
    repo.insert(&identity_changed_message(peer, 1_777_000_000_000_000_001, true))
        .expect("insert identity-change marker");

    let listed = repo
        .list_for_conversation(&conversation, &ConversationType::ServerDm, None, 50)
        .expect("list conversation");

    assert_eq!(listed.len(), 2, "one marker swallowed the other");
    assert!(
        listed.iter().any(|m| matches!(
            m.content,
            MessageContent::System(SystemMessage::IdentityChanged { .. })
        )),
        "the identity-change marker must survive the round trip through JSON"
    );
}

/// A replayed envelope must not stack identity-change markers either.
#[test]
fn replaying_the_same_identity_change_envelope_does_not_stack_markers() {
    let (_dir, db) = setup_test_db();
    let repo = MessageRepository::new(db);

    let peer = UserId::new();
    let conversation: ChannelId = peer.into();
    let envelope_ts_ns: u64 = 1_777_000_000_000_000_000;

    for _ in 0..3 {
        let _ = repo.insert(&identity_changed_message(peer, envelope_ts_ns, true));
    }

    let listed = repo
        .list_for_conversation(&conversation, &ConversationType::ServerDm, None, 50)
        .expect("list conversation");

    assert_eq!(listed.len(), 1, "a replayed queue will spam the conversation");
}
