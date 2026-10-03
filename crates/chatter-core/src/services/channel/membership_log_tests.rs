//! MembershipLog: authoring round-trips through the verifier, conflict
//! retry, gap fetch, joiner bootstrap, the asymmetric rule, and refusal of
//! far-future changes.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::test_support::{device, who, FakeServer, Who};
use super::*;
use crate::services::channel::verified_state::{ChannelRole, Policy, VerifiedMembership};

fn act(a: Action) -> pb::ChannelAction {
    pb::ChannelAction { action: Some(a) }
}

fn channel_id() -> ChannelId {
    ChannelId::from(uuid::Uuid::new_v4())
}

/// Author through `log`, submitting to the fake server.
async fn author(
    log: &MembershipLog,
    server: &Arc<FakeServer>,
    ch: &ChannelId,
    base: AuthorBase<'_>,
    acts: impl Fn(&IntentContext<'_>) -> Vec<pb::ChannelAction>,
) -> Result<u64, LogError> {
    log.author(ch, base, None, |ctx| Ok(acts(ctx)), |change| {
        let r = server.append(ch, change);
        async move { r }
    })
    .await
}

fn genesis() -> Vec<pb::ChannelAction> {
    vec![act(Action::Genesis(pb::Genesis { policy: Some(Policy::defaults(false).to_proto()), disappearing_seconds: 0 }))]
}

fn add(w: &Who, role: ChannelRole, pending: bool) -> pb::ChannelAction {
    act(Action::AddMember(pb::AddMember { user_id: w.id.clone(), role: role as i32, identity_key: w.key().to_vec(), pending }))
}

#[derive(Default)]
struct Seen {
    applied: parking_lot::Mutex<Vec<Applied>>,
    notices: parking_lot::Mutex<Vec<MembershipNotice>>,
}

impl LogObserver for Seen {
    fn applied(&self, a: &Applied) {
        self.applied.lock().push(a.clone());
    }
    fn notice(&self, _: &ChannelId, n: MembershipNotice) {
        self.notices.lock().push(n);
    }
}

fn observe(log: &MembershipLog) -> Arc<Seen> {
    let seen = Arc::new(Seen::default());
    log.set_observer(Arc::downgrade(&(seen.clone() as Arc<dyn LogObserver>)));
    seen
}

/// Everything the authoring path produces is accepted by the verifier on
/// another device: genesis, a direct invite, the invitee's own accept, a
/// role change, a kick, a transfer and a leave.
#[tokio::test]
async fn authored_changes_verify_on_every_device() {
    let (owner, bob, carol) = (who(1), who(2), who(3));
    let people = [&owner, &bob, &carol];
    let server = Arc::new(FakeServer::default());
    let ch = channel_id();
    let (o_log, o_backend, _) = device(&owner, &server, &people);
    let (b_log, _, _) = device(&bob, &server, &people);

    assert_eq!(author(&o_log, &server, &ch, AuthorBase::Genesis, |_| genesis()).await.unwrap(), 1);
    assert_eq!(o_log.load(&ch).unwrap().unwrap().revision(), 1, "own genesis committed locally");

    // Invite Bob with his KT-vouched key, then Carol directly... Carol by invite too.
    let bob_key = o_log.vouched_identity_key(&bob.id).await.unwrap();
    assert_eq!(bob_key, bob.key());
    author(&o_log, &server, &ch, AuthorBase::Verified, |_| vec![add(&bob, ChannelRole::Member, true)]).await.unwrap();

    // Bob has no state: the relayed invite names him, so he bootstraps from N-1.
    let invite = server.log.lock()[1].clone();
    b_log.ingest(&ch, &invite).await.unwrap();
    assert_eq!(b_log.load(&ch).unwrap().unwrap().state.members[&bob.id].status, MemberStatus::Invited);

    // Bob accepts with his own signature.
    author(&b_log, &server, &ch, AuthorBase::Verified, |_| vec![act(Action::AcceptInvite(pb::AcceptInvite {}))])
        .await
        .unwrap();
    o_log.catch_up(&ch).await.unwrap();
    assert!(o_log.load(&ch).unwrap().unwrap().contains(&bob.id));

    // Promote, invite Carol, Carol accepts, Bob (admin) kicks Carol.
    author(&o_log, &server, &ch, AuthorBase::Verified, |_| {
        vec![act(Action::SetRole(pb::SetRole { user_id: bob.id.clone(), role: ChannelRole::Admin as i32 }))]
    })
    .await
    .unwrap();
    author(&o_log, &server, &ch, AuthorBase::Verified, |_| vec![add(&carol, ChannelRole::Member, true)]).await.unwrap();
    let (c_log, _, _) = device(&carol, &server, &people);
    author(&c_log, &server, &ch, AuthorBase::Verified, |_| vec![act(Action::AcceptInvite(pb::AcceptInvite {}))])
        .await
        .unwrap();
    author(&b_log, &server, &ch, AuthorBase::Verified, |_| {
        vec![act(Action::RemoveMember(pb::RemoveMember { user_id: carol.id.clone(), reason: pb::remove_member::Reason::Kicked as i32 }))]
    })
    .await
    .unwrap();

    // Owner catches up: Carol is out, and Owner must rotate (she held our key).
    let rotations_before = o_backend.rotations.lock().len();
    o_log.catch_up(&ch).await.unwrap();
    let o_state = o_log.load(&ch).unwrap().unwrap();
    assert!(!o_state.state.members.contains_key(&carol.id));
    assert!(o_backend.rotations.lock().len() > rotations_before, "a verified removal marks rotation");

    // Transfer, then the former owner leaves.
    author(&o_log, &server, &ch, AuthorBase::Verified, |_| {
        vec![act(Action::TransferOwnership(pb::TransferOwnershipAction { new_owner_user_id: bob.id.clone() }))]
    })
    .await
    .unwrap();
    author(&o_log, &server, &ch, AuthorBase::Verified, |ctx| {
        vec![act(Action::RemoveMember(pb::RemoveMember { user_id: ctx.me.to_string(), reason: pb::remove_member::Reason::Left as i32 }))]
    })
    .await
    .unwrap();

    // Every device converges on the server's head, hash for hash.
    b_log.catch_up(&ch).await.unwrap();
    c_log.catch_up(&ch).await.unwrap();
    let head = server.head().unwrap();
    for log in [&o_log, &b_log] {
        let v = log.load(&ch).unwrap().unwrap();
        assert_eq!(v.state, head);
        assert_eq!(v.status, VerificationStatus::Ok);
    }
    assert_eq!(head.members[&bob.id].role, ChannelRole::Owner);
    assert!(head.has_one_owner());
}

/// A submit that loses the race is refused as ABORTED; the author catches
/// up, re-runs its intent against the newer state, re-signs and succeeds.
#[tokio::test]
async fn a_conflict_catches_up_and_re_signs() {
    let (owner, bob, carol) = (who(1), who(2), who(3));
    let people = [&owner, &bob, &carol];
    let server = Arc::new(FakeServer::default());
    let ch = channel_id();
    let (o_log, _, _) = device(&owner, &server, &people);
    let (b_log, _, _) = device(&bob, &server, &people);
    author(&o_log, &server, &ch, AuthorBase::Genesis, |_| genesis()).await.unwrap();
    author(&o_log, &server, &ch, AuthorBase::Verified, |_| vec![add(&bob, ChannelRole::Admin, true)]).await.unwrap();
    b_log.catch_up(&ch).await.unwrap();
    // Bob signs his accept now; it reaches the server only while the owner's
    // own change is in flight, so the owner loses the race.
    let held = parking_lot::Mutex::new(None);
    b_log
        .author(&ch, AuthorBase::Verified, None, |_| Ok(vec![act(Action::AcceptInvite(pb::AcceptInvite {}))]), |c| {
            *held.lock() = Some(c);
            async { Ok(()) }
        })
        .await
        .unwrap();
    let racing = held.lock().take().unwrap();

    let before = server.submits.load(Ordering::SeqCst);
    let seen_revisions = parking_lot::Mutex::new(Vec::new());
    let rev = o_log
        .author(
            &ch,
            AuthorBase::Verified,
            None,
            |ctx| {
                seen_revisions.lock().push(ctx.state.unwrap().revision);
                Ok(vec![add(&carol, ChannelRole::Member, true)])
            },
            |change| {
                if server.log.lock().len() == 2 {
                    server.append(&ch, racing.clone()).unwrap();
                }
                let r = server.append(&ch, change);
                async move { r }
            },
        )
        .await
        .unwrap();
    assert_eq!(rev, 4);
    assert_eq!(server.submits.load(Ordering::SeqCst) - before, 3, "racing accept, our refused submit, our retry");
    assert_eq!(*seen_revisions.lock(), vec![2, 3], "intent re-run against the caught-up state");
    assert_eq!(o_log.load(&ch).unwrap().unwrap().revision(), 4);
}

/// Retries are bounded: a server that always conflicts ends in an error.
#[tokio::test]
async fn conflict_retries_are_bounded() {
    let owner = who(1);
    let server = Arc::new(FakeServer::default());
    let ch = channel_id();
    let (o_log, _, _) = device(&owner, &server, &[&owner]);
    author(&o_log, &server, &ch, AuthorBase::Genesis, |_| genesis()).await.unwrap();
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let res: Result<(), _> = o_log
        .author(
            &ch,
            AuthorBase::Verified,
            None,
            |_| Ok(vec![act(Action::SetDisappearing(pb::SetDisappearing { seconds: 60 }))]),
            |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err(NetworkError::ServerError("aborted: change conflict".into())) }
            },
        )
        .await;
    assert!(res.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), MAX_SUBMIT_ATTEMPTS);
}

/// A relayed change past local + 1 fetches the missing ones in order.
#[tokio::test]
async fn a_gap_fetches_the_missing_changes() {
    let (owner, bob) = (who(1), who(2));
    let people = [&owner, &bob];
    let server = Arc::new(FakeServer::default());
    let ch = channel_id();
    let (o_log, _, _) = device(&owner, &server, &people);
    let (b_log, _, _) = device(&bob, &server, &people);
    author(&o_log, &server, &ch, AuthorBase::Genesis, |_| genesis()).await.unwrap();
    author(&o_log, &server, &ch, AuthorBase::Verified, |_| vec![add(&bob, ChannelRole::Member, true)]).await.unwrap();
    let c = server.log.lock()[1].clone();
    b_log.ingest(&ch, &c).await.unwrap();
    for s in [60, 120, 180] {
        author(&o_log, &server, &ch, AuthorBase::Verified, |_| {
            vec![act(Action::SetDisappearing(pb::SetDisappearing { seconds: s }))]
        })
        .await
        .unwrap();
    }
    assert_eq!(b_log.load(&ch).unwrap().unwrap().revision(), 2);

    // Bob hears only revision 5.
    let c = server.log.lock()[4].clone();
    b_log.ingest(&ch, &c).await.unwrap();
    let v = b_log.load(&ch).unwrap().unwrap();
    assert_eq!(v.revision(), 5);
    assert_eq!(v.state.disappearing_seconds, 180);
    // Hearing an older one again is a duplicate, not a fork.
    let c = server.log.lock()[2].clone();
    b_log.ingest(&ch, &c).await.unwrap();
    assert_eq!(b_log.load(&ch).unwrap().unwrap().status, VerificationStatus::Ok);
}

/// A change that fails verification is never applied: the channel is
/// marked VerificationFailed (send-blocked) and the user is told.
#[tokio::test]
async fn a_forged_change_is_refused_and_surfaced() {
    let (owner, mallory) = (who(1), who(9));
    let server = Arc::new(FakeServer::default());
    let ch = channel_id();
    let (o_log, _, _) = device(&owner, &server, &[&owner]);
    let seen = observe(&o_log);
    author(&o_log, &server, &ch, AuthorBase::Genesis, |_| genesis()).await.unwrap();
    let state = o_log.load(&ch).unwrap().unwrap().state;

    // The server relays a change "by the owner" signed with a key that is
    // not the owner's KT key, adding an admin ghost.
    let forged = {
        let cid = ch.to_string();
        let acts = vec![add(&mallory, ChannelRole::Admin, false)];
        let new = rules::preview(Some(&state), &acts, &cid, 2, &owner.id, mallory.key(), now_nanos()).unwrap();
        let a = pb::channel_change::Actions {
            channel_id: cid,
            revision: 2,
            prev_state_hash: state.hash().to_vec(),
            author_user_id: owner.id.clone(),
            author_identity_key: mallory.key().to_vec(),
            timestamp: now_nanos(),
            actions: acts,
            new_state_hash: new.hash().to_vec(),
        };
        let (actions, signature) = crate::crypto::signal::identity::sign_channel_change(&mallory.kp, &a).unwrap();
        pb::ChannelChange { actions, signature, invite_proof: Vec::new() }
    };
    assert!(matches!(o_log.ingest(&ch, &forged).await, Err(LogError::Verify(VerifyError::KtMismatch))));
    let v = o_log.load(&ch).unwrap().unwrap();
    assert_eq!(v.revision(), 1, "nothing applied");
    assert!(!v.contains(&mallory.id));
    assert_eq!(v.status, VerificationStatus::VerificationFailed);
    assert!(matches!(seen.notices.lock().as_slice(), [MembershipNotice::VerificationFailed { .. }]));

    // The next genuine change clears the failure, and says so: the banner
    // and send block it explains refresh on the notice.
    let bob = who(2);
    author(&o_log, &server, &ch, AuthorBase::Verified, |_| vec![add(&bob, ChannelRole::Member, true)]).await.unwrap();
    assert_eq!(o_log.load(&ch).unwrap().unwrap().status, VerificationStatus::Ok);
    assert!(matches!(seen.notices.lock().last(), Some(MembershipNotice::Verified)));
}

/// The author's clock is trusted only so far: a change dated more than
/// MAX_FUTURE_SKEW ahead is refused.
#[tokio::test]
async fn a_far_future_change_is_refused() {
    let owner = who(1);
    let server = Arc::new(FakeServer::default());
    let ch = channel_id();
    let (o_log, _, _) = device(&owner, &server, &[&owner]);
    let (other, _, _) = device(&owner, &server, &[&owner]);
    author(&o_log, &server, &ch, AuthorBase::Genesis, |_| genesis()).await.unwrap();
    other.catch_up(&ch).await.unwrap();
    let state = o_log.load(&ch).unwrap().unwrap().state;
    let future = now_nanos() + 2 * MAX_FUTURE_SKEW.as_nanos() as u64;
    let cid = ch.to_string();
    let acts = vec![act(Action::SetDisappearing(pb::SetDisappearing { seconds: 5 }))];
    let new = rules::preview(Some(&state), &acts, &cid, 2, &owner.id, owner.key(), future).unwrap();
    let a = pb::channel_change::Actions {
        channel_id: cid,
        revision: 2,
        prev_state_hash: state.hash().to_vec(),
        author_user_id: owner.id.clone(),
        author_identity_key: owner.key().to_vec(),
        timestamp: future,
        actions: acts,
        new_state_hash: new.hash().to_vec(),
    };
    let (actions, signature) = crate::crypto::signal::identity::sign_channel_change(&owner.kp, &a).unwrap();
    let change = pb::ChannelChange { actions, signature, invite_proof: Vec::new() };
    assert!(matches!(other.ingest(&ch, &change).await, Err(LogError::Verify(_))));
    assert_eq!(other.load(&ch).unwrap().unwrap().revision(), 1);
}

/// The asymmetric rule on roster facts only the server vouches for.
#[tokio::test]
async fn unsigned_roster_facts_follow_the_asymmetric_rule() {
    let (owner, bob, ghost) = (who(1), who(2), who(66));
    let people = [&owner, &bob];
    let server = Arc::new(FakeServer::default());
    let ch = channel_id();
    let (o_log, _, _) = device(&owner, &server, &people);
    let (b_log, _, _) = device(&bob, &server, &people);
    let seen = observe(&o_log);
    author(&o_log, &server, &ch, AuthorBase::Genesis, |_| genesis()).await.unwrap();
    author(&o_log, &server, &ch, AuthorBase::Verified, |_| vec![add(&bob, ChannelRole::Member, true)]).await.unwrap();
    b_log.catch_up(&ch).await.unwrap();
    author(&b_log, &server, &ch, AuthorBase::Verified, |_| vec![act(Action::AcceptInvite(pb::AcceptInvite {}))])
        .await
        .unwrap();
    o_log.catch_up(&ch).await.unwrap();

    // The server says a ghost joined: ignored for keys, surfaced.
    assert_eq!(
        o_log.apply_unsigned(&ch, UnsignedEvent::Added { user_id: ghost.id.clone() }).unwrap(),
        UnsignedDisposition::IgnoreAndSurface
    );
    let v = o_log.load(&ch).unwrap().unwrap();
    assert!(!v.contains(&ghost.id));
    assert!(!v.members().contains(&ghost.id));

    // The server says Bob left, unsigned: out of the distribution set now.
    o_log.apply_unsigned(&ch, UnsignedEvent::Removed { user_id: bob.id.clone() }).unwrap();
    let v = o_log.load(&ch).unwrap().unwrap();
    assert!(!v.contains(&bob.id), "unsigned removal is fail-safe");
    assert!(v.state.members.contains_key(&bob.id), "still in the verified state, pending a signature");
    assert_eq!(
        *seen.notices.lock(),
        vec![
            MembershipNotice::UnverifiedMember { user_id: ghost.id.clone() },
            MembershipNotice::RemovedByServer { user_id: bob.id.clone() },
        ]
    );
}

/// Join links and password capabilities: the joiner signs JoinViaInvite on
/// the server's state with the capability's proof; members verify it.
#[tokio::test]
async fn link_and_password_joins_verify() {
    let (owner, joiner) = (who(1), who(4));
    let people = [&owner, &joiner];
    let server = Arc::new(FakeServer::default());
    let ch = channel_id();
    let (o_log, _, _) = device(&owner, &server, &people);
    let (j_log, _, _) = device(&joiner, &server, &people);
    author(&o_log, &server, &ch, AuthorBase::Genesis, |_| genesis()).await.unwrap();

    let link = InviteLink::generate(ch);
    let parsed = InviteLink::parse(&link.to_url()).unwrap();
    assert_eq!(parsed.invite_pub(), link.invite_pub());
    let pw = password_invite_key(&ch, "correct horse").unwrap();
    assert_eq!(invite_pub_of(&pw), invite_pub_of(&password_invite_key(&ch, "correct horse").unwrap()));
    assert_ne!(invite_pub_of(&pw), invite_pub_of(&password_invite_key(&channel_id(), "correct horse").unwrap()));

    author(&o_log, &server, &ch, AuthorBase::Verified, |_| {
        vec![act(Action::CreateInvite(pb::CreateInvite {
            capability: Some(pb::InviteCapability {
                invite_pub: invite_pub_of(&pw).to_vec(),
                role: ChannelRole::Member as i32,
                ..Default::default()
            }),
        }))]
    })
    .await
    .unwrap();

    let pub_ = invite_pub_of(&pw).to_vec();
    j_log
        .author(
            &ch,
            AuthorBase::Server { invite_pub: &pub_ },
            Some(&pw),
            |_| Ok(vec![act(Action::JoinViaInvite(pb::JoinViaInvite { invite_pub: pub_.clone() }))]),
            |change| {
                let r = server.append(&ch, change);
                async move { r }
            },
        )
        .await
        .unwrap();
    assert!(j_log.load(&ch).unwrap().unwrap().contains(&joiner.id), "joiner bootstrapped from its own join");
    o_log.catch_up(&ch).await.unwrap();
    assert!(o_log.load(&ch).unwrap().unwrap().contains(&joiner.id));
}

/// A device with no state that hears a change not about itself replays the
/// log from genesis, as backfill (no notices).
#[tokio::test]
async fn no_state_and_not_named_replays_from_genesis() {
    let (owner, bob) = (who(1), who(2));
    let people = [&owner, &bob];
    let server = Arc::new(FakeServer::default());
    let ch = channel_id();
    let (o_log, _, _) = device(&owner, &server, &people);
    author(&o_log, &server, &ch, AuthorBase::Genesis, |_| genesis()).await.unwrap();
    author(&o_log, &server, &ch, AuthorBase::Verified, |_| {
        vec![act(Action::SetDisappearing(pb::SetDisappearing { seconds: 9 }))]
    })
    .await
    .unwrap();
    let (fresh, _, _) = device(&owner, &server, &people); // a reinstall
    let seen = observe(&fresh);
    let c = server.log.lock()[1].clone();
    fresh.ingest(&ch, &c).await.unwrap();
    assert_eq!(fresh.load(&ch).unwrap().unwrap().state, server.head().unwrap());
    assert!(seen.applied.lock().iter().all(|a| a.backfill));
}
