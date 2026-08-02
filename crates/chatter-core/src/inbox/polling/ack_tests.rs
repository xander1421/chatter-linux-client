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
//!
//! Plus the two properties `Retain` needs in order to mean anything: that a
//! retained envelope is genuinely re-offered (see
//! `inbox::poller::next_after_id`), and that retaining it eventually stops.

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
        "envelope-dev-perm",
        &crate::inbox::ack_policy::RetryBudget::new(),
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
        "envelope-dev-transient",
        &crate::inbox::ack_policy::RetryBudget::new(),
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

// ---------------------------------------------------------------------------
// LOOP 1b — the three device-inbox paths that never consulted the policy
//
// `process_device_message` routed sender-key, identified-sender and ECIES-init
// envelopes to helpers that returned `()`, swallowing every error, and then
// returned `AckDecision::Ack` regardless. So the policy governed exactly one of
// the four device paths, and the other three deleted whatever they could not
// handle — including the failures the policy classifies transient by name.
// ---------------------------------------------------------------------------

fn channel_repo() -> (TempDir, Arc<crate::storage::MessageRepository>, Arc<crate::storage::ChannelRepository>)
{
    let dir = TempDir::new().unwrap();
    let db = Arc::new(Database::open(&dir.path().join("test.db"), "test_passphrase").unwrap());
    db.run_migrations().unwrap();
    (
        dir,
        Arc::new(MessageRepository::new(db.clone())),
        Arc::new(crate::storage::ChannelRepository::new(db)),
    )
}

/// A channel message that arrives before the sender key that opens it is the
/// SKDM race — the exact case the policy names as transient: "the SKDM … may
/// land on a later poll and then the same bytes decrypt". The device path
/// emitted `SenderKeyNeeded`, logged "will succeed on retry after SKDM
/// arrives", and then acked, so the server deleted the message before the key
/// it was waiting for could arrive. Nothing buffers it either:
/// `defer_pending_group_decrypt` is called from the channel-history path, never
/// from here.
#[test]
fn a_channel_message_waiting_on_its_sender_key_is_not_deleted_first() {
    use crate::inbox::ack_policy::RetryBudget;

    let (_d, repo, channels) = channel_repo();
    let channel = ChannelId::new();

    let decision = super::device::sender_key_missing_decision(
        channel,
        UserId::new(),
        1_777_000_000_000_000_000,
        &repo,
        &channels,
        None,
        "envelope-sk-1",
        &RetryBudget::new(),
    );

    assert_eq!(
        decision,
        AckDecision::Retain,
        "acking here deletes the message while the SKDM that decrypts it is \
         still in flight, and nothing on this path buffers the ciphertext"
    );
}

/// The same failures the sealed-sender path treats as transient must be
/// transient here too — a locked database says nothing about the ciphertext.
#[test]
fn a_sender_key_envelope_that_failed_on_our_own_storage_is_not_deleted() {
    use crate::inbox::ack_policy::RetryBudget;

    let (_d, repo, channels) = channel_repo();

    assert_eq!(
        super::device::sender_key_failure_decision(
            Some(ChannelId::new()),
            UserId::new(),
            1_777_000_000_000_000_000,
            &storage_error(),
            &repo,
            &channels,
            None,
            "envelope-sk-2",
            &RetryBudget::new(),
        ),
        AckDecision::Retain,
        "our disk, not their ciphertext"
    );
}

/// And a genuinely dead one must leave a trace — in the CHANNEL, which is what
/// the user is looking at, not in a DM with the sender.
#[test]
fn a_dead_sender_key_envelope_is_marked_in_the_channel_before_it_is_acked() {
    use crate::inbox::ack_policy::RetryBudget;

    let (_d, repo, channels) = channel_repo();
    let channel = ChannelId::new();

    let decision = super::device::sender_key_failure_decision(
        Some(channel),
        UserId::new(),
        1_777_000_000_000_000_000,
        &permanent_error(),
        &repo,
        &channels,
        None,
        "envelope-sk-3",
        &RetryBudget::new(),
    );

    assert_eq!(decision, AckDecision::Ack);
    assert_eq!(
        markers(&repo, &channel, ConversationType::AnonGroup),
        vec![SystemMessage::DeliveryIssue],
        "the group message is gone from the server; without an entry in the \
         channel the user sees an unexplained gap"
    );
}

/// The identified-sender path swallowed its `Err` the same way. A
/// `SessionNotFound` there is a message that arrived before the session
/// existed — deleting it is exactly the loss the DM loop's own test guards
/// against.
#[test]
fn an_identified_sender_message_with_no_session_yet_is_not_deleted() {
    use crate::inbox::ack_policy::RetryBudget;

    let (_d, repo) = repo();
    let peer = UserId::new();

    assert_eq!(
        super::device::device_failure_decision(
            Some(peer),
            1_777_000_000_000_000_000,
            &transient_error(),
            &repo,
            None,
            "envelope-id-1",
            &RetryBudget::new(),
        ),
        AckDecision::Retain,
        "the session can still be established and then these same bytes decrypt"
    );
}

/// ECIES init is the first contact, so there is no conversation to mark either
/// way — but failing to read OUR OWN identity key is our state, not their
/// bytes, and a later poll can succeed once registration has finished.
#[test]
fn an_ecies_init_we_could_not_open_for_want_of_our_own_key_is_not_deleted() {
    use crate::inbox::ack_policy::AckDecision as D;

    let err = storage_error();
    assert_eq!(
        super::device::ecies_init_decision(
            &super::device::EciesInitFailure::IdentityKeyUnavailable(&err)
        ),
        D::Retain,
        "our identity key being unreadable is our disk or an unfinished \
         registration; the stranger's contact request is fine and will open on \
         a later poll"
    );
}

/// The other half: a malformed ECIES envelope is a pure function of the bytes,
/// so retaining it replays forever.
#[test]
fn a_malformed_ecies_init_is_drained_rather_than_replayed_forever() {
    assert_eq!(
        super::device::ecies_init_decision(&super::device::EciesInitFailure::Malformed(
            "ECIES envelope would not parse"
        )),
        AckDecision::Ack,
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
        "envelope-dm-perm",
        &crate::inbox::ack_policy::RetryBudget::new(),
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
        "envelope-dm-identity",
        &crate::inbox::ack_policy::RetryBudget::new(),
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
            "envelope-dm-transient",
            &crate::inbox::ack_policy::RetryBudget::new(),
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
            "envelope-dm-storage",
            &crate::inbox::ack_policy::RetryBudget::new(),
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
        "envelope-grp-perm",
        &crate::inbox::ack_policy::RetryBudget::new(),
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
            "envelope-grp-transient",
            &crate::inbox::ack_policy::RetryBudget::new(),
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

/// An `AnonDmMessage` whose `inner_content` is absent carries nothing to
/// deliver — and nothing a later poll can supply, because the field is simply
/// not on the wire. The pre-fix loop answered `was_processed = false`, which
/// means: not marked processed, not acked, no marker. The server therefore
/// re-delivers it on every poll, and every poll re-runs the sealed-sender open
/// on it. One crafted envelope is a permanent decrypt loop on the victim, and
/// an unauthenticated stranger can send it: anon-DM mailboxes are exactly the
/// stranger-messaging surface.
#[test]
fn an_anon_dm_message_with_no_inner_content_cannot_be_left_to_redeliver_forever() {
    use crate::proto::content_v1::{self, AnonDmMessage, Content};

    let conversation = AnonDmId::new();
    let route = super::anon_dm::route_anon_dm_content(
        Content {
            content: Some(content_v1::content::Content::AnonDmMessage(Box::new(
                AnonDmMessage {
                    conversation_id: conversation.as_bytes().to_vec(),
                    inner_content: None,
                },
            ))),
            timestamp: 1_777_000_000_000_000_000,
            ephemeral_reply_to: None,
            padding: Vec::new(),
        },
        UserId::new(),
        conversation,
    );

    assert!(
        matches!(route, super::anon_dm::AnonDmRoute::Malformed(_)),
        "an absent inner_content is deterministic in the bytes, so it must reach \
         the policy as a permanent failure (marker, then drain). Leaving it \
         unacked pins a re-decrypt loop that a stranger can start at will; got \
         {route:?}"
    );
}

/// The anti-stacking story rests on the UNIQUE index over
/// `(conversation_type, conversation_id, sender_id, sent_at)`, and that index
/// is partial: `WHERE sender_id IS NOT NULL AND sent_at > 0`. Every anon-DM
/// marker and every group sealed-sender-open failure names nobody, so the index
/// never applies to them — and SQLite treats NULLs in a unique index as
/// distinct anyway. So the exact paths that most need the dedup are the ones
/// that never had it: a failed `acknowledge` replays the same envelope and
/// stacks another identical marker every time.
#[test]
fn a_replayed_envelope_with_no_authenticated_sender_does_not_stack_markers() {
    let (_d, repo) = repo();
    let conversation = AnonDmId::new();
    let ts = 1_777_000_000_000_000_000;

    for _ in 0..3 {
        super::anon_dm::anon_dm_failure_decision(
            conversation,
            ts,
            "sealed sender decrypt failed: unsupported envelope version",
            &repo,
            None,
        );
    }

    assert_eq!(
        markers(&repo, &conversation.into(), ConversationType::AnonDm).len(),
        1,
        "the marker names nobody, so the partial UNIQUE index does not cover it \
         and every replay of the same envelope wrote another copy"
    );
}

/// Distinct `sent_at` values defeat the index even where it does apply, so a
/// stranger who sends N undecryptable envelopes writes N markers into the
/// conversation. Anon-DM mailboxes accept mail from strangers, which makes that
/// a free way to bury a conversation under system entries.
#[test]
fn a_flood_of_undecryptable_envelopes_cannot_bury_the_conversation() {
    let (_d, repo) = repo();
    let conversation = AnonDmId::new();
    let base = 1_777_000_000_000_000_000u64;

    // 200 envelopes one second apart — a few minutes of a trivial flood.
    for i in 0..200u64 {
        super::anon_dm::anon_dm_failure_decision(
            conversation,
            base + i * 1_000_000_000,
            "sealed sender decrypt failed: unsupported envelope version",
            &repo,
            None,
        );
    }

    let count = markers(&repo, &conversation.into(), ConversationType::AnonDm).len();
    assert!(
        count <= 2,
        "one marker per hostile envelope means the attacker chooses how much of \
         the user's conversation is system noise; got {count} markers for 200 \
         envelopes spanning {} seconds",
        200,
    );
}

// ===========================================================================
// Retaining is not free — it must end
// ===========================================================================

/// Retain has no cursor, no attempt counter and no age cap on the mailbox
/// loops, so an envelope that keeps failing transiently is re-fetched and
/// re-decrypted on every poll for as long as the client runs. And the surface
/// is wide, by design: `classify_crypto` defaults to transient and
/// `CryptoError::Protocol` alone absorbs ~40 distinct libsignal causes, so
/// "transient" often means "unrecognised" rather than "will succeed later".
#[test]
fn an_envelope_that_never_stops_failing_eventually_stops_being_retried() {
    use crate::inbox::ack_policy::{self, MAX_RETAIN_ATTEMPTS, RetryBudget};

    let (_d, repo) = repo();
    let peer = UserId::new();
    let ts = 1_777_000_000_000_000_000;
    let budget = RetryBudget::new();
    // Age is not the trigger here: the envelope is treated as fresh every time.
    let now = ts + 1_000_000_000;

    // Once it gives up the envelope is acked, so the server deletes it and it
    // never comes back — stop at the first Ack, as production does.
    let mut gave_up_at = None;
    for attempt in 1..=(MAX_RETAIN_ATTEMPTS + 5) {
        let decision = ack_policy::decide_after_failure_bounded(
            &transient_error(),
            ack_policy::dm_target(peer, ts),
            &repo,
            None,
            "envelope-1",
            &budget,
            now,
        );
        if decision == AckDecision::Ack {
            gave_up_at = Some(attempt);
            break;
        }
    }

    assert_eq!(
        gave_up_at,
        Some(MAX_RETAIN_ATTEMPTS),
        "past {MAX_RETAIN_ATTEMPTS} deliveries of the same bytes the retain is \
         not a redelivery any more, it is an unbounded re-decrypt loop that \
         only ends when the process does"
    );
    assert_eq!(
        markers(&repo, &peer.into(), ConversationType::ServerDm),
        vec![SystemMessage::DeliveryIssue],
        "and giving up silently is the very thing the policy exists to prevent \
         — the marker is what makes draining it safe"
    );
}

/// The attempt cap is deliberately set above what a day of the fastest poll
/// interval produces, so on a slow or idle poller it is the elapsed time that
/// has to stop the loop.
#[test]
fn an_envelope_retried_for_a_whole_day_is_given_up_on() {
    use crate::inbox::ack_policy::{self, MAX_RETAIN_AGE_NS, RetryBudget};

    let (_d, repo) = repo();
    let peer = UserId::new();
    let ts = 1_777_000_000_000_000_000;
    let budget = RetryBudget::new();
    let started = ts + 5_000_000_000;

    assert_eq!(
        ack_policy::decide_after_failure_bounded(
            &transient_error(),
            ack_policy::dm_target(peer, ts),
            &repo,
            None,
            "envelope-2",
            &budget,
            started,
        ),
        AckDecision::Retain,
        "first delivery — nothing has had a chance yet"
    );

    assert_eq!(
        ack_policy::decide_after_failure_bounded(
            &transient_error(),
            ack_policy::dm_target(peer, ts),
            &repo,
            None,
            "envelope-2",
            &budget,
            started + MAX_RETAIN_AGE_NS + 1,
        ),
        AckDecision::Ack,
        "every transient cause the policy names resolves in seconds to minutes; \
         a day of retrying later, 'the SKDM may still land' is no longer a \
         description of anything"
    );
    assert_eq!(
        markers(&repo, &peer.into(), ConversationType::ServerDm),
        vec![SystemMessage::DeliveryIssue],
    );
}

/// The clock starts when WE first see the envelope, not when the sender sent
/// it. Billing the envelope's own age against the budget would delete a week of
/// legitimately-queued mail on its very first delivery, before the SKDM fetch
/// that opens it had run once — the exact loss this whole module exists to
/// prevent, reintroduced by the fix for the loop.
#[test]
fn a_long_queued_envelope_still_gets_its_full_retry_window() {
    use crate::inbox::ack_policy::{self, MAX_RETAIN_AGE_NS, RetryBudget};

    let (_d, repo) = repo();
    let peer = UserId::new();
    // Sent a week ago; we have been offline and are seeing it for the first
    // time right now.
    let sent_at = 1_777_000_000_000_000_000;
    let now = sent_at + 7 * 24 * MAX_RETAIN_AGE_NS / 24;

    assert_eq!(
        ack_policy::decide_after_failure_bounded(
            &transient_error(),
            ack_policy::dm_target(peer, sent_at),
            &repo,
            None,
            "envelope-old",
            &RetryBudget::new(),
            now,
        ),
        AckDecision::Retain,
        "a client back from a week offline must not delete the queue it just \
         downloaded because the messages in it are older than the retry window"
    );
    assert!(markers(&repo, &peer.into(), ConversationType::ServerDm).is_empty());
}

/// The bound must not fire early — that would be the original data-loss bug
/// with extra steps.
#[test]
fn a_fresh_transient_failure_is_still_retained() {
    use crate::inbox::ack_policy::{self, RetryBudget};

    let (_d, repo) = repo();
    let peer = UserId::new();
    let ts = 1_777_000_000_000_000_000;
    let budget = RetryBudget::new();

    assert_eq!(
        ack_policy::decide_after_failure_bounded(
            &transient_error(),
            ack_policy::dm_target(peer, ts),
            &repo,
            None,
            "envelope-3",
            &budget,
            ts + 1_000_000_000,
        ),
        AckDecision::Retain,
        "one second old, first delivery — the bootstrap has not had a chance yet"
    );
    assert!(
        markers(&repo, &peer.into(), ConversationType::ServerDm).is_empty(),
        "a message that will be redelivered must not also be reported as lost"
    );
}

/// An envelope that is acked must stop being tracked, or the budget is a leak
/// that grows with every message the client ever receives.
#[test]
fn settling_an_envelope_releases_its_budget_entry() {
    use crate::inbox::ack_policy::{self, RetryBudget};

    let (_d, repo) = repo();
    let peer = UserId::new();
    let ts = 1_777_000_000_000_000_000;
    let budget = RetryBudget::new();

    ack_policy::decide_after_failure_bounded(
        &transient_error(),
        ack_policy::dm_target(peer, ts),
        &repo,
        None,
        "envelope-4",
        &budget,
        ts + 1,
    );
    assert_eq!(budget.tracked(), 1);

    // The next delivery decrypts, or fails permanently — either way the loop
    // acks and the entry goes.
    budget.forget("envelope-4");
    assert_eq!(budget.tracked(), 0);
}

/// Replays must not stack markers: the marker is keyed to the envelope
/// timestamp so the UNIQUE index collapses repeats when `acknowledge` fails.
#[test]
fn a_replayed_envelope_does_not_stack_markers() {
    let (_d, repo) = repo();
    let peer = UserId::new();
    let ts = 1_777_000_000_000_000_000;

    for _ in 0..3 {
        super::dm::dm_failure_decision(
            peer,
            ts,
            &permanent_error(),
            &repo,
            None,
            "envelope-dm-replay",
            &crate::inbox::ack_policy::RetryBudget::new(),
        );
    }

    assert_eq!(
        markers(&repo, &peer.into(), ConversationType::ServerDm).len(),
        1,
        "a failed acknowledge replays the same envelope every start; one marker \
         per replay would spam the conversation"
    );
}
