//! One test per inbox loop: a failed decrypt must not vanish silently.
//!
//! Acking makes the SERVER DELETE the envelope, so each of these asserts the
//! same two things about the loop's failure seam:
//!
//! 1. A **permanent** failure is acked only after a marker exists in the
//!    conversation the user is looking at. Ack without a marker is silent data
//!    loss; that is the bug in `dm`, `group`, and the device inbox's generic
//!    branch.
//! 2. A **transient** failure is not acked at all, so it redelivers. Acking it
//!    deletes a message that would have decrypted on the next poll.
//!
//! And, for `anon_dm`, the mirror-image bug: a permanent failure that is never
//! acked redelivers forever.

use std::sync::Arc;

use tempfile::TempDir;

use crate::domain::anon_dm::AnonDmId;
use crate::domain::ids::ChannelId;
use crate::domain::message::{ConversationType, MessageContent, SystemMessage};
use crate::domain::UserId;
use crate::error::{CryptoError, IdentityChangeSource, ServiceError, StorageError};
use crate::inbox::ack_policy::AckDecision;
use crate::storage::{Database, MessageRepository};

fn repo() -> (TempDir, Arc<MessageRepository>) {
    let dir = TempDir::new().unwrap();
    let db = Database::open(&dir.path().join("test.db"), "test_passphrase").unwrap();
    db.run_migrations().unwrap();
    (dir, Arc::new(MessageRepository::new(Arc::new(db))))
}

fn markers(
    repo: &MessageRepository,
    conversation: &ChannelId,
    ty: ConversationType,
) -> Vec<SystemMessage> {
    repo.list_for_conversation(conversation, &ty, None, 50)
        .expect("list conversation")
        .into_iter()
        .filter_map(|m| match m.content {
            MessageContent::System(s) => Some(s),
            _ => None,
        })
        .collect()
}

/// A dead ciphertext: deterministic in the bytes, so redelivery cannot help.
fn permanent_error() -> ServiceError {
    ServiceError::Crypto(CryptoError::DecryptionFailed("MAC verification failed".into()))
}

/// The peer's pinned key changed. Also permanent, but a different resolution.
fn identity_change_error() -> ServiceError {
    ServiceError::Crypto(CryptoError::UntrustedIdentity {
        peer_id: UserId::new().to_string(),
        device_id: 1,
        new_identity_key: None,
        attribution: IdentityChangeSource::SenderCertificate,
    })
}

/// No session yet — the bootstrap response or SKDM may land on a later poll.
fn transient_error() -> ServiceError {
    ServiceError::Crypto(CryptoError::SessionNotFound("peer:1".into()))
}

/// Our disk, not their ciphertext. Also the case where the marker write fails.
fn storage_error() -> ServiceError {
    ServiceError::Storage(StorageError::ConnectionPool("database is locked".into()))
}

// ===========================================================================
// LOOP 1 — device inbox (`polling::device` + `provider::run_loop`)
// ===========================================================================

#[test]
fn device_inbox_permanent_failure_is_marked_before_it_is_acked() {
    let (_d, repo) = repo();
    let peer = UserId::new();

    let decision = super::device::device_failure_decision(
        Some(peer),
        1_777_000_000_000_000_000,
        &permanent_error(),
        &repo,
        None,
    );

    let found = markers(&repo, &peer.into(), ConversationType::ServerDm);
    assert_eq!(
        found,
        vec![SystemMessage::DeliveryIssue],
        "the device inbox acked a dead envelope with no marker — the user sees an \
         unexplained gap and the server has already deleted the only copy"
    );
    assert_eq!(decision, AckDecision::Ack);
}

#[test]
fn device_inbox_transient_failure_is_not_acked() {
    let (_d, repo) = repo();
    let peer = UserId::new();

    let decision = super::device::device_failure_decision(
        Some(peer),
        1_777_000_000_000_000_000,
        &transient_error(),
        &repo,
        None,
    );

    assert_eq!(
        decision,
        AckDecision::Retain,
        "no session yet is not a dead message — acking deletes mail that would \
         have decrypted once the bootstrap landed"
    );
    assert!(
        markers(&repo, &peer.into(), ConversationType::ServerDm).is_empty(),
        "a message that will be redelivered must not also be reported as lost"
    );
}

// ===========================================================================
// LOOP 2 — DM mailboxes (`polling::dm`)
// ===========================================================================

#[test]
fn dm_permanent_failure_is_marked_before_it_is_acked() {
    let (_d, repo) = repo();
    let peer = UserId::new();

    let decision = super::dm::dm_failure_decision(
        peer,
        1_777_000_000_000_000_000,
        &permanent_error(),
        &repo,
        None,
    );

    let found = markers(&repo, &peer.into(), ConversationType::ServerDm);
    assert_eq!(
        found,
        vec![SystemMessage::DeliveryIssue],
        "the DM loop collects every message id BEFORE the decrypt loop and acks \
         the lot — a failed DM vanishes with no marker of any kind"
    );
    assert_eq!(decision, AckDecision::Ack);
}

#[test]
fn dm_identity_change_gets_its_own_marker_not_the_delivery_issue_one() {
    let (_d, repo) = repo();
    let peer = UserId::new();

    super::dm::dm_failure_decision(
        peer,
        1_777_000_000_000_000_000,
        &identity_change_error(),
        &repo,
        None,
    );

    assert_eq!(
        markers(&repo, &peer.into(), ConversationType::ServerDm),
        vec![SystemMessage::IdentityChanged { attributed: true }],
        "DeliveryIssue tells the user to ask for a resend; after an identity \
         change the resend re-presents the same key and fails identically forever"
    );
}

#[test]
fn dm_transient_failure_is_not_acked() {
    let (_d, repo) = repo();
    let peer = UserId::new();

    assert_eq!(
        super::dm::dm_failure_decision(
            peer,
            1_777_000_000_000_000_000,
            &transient_error(),
            &repo,
            None,
        ),
        AckDecision::Retain,
        "the DM loop acks the whole batch regardless of outcome, so a message \
         waiting on its bootstrap is deleted before the session exists"
    );
}

#[test]
fn dm_storage_failure_is_not_acked() {
    let (_d, repo) = repo();
    let peer = UserId::new();

    assert_eq!(
        super::dm::dm_failure_decision(
            peer,
            1_777_000_000_000_000_000,
            &storage_error(),
            &repo,
            None,
        ),
        AckDecision::Retain,
        "a locked database is our problem, not the ciphertext's"
    );
}

// ===========================================================================
// LOOP 3 — group mailboxes (`polling::group`)
// ===========================================================================

#[test]
fn group_permanent_failure_is_marked_in_the_group_before_it_is_acked() {
    let (_d, repo) = repo();
    let group = ChannelId::new();

    let decision = super::group::group_failure_decision(
        group,
        None,
        1_777_000_000_000_000_000,
        &permanent_error(),
        &repo,
        None,
    );

    assert_eq!(
        markers(&repo, &group, ConversationType::Channel),
        vec![SystemMessage::DeliveryIssue],
        "the group loop acks the whole batch regardless of outcome, so a group \
         message that failed to decrypt leaves no trace in the group"
    );
    assert_eq!(decision, AckDecision::Ack);
}

#[test]
fn group_transient_failure_is_not_acked() {
    let (_d, repo) = repo();
    let group = ChannelId::new();

    assert_eq!(
        super::group::group_failure_decision(
            group,
            None,
            1_777_000_000_000_000_000,
            &transient_error(),
            &repo,
            None,
        ),
        AckDecision::Retain,
        "a group message whose sender key has not arrived yet decrypts on a \
         later poll — acking it deletes it first"
    );
}

// ===========================================================================
// LOOP 4 — anonymous DM mailboxes (`polling::anon_dm`)
//
// The mirror image: this loop never acks a failure, so an old-format envelope
// that can never decrypt is redelivered forever.
// ===========================================================================

#[test]
fn anon_dm_permanent_failure_is_marked_and_then_acked_so_it_stops_redelivering() {
    let (_d, repo) = repo();
    let conversation = AnonDmId::new();

    let decision = super::anon_dm::anon_dm_failure_decision(
        conversation,
        1_777_000_000_000_000_000,
        "sealed sender decrypt failed: unsupported envelope version",
        &repo,
        None,
    );

    assert_eq!(
        decision,
        AckDecision::Ack,
        "never acking is what turns one old-build peer into an infinite \
         redelivery of something that can never decrypt"
    );
    assert_eq!(
        markers(&repo, &conversation.into(), ConversationType::AnonDm),
        vec![SystemMessage::DeliveryIssue],
        "and it must not be acked silently — the marker is the whole reason \
         draining it is safe"
    );
}

/// Replays must not stack markers: the marker is keyed to the envelope
/// timestamp so the UNIQUE index collapses repeats when `acknowledge` fails.
#[test]
fn a_replayed_envelope_does_not_stack_markers() {
    let (_d, repo) = repo();
    let peer = UserId::new();
    let ts = 1_777_000_000_000_000_000;

    for _ in 0..3 {
        super::dm::dm_failure_decision(peer, ts, &permanent_error(), &repo, None);
    }

    assert_eq!(
        markers(&repo, &peer.into(), ConversationType::ServerDm).len(),
        1,
        "a failed acknowledge replays the same envelope every start; one marker \
         per replay would spam the conversation"
    );
}
