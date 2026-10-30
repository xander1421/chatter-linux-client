//! Verifier tests: hand-built chains signed with fresh keys, plus the golden
//! vectors shared with the server verifier.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use libsignal_protocol::{KeyPair, PrivateKey};
use prost::Message;

use super::canonical::{
    CHANGE_DOMAIN, INVITE_DOMAIN, STATE_DOMAIN, canonical_state, signed_message,
};
use super::rules::preview;
use super::*;
use crate::proto::chat_v1 as pb;
use pb::channel_action::Action;
use pb::remove_member::Reason;

const CH: &str = "0b5e1c1e-7c2a-4a43-9d7e-0000000000aa";
const TS: u64 = 1_790_000_000_000_000_000;

struct Who {
    id: String,
    kp: KeyPair,
}

fn who(n: u32) -> Who {
    Who {
        id: format!("{n:08x}-0000-4000-8000-{n:012x}"),
        kp: KeyPair::generate(&mut rand::rng()),
    }
}

fn key(kp: &KeyPair) -> KeyBytes {
    kp.public_key.serialize().as_ref().try_into().unwrap()
}

fn sign(kp: &KeyPair, domain: &[u8], bytes: &[u8]) -> Vec<u8> {
    kp.private_key
        .calculate_signature(&signed_message(domain, bytes), &mut rand::rng())
        .unwrap()
        .to_vec()
}

fn act(a: Action) -> pb::ChannelAction {
    pb::ChannelAction { action: Some(a) }
}

fn add(w: &Who, role: ChannelRole, pending: bool) -> Action {
    Action::AddMember(pb::AddMember {
        user_id: w.id.clone(),
        role: role as i32,
        identity_key: key(&w.kp).to_vec(),
        pending,
    })
}

fn remove(w: &Who, reason: Reason) -> Action {
    Action::RemoveMember(pb::RemoveMember {
        user_id: w.id.clone(),
        reason: reason as i32,
    })
}

fn set_role(w: &Who, role: ChannelRole) -> Action {
    Action::SetRole(pb::SetRole {
        user_id: w.id.clone(),
        role: role as i32,
    })
}

fn genesis(public: bool) -> Action {
    Action::Genesis(pb::Genesis {
        policy: Some(Policy::defaults(public).to_proto()),
        disappearing_seconds: 0,
    })
}

/// Raw Actions for `by` on top of `prev`, committing to `new_hash`.
fn raw_actions(
    prev: Option<&MembershipState>,
    by: &Who,
    acts: Vec<Action>,
    ts: u64,
    new_hash: Vec<u8>,
) -> pb::channel_change::Actions {
    pb::channel_change::Actions {
        channel_id: CH.into(),
        revision: prev.map_or(1, |p| p.revision + 1),
        prev_state_hash: prev.map(|p| p.hash().to_vec()).unwrap_or_default(),
        author_user_id: by.id.clone(),
        author_identity_key: key(&by.kp).to_vec(),
        timestamp: ts,
        actions: acts.into_iter().map(act).collect(),
        new_state_hash: new_hash,
    }
}

fn signed(a: &pb::channel_change::Actions, signer: &KeyPair) -> pb::ChannelChange {
    let bytes = a.encode_to_vec();
    pb::ChannelChange {
        signature: sign(signer, CHANGE_DOMAIN, &bytes),
        actions: bytes,
        invite_proof: Vec::new(),
    }
}

/// An honestly authored change (the author previews the result and commits).
fn author(prev: Option<&MembershipState>, by: &Who, acts: Vec<Action>) -> pb::ChannelChange {
    author_at(prev, by, acts, TS)
}

fn author_at(
    prev: Option<&MembershipState>,
    by: &Who,
    acts: Vec<Action>,
    ts: u64,
) -> pb::ChannelChange {
    let revision = prev.map_or(1, |p| p.revision + 1);
    let actions: Vec<_> = acts.iter().cloned().map(act).collect();
    let new =
        preview(prev, &actions, CH, revision, &by.id, key(&by.kp), ts).expect("author preview");
    signed(
        &raw_actions(prev, by, acts, ts, new.hash().to_vec()),
        &by.kp,
    )
}

fn ok(prev: Option<&MembershipState>, change: &pb::ChannelChange) -> MembershipState {
    check_change(CH, prev, change)
        .expect("change verifies")
        .new_state
}

fn err(prev: Option<&MembershipState>, change: &pb::ChannelChange) -> &'static str {
    check_change(CH, prev, change)
        .expect_err("change must be rejected")
        .code()
}

/// Preview the change, ignoring the author's own preview failure, so a
/// change the rules reject can still be signed and sent to the verifier.
fn forced(prev: Option<&MembershipState>, by: &Who, acts: Vec<Action>) -> pb::ChannelChange {
    signed(&raw_actions(prev, by, acts, TS, vec![0; 32]), &by.kp)
}

struct Cast {
    owner: Who,
    admin: Who,
    moderator: Who,
    member: Who,
    outsider: Who,
}

/// Genesis by `owner`, then admin / moderator / member added.
fn channel() -> (Cast, MembershipState) {
    let c = Cast {
        owner: who(1),
        admin: who(2),
        moderator: who(3),
        member: who(4),
        outsider: who(5),
    };
    let s1 = ok(
        None,
        &author(
            None,
            &c.owner,
            vec![
                genesis(false),
                add(&c.admin, ChannelRole::Admin, false),
                add(&c.moderator, ChannelRole::Moderator, false),
                add(&c.member, ChannelRole::Member, false),
            ],
        ),
    );
    (c, s1)
}

// ------------------------------------------------------------------ canonical

#[test]
fn canonical_encoding_is_the_specified_layout() {
    let owner = [0x05u8; 33];
    let mut s = MembershipState {
        channel_id: CH.into(),
        revision: 7,
        members: Default::default(),
        policy: Policy::defaults(false),
        invites: Default::default(),
        disappearing_seconds: 3600,
    };
    s.members.insert(
        "bbbbbbbb-0000-4000-8000-000000000000".into(),
        Member {
            role: ChannelRole::Member,
            status: MemberStatus::Invited,
            identity_key: owner,
        },
    );
    s.members.insert(
        "aaaaaaaa-0000-4000-8000-000000000000".into(),
        Member {
            role: ChannelRole::Owner,
            status: MemberStatus::Active,
            identity_key: owner,
        },
    );
    s.invites.insert(
        [0x05; 33],
        Invite {
            role: ChannelRole::Member,
            expires_at: 0x0102,
            max_uses: 3,
            uses: 1,
            needs_approval: true,
        },
    );

    let mut want = vec![1u8];
    want.extend(36u32.to_be_bytes());
    want.extend(CH.as_bytes());
    want.extend(7u64.to_be_bytes());
    want.extend([3, 2, 3, 3, 3, 1]);
    want.extend(3600u32.to_be_bytes());
    want.extend(2u32.to_be_bytes());
    for (id, role, status) in [
        ("aaaaaaaa-0000-4000-8000-000000000000", 4u8, 1u8),
        ("bbbbbbbb-0000-4000-8000-000000000000", 1, 2),
    ] {
        want.extend(36u32.to_be_bytes());
        want.extend(id.as_bytes());
        want.extend([role, status]);
        want.extend(33u32.to_be_bytes());
        want.extend(owner);
    }
    want.extend(1u32.to_be_bytes());
    want.extend(33u32.to_be_bytes());
    want.extend([0x05; 33]);
    want.push(1);
    want.extend(0x0102u64.to_be_bytes());
    want.extend(3u32.to_be_bytes());
    want.extend(1u32.to_be_bytes());
    want.push(1);
    assert_eq!(canonical_state(&s), want);

    use sha2::{Digest, Sha256};
    let h: [u8; 32] = Sha256::digest([STATE_DOMAIN, &want].concat()).into();
    assert_eq!(s.hash(), h);
}

#[test]
fn received_states_are_validated() {
    let (_, s1) = channel();
    let good = s1.to_proto();
    assert_eq!(MembershipState::from_proto(&good).unwrap(), s1);

    // Order of the received list does not matter: the encoding sorts.
    let mut shuffled = good.clone();
    shuffled.members.reverse();
    assert_eq!(
        MembershipState::from_proto(&shuffled).unwrap().hash(),
        s1.hash()
    );

    let mut dup = good.clone();
    dup.members.push(dup.members[0].clone());
    assert!(
        MembershipState::from_proto(&dup).is_err(),
        "duplicate member"
    );

    let mut simple = good.clone();
    simple.members[0].user_id = simple.members[0].user_id.replace('-', "");
    assert!(
        MembershipState::from_proto(&simple).is_err(),
        "non-canonical uuid"
    );
    let mut upper = good.clone();
    upper.channel_id = "0B5E1C1E-7C2A-4A43-9D7E-0000000000AA".into();
    assert!(
        MembershipState::from_proto(&upper).is_err(),
        "upper-case uuid"
    );

    let mut bad_role = good.clone();
    bad_role.members[0].role = 9;
    assert!(
        MembershipState::from_proto(&bad_role).is_err(),
        "unknown role"
    );

    let mut bad_status = good.clone();
    bad_status.members[0].status = 0;
    assert!(
        MembershipState::from_proto(&bad_status).is_err(),
        "unspecified status"
    );

    let mut no_policy = good.clone();
    no_policy.policy = None;
    assert!(MembershipState::from_proto(&no_policy).is_err());

    let mut short_key = good;
    short_key.members[0].identity_key.pop();
    assert!(MembershipState::from_proto(&short_key).is_err());
}

/// A state has exactly one OWNER, ACTIVE (review fix): a server-supplied
/// bootstrap state with none or two is refused, and so is a change whose
/// result would have none or two.
#[test]
fn a_state_has_exactly_one_owner() {
    let (c, s1) = channel();
    assert!(s1.has_one_owner());

    let mut two = s1.to_proto();
    let admin = two
        .members
        .iter_mut()
        .find(|m| m.user_id == c.admin.id)
        .unwrap();
    admin.role = ChannelRole::Owner as i32;
    assert!(MembershipState::from_proto(&two).is_err(), "two owners");

    let mut none = s1.to_proto();
    none.members.retain(|m| m.user_id != c.owner.id);
    assert!(MembershipState::from_proto(&none).is_err(), "no owner");

    // A bootstrap whose N-1 hides a second owner fails before N is looked at.
    let mut forged = s1.clone();
    forged.members.get_mut(&c.admin.id).unwrap().role = ChannelRole::Owner;
    let n = author(
        Some(&s1),
        &c.admin,
        vec![add(&c.outsider, ChannelRole::Member, false)],
    );
    assert_eq!(
        check_change(CH, Some(&forged), &n).unwrap_err().code(),
        "wrong_prev_hash",
        "doctored state cannot chain"
    );
    let forged_n = forced(
        Some(&forged),
        &c.owner,
        vec![add(&c.outsider, ChannelRole::Member, false)],
    );
    assert_eq!(
        err(Some(&forged), &forged_n),
        "invalid_action",
        "a two-owner result is refused"
    );
}

// ---------------------------------------------------------------------- chain

#[test]
fn genesis_makes_the_author_owner() {
    let (c, s1) = channel();
    assert_eq!(s1.revision, 1);
    assert_eq!(s1.members[&c.owner.id].role, ChannelRole::Owner);
    assert_eq!(s1.members.len(), 4);
    assert_eq!(s1.policy, Policy::defaults(false));
}

#[test]
fn chain_position_is_enforced() {
    let (c, s1) = channel();
    let s2 = ok(
        Some(&s1),
        &author(Some(&s1), &c.owner, vec![remove(&c.member, Reason::Kicked)]),
    );
    let next = author(
        Some(&s2),
        &c.owner,
        vec![remove(&c.moderator, Reason::Kicked)],
    );

    // From s1, revision 3 is a gap; replaying revision 2 on s2 is stale.
    assert!(matches!(
        check_change(CH, Some(&s1), &next),
        Err(VerifyError::Gap {
            expected: 2,
            got: 3
        })
    ));
    let replay = author(Some(&s1), &c.owner, vec![remove(&c.member, Reason::Kicked)]);
    assert!(matches!(
        check_change(CH, Some(&s2), &replay),
        Err(VerifyError::Stale { local: 2, got: 2 })
    ));
    assert!(
        !VerifyError::Gap {
            expected: 1,
            got: 2
        }
        .is_failure()
    );

    // Right revision, wrong predecessor.
    let mut forked = s1.clone();
    forked.disappearing_seconds = 60;
    forked.revision = 2;
    assert_eq!(err(Some(&forked), &next), "wrong_prev_hash");

    // Another channel.
    assert_eq!(
        check_change("0b5e1c1e-7c2a-4a43-9d7e-0000000000bb", Some(&s2), &next)
            .unwrap_err()
            .code(),
        "wrong_channel"
    );

    // Genesis carries no prev hash and needs no state.
    let mut g = raw_actions(None, &c.owner, vec![genesis(false)], TS, vec![]);
    g.prev_state_hash = vec![1; 32];
    assert_eq!(err(None, &signed(&g, &c.owner.kp)), "wrong_prev_hash");
    assert_eq!(
        err(
            None,
            &forced(
                None,
                &c.owner,
                vec![add(&c.member, ChannelRole::Member, false)]
            )
        ),
        "invalid_action"
    );
    assert_eq!(
        err(
            Some(&s1),
            &forced(Some(&s1), &c.owner, vec![genesis(false)])
        ),
        "invalid_action"
    );
}

#[test]
fn signatures_cover_the_exact_bytes_under_the_author_key() {
    let (c, s1) = channel();
    let good = author(
        Some(&s1),
        &c.admin,
        vec![add(&c.outsider, ChannelRole::Member, false)],
    );
    assert!(check_change(CH, Some(&s1), &good).is_ok());

    let mut by_other = good.clone();
    by_other.signature = sign(&c.outsider.kp, CHANGE_DOMAIN, &good.actions);
    assert_eq!(err(Some(&s1), &by_other), "bad_signature");

    let mut wrong_domain = good.clone();
    wrong_domain.signature = sign(&c.admin.kp, INVITE_DOMAIN, &good.actions);
    assert_eq!(err(Some(&s1), &wrong_domain), "bad_signature");

    let mut unsigned = good.clone();
    unsigned.signature.clear();
    assert_eq!(err(Some(&s1), &unsigned), "bad_signature");

    // Appending an unknown field changes the bytes, so the signature fails,
    // even though the parsed meaning would be identical.
    let mut extended = good.clone();
    extended.actions.extend([0xF8, 0x01, 0x01]); // field 31, varint 1
    assert_eq!(err(Some(&s1), &extended), "bad_signature");

    // The author's commitment must be the actual result.
    let a = raw_actions(
        Some(&s1),
        &c.admin,
        vec![add(&c.outsider, ChannelRole::Member, false)],
        TS,
        s1.hash().to_vec(),
    );
    assert_eq!(
        err(Some(&s1), &signed(&a, &c.admin.kp)),
        "wrong_new_state_hash"
    );
}

// --------------------------------------------------------------------- policy

#[test]
fn ghost_injection_is_refused() {
    let (c, s1) = channel();
    // An account the server controls, legitimately in KT, adding itself.
    let ghost = &c.outsider;
    assert_eq!(
        err(
            Some(&s1),
            &forced(
                Some(&s1),
                ghost,
                vec![add(ghost, ChannelRole::Admin, false)]
            )
        ),
        "role_not_permitted"
    );
    // Self-join of an invite-only channel without a link.
    let join = Action::JoinViaInvite(pb::JoinViaInvite { invite_pub: vec![] });
    assert_eq!(
        err(Some(&s1), &forced(Some(&s1), ghost, vec![join])),
        "role_not_permitted"
    );
    // A MEMBER adding the ghost.
    assert_eq!(
        err(
            Some(&s1),
            &forced(
                Some(&s1),
                &c.member,
                vec![add(ghost, ChannelRole::Member, false)]
            )
        ),
        "role_not_permitted"
    );
}

#[test]
fn default_policy_roles() {
    let (c, s1) = channel();
    let s = Some(&s1);
    // add: ADMIN+, never above own role, never OWNER.
    assert_eq!(
        err(
            s,
            &forced(
                s,
                &c.moderator,
                vec![add(&c.outsider, ChannelRole::Member, false)]
            )
        ),
        "role_not_permitted"
    );
    assert!(
        check_change(
            CH,
            s,
            &author(
                s,
                &c.admin,
                vec![add(&c.outsider, ChannelRole::Admin, false)]
            )
        )
        .is_ok()
    );
    assert_eq!(
        err(
            s,
            &forced(
                s,
                &c.admin,
                vec![add(&c.outsider, ChannelRole::Owner, false)]
            )
        ),
        "role_not_permitted"
    );
    assert_eq!(
        err(
            s,
            &forced(
                s,
                &c.owner,
                vec![add(&c.member, ChannelRole::Member, false)]
            )
        ),
        "invalid_action"
    );

    // remove others: MODERATOR+, strictly above the target.
    assert!(
        check_change(
            CH,
            s,
            &author(s, &c.moderator, vec![remove(&c.member, Reason::Kicked)])
        )
        .is_ok()
    );
    assert_eq!(
        err(
            s,
            &forced(s, &c.moderator, vec![remove(&c.admin, Reason::Kicked)])
        ),
        "role_not_permitted"
    );
    assert_eq!(
        err(
            s,
            &forced(s, &c.member, vec![remove(&c.moderator, Reason::Banned)])
        ),
        "role_not_permitted"
    );
    assert_eq!(
        err(
            s,
            &forced(s, &c.admin, vec![remove(&c.member, Reason::Left)])
        ),
        "malformed"
    );
    assert_eq!(
        err(
            s,
            &forced(s, &c.admin, vec![remove(&c.outsider, Reason::Kicked)])
        ),
        "invalid_action"
    );

    // self-remove: anyone but the owner, and only as LEFT.
    let left = ok(
        s,
        &author(s, &c.member, vec![remove(&c.member, Reason::Left)]),
    );
    assert!(!left.members.contains_key(&c.member.id));
    assert_eq!(
        err(
            s,
            &forced(s, &c.member, vec![remove(&c.member, Reason::Kicked)])
        ),
        "malformed"
    );
    assert_eq!(
        err(
            s,
            &forced(s, &c.owner, vec![remove(&c.owner, Reason::Left)])
        ),
        "role_not_permitted"
    );

    // set_role: ADMIN+, target strictly below, new role <= own, never OWNER, not self.
    let promoted = ok(
        s,
        &author(s, &c.admin, vec![set_role(&c.member, ChannelRole::Admin)]),
    );
    assert_eq!(promoted.members[&c.member.id].role, ChannelRole::Admin);
    assert_eq!(
        err(
            s,
            &forced(
                s,
                &c.moderator,
                vec![set_role(&c.member, ChannelRole::Moderator)]
            )
        ),
        "role_not_permitted"
    );
    assert_eq!(
        err(
            s,
            &forced(s, &c.admin, vec![set_role(&c.admin, ChannelRole::Member)])
        ),
        "invalid_action"
    );
    assert_eq!(
        err(
            s,
            &forced(s, &c.owner, vec![set_role(&c.admin, ChannelRole::Owner)])
        ),
        "role_not_permitted"
    );
    let peer_admin = ok(
        s,
        &author(
            s,
            &c.owner,
            vec![set_role(&c.moderator, ChannelRole::Admin)],
        ),
    );
    let p = Some(&peer_admin);
    assert_eq!(
        err(
            p,
            &forced(
                p,
                &c.admin,
                vec![set_role(&c.moderator, ChannelRole::Member)]
            )
        ),
        "role_not_permitted"
    );

    // timer: ADMIN+; policy: OWNER only.
    let timer = Action::SetDisappearing(pb::SetDisappearing { seconds: 60 });
    assert_eq!(
        ok(s, &author(s, &c.admin, vec![timer.clone()])).disappearing_seconds,
        60
    );
    assert_eq!(
        err(s, &forced(s, &c.moderator, vec![timer])),
        "role_not_permitted"
    );
    let open = Action::SetPolicy(pb::SetPolicy {
        policy: Some(Policy::defaults(true).to_proto()),
    });
    assert_eq!(
        err(s, &forced(s, &c.admin, vec![open.clone()])),
        "role_not_permitted"
    );
    assert_eq!(
        ok(s, &author(s, &c.owner, vec![open])).policy.join,
        JoinMode::Open
    );
    let mut bad = Policy::defaults(false).to_proto();
    bad.set_role = 0;
    assert_eq!(
        err(
            s,
            &forced(
                s,
                &c.owner,
                vec![Action::SetPolicy(pb::SetPolicy { policy: Some(bad) })]
            )
        ),
        "malformed"
    );
}

#[test]
fn ownership_transfer_and_running_state() {
    let (c, s1) = channel();
    let s = Some(&s1);
    let transfer = |w: &Who| {
        Action::TransferOwnership(pb::TransferOwnershipAction {
            new_owner_user_id: w.id.clone(),
        })
    };
    let s2 = ok(s, &author(s, &c.owner, vec![transfer(&c.member)]));
    assert_eq!(s2.members[&c.member.id].role, ChannelRole::Owner);
    assert_eq!(s2.members[&c.owner.id].role, ChannelRole::Admin);
    assert_eq!(
        err(s, &forced(s, &c.admin, vec![transfer(&c.member)])),
        "role_not_permitted"
    );
    assert_eq!(
        err(s, &forced(s, &c.owner, vec![transfer(&c.outsider)])),
        "invalid_action"
    );

    // Later actions read the running state: once transferred, the former
    // owner is an ADMIN and may no longer set the policy.
    let open = Action::SetPolicy(pb::SetPolicy {
        policy: Some(Policy::defaults(true).to_proto()),
    });
    assert_eq!(
        err(s, &forced(s, &c.owner, vec![transfer(&c.member), open])),
        "role_not_permitted"
    );
    // And the former owner can now leave.
    let s3 = ok(
        Some(&s2),
        &author(Some(&s2), &c.owner, vec![remove(&c.owner, Reason::Left)]),
    );
    assert!(!s3.members.contains_key(&c.owner.id));
}

#[test]
fn direct_invites_are_accepted_or_declined_by_the_invitee() {
    let (c, s1) = channel();
    let s = Some(&s1);
    let invited = ok(
        s,
        &author(
            s,
            &c.admin,
            vec![add(&c.outsider, ChannelRole::Member, true)],
        ),
    );
    assert_eq!(
        invited.members[&c.outsider.id].status,
        MemberStatus::Invited
    );
    let i = Some(&invited);

    // An INVITED user has no authority, and cannot be re-added.
    assert_eq!(
        err(
            i,
            &forced(
                i,
                &c.outsider,
                vec![add(&who(9), ChannelRole::Member, false)]
            )
        ),
        "role_not_permitted"
    );
    assert_eq!(
        err(
            i,
            &forced(
                i,
                &c.admin,
                vec![add(&c.outsider, ChannelRole::Member, false)]
            )
        ),
        "invalid_action"
    );

    let accepted = ok(
        i,
        &author(
            i,
            &c.outsider,
            vec![Action::AcceptInvite(pb::AcceptInvite {})],
        ),
    );
    assert_eq!(
        accepted.members[&c.outsider.id].status,
        MemberStatus::Active
    );
    assert_eq!(
        err(
            Some(&accepted),
            &forced(
                Some(&accepted),
                &c.outsider,
                vec![Action::AcceptInvite(pb::AcceptInvite {})]
            )
        ),
        "invalid_action"
    );

    let declined = ok(
        i,
        &author(i, &c.outsider, vec![remove(&c.outsider, Reason::Left)]),
    );
    assert!(!declined.members.contains_key(&c.outsider.id));
}

// -------------------------------------------------------------------- invites

fn create_invite(invite: &KeyPair, max_uses: u32, expires_at: u64, needs_approval: bool) -> Action {
    Action::CreateInvite(pb::CreateInvite {
        capability: Some(pb::InviteCapability {
            invite_pub: key(invite).to_vec(),
            role: ChannelRole::Member as i32,
            expires_at,
            max_uses,
            needs_approval,
            uses: 0,
        }),
    })
}

/// `joiner`'s JoinViaInvite on `prev`, with an invite proof by `link`.
fn join(prev: &MembershipState, joiner: &Who, link: &KeyPair, ts: u64) -> pb::ChannelChange {
    let acts = vec![Action::JoinViaInvite(pb::JoinViaInvite {
        invite_pub: key(link).to_vec(),
    })];
    let actions: Vec<_> = acts.iter().cloned().map(act).collect();
    let new_hash = preview(
        Some(prev),
        &actions,
        CH,
        prev.revision + 1,
        &joiner.id,
        key(&joiner.kp),
        ts,
    )
    .map(|s| s.hash().to_vec())
    .unwrap_or(vec![0; 32]);
    let mut change = signed(
        &raw_actions(Some(prev), joiner, acts, ts, new_hash),
        &joiner.kp,
    );
    change.invite_proof = sign(link, INVITE_DOMAIN, &change.actions);
    change
}

#[test]
fn join_links_are_capabilities_with_counted_uses() {
    let (c, s1) = channel();
    let link = KeyPair::generate(&mut rand::rng());
    let s2 = ok(
        Some(&s1),
        &author(
            Some(&s1),
            &c.admin,
            vec![create_invite(&link, 1, TS + 10, false)],
        ),
    );
    assert_eq!(
        err(
            Some(&s1),
            &forced(
                Some(&s1),
                &c.moderator,
                vec![create_invite(&link, 1, 0, false)]
            )
        ),
        "role_not_permitted"
    );
    assert_eq!(
        err(
            Some(&s2),
            &forced(Some(&s2), &c.admin, vec![create_invite(&link, 1, 0, false)])
        ),
        "invalid_action"
    );

    let (dave, erin) = (who(10), who(11));
    let s3 = ok(Some(&s2), &join(&s2, &dave, &link, TS));
    assert_eq!(s3.members[&dave.id].status, MemberStatus::Active);
    assert_eq!(s3.members[&dave.id].identity_key, key(&dave.kp));
    assert_eq!(s3.invites[&key(&link)].uses, 1);

    // One use: the second joiner is refused.
    assert_eq!(
        err(Some(&s3), &join(&s3, &erin, &link, TS)),
        "invite_unusable"
    );
    // Expired at the change's timestamp (expires_at is exclusive).
    assert_eq!(
        err(Some(&s2), &join(&s2, &dave, &link, TS + 10)),
        "invite_unusable"
    );
    // A proof by any other key, or under the change domain.
    let mut wrong = join(&s2, &dave, &link, TS);
    wrong.invite_proof = sign(&dave.kp, INVITE_DOMAIN, &wrong.actions);
    assert_eq!(err(Some(&s2), &wrong), "bad_invite_proof");
    wrong.invite_proof = sign(&link, CHANGE_DOMAIN, &wrong.actions);
    assert_eq!(err(Some(&s2), &wrong), "bad_invite_proof");
    wrong.invite_proof.clear();
    assert_eq!(err(Some(&s2), &wrong), "bad_invite_proof");
    // Unknown link.
    assert_eq!(
        err(
            Some(&s2),
            &join(&s2, &dave, &KeyPair::generate(&mut rand::rng()), TS)
        ),
        "invite_unusable"
    );
    // A join must stand alone; a proof on a non-join change is malformed.
    let pair = vec![
        Action::JoinViaInvite(pb::JoinViaInvite {
            invite_pub: key(&link).to_vec(),
        }),
        Action::AcceptInvite(pb::AcceptInvite {}),
    ];
    assert_eq!(
        err(Some(&s2), &forced(Some(&s2), &dave, pair)),
        "invalid_action"
    );
    let mut stray = author(
        Some(&s2),
        &c.admin,
        vec![add(&dave, ChannelRole::Member, false)],
    );
    stray.invite_proof = vec![0; 64];
    assert_eq!(err(Some(&s2), &stray), "malformed");

    // Revoked links stop working.
    let revoke = Action::RevokeInvite(pb::RevokeInvite {
        invite_pub: key(&link).to_vec(),
    });
    let s3r = ok(
        Some(&s2),
        &author(Some(&s2), &c.admin, vec![revoke.clone()]),
    );
    assert_eq!(
        err(Some(&s3r), &join(&s3r, &dave, &link, TS)),
        "invite_unusable"
    );
    assert_eq!(
        err(Some(&s3r), &forced(Some(&s3r), &c.admin, vec![revoke])),
        "invalid_action"
    );
}

#[test]
fn approval_links_land_as_requesting() {
    let (c, s1) = channel();
    let link = KeyPair::generate(&mut rand::rng());
    let s2 = ok(
        Some(&s1),
        &author(Some(&s1), &c.admin, vec![create_invite(&link, 0, 0, true)]),
    );
    let dave = who(10);
    let s3 = ok(Some(&s2), &join(&s2, &dave, &link, TS));
    assert_eq!(s3.members[&dave.id].status, MemberStatus::Requesting);
    let view = VerifiedChannel {
        state: s3.clone(),
        state_hash: s3.hash(),
        excluded: Default::default(),
        status: VerificationStatus::Ok,
    };
    assert!(!view.contains(&dave.id), "a request does not receive keys");

    // An admin approves; the joiner keeps their own key.
    let mut approve = add(&dave, ChannelRole::Member, false);
    if let Action::AddMember(a) = &mut approve {
        a.identity_key = key(&c.admin.kp).to_vec();
    }
    let s4 = ok(Some(&s3), &author(Some(&s3), &c.admin, vec![approve]));
    assert_eq!(s4.members[&dave.id].status, MemberStatus::Active);
    assert_eq!(s4.members[&dave.id].identity_key, key(&dave.kp));
}

#[test]
fn open_channels_admit_self_signed_joins() {
    let owner = who(1);
    let p1 = ok(None, &author(None, &owner, vec![genesis(true)]));
    let anyone = who(42);
    let join = Action::JoinViaInvite(pb::JoinViaInvite { invite_pub: vec![] });
    let p2 = ok(Some(&p1), &author(Some(&p1), &anyone, vec![join.clone()]));
    assert_eq!(
        p2.members[&anyone.id],
        Member {
            role: ChannelRole::Member,
            status: MemberStatus::Active,
            identity_key: key(&anyone.kp)
        }
    );
    // A second self-join by a member is refused, and a stray proof is malformed.
    assert_eq!(
        err(Some(&p2), &forced(Some(&p2), &anyone, vec![join.clone()])),
        "invalid_action"
    );
    let mut stray = author(Some(&p1), &who(43), vec![join]);
    stray.invite_proof = vec![0; 64];
    assert_eq!(err(Some(&p1), &stray), "malformed");
}

// ------------------------------------------------------------- KT, bootstrap

struct MockKt {
    keys: HashMap<String, KeyBytes>,
    calls: Arc<AtomicUsize>,
    down: bool,
}

#[async_trait]
impl IdentityKeyVerifier for MockKt {
    async fn is_current_identity_key(
        &self,
        user_id: &str,
        k: &KeyBytes,
    ) -> Result<bool, VerifyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.down {
            return Err(VerifyError::KtUnavailable("offline".into()));
        }
        Ok(self.keys.get(user_id) == Some(k))
    }
}

fn kt(c: &Cast) -> (MockKt, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let keys = [&c.owner, &c.admin, &c.moderator, &c.member, &c.outsider]
        .iter()
        .map(|w| (w.id.clone(), key(&w.kp)))
        .collect();
    (
        MockKt {
            keys,
            calls: calls.clone(),
            down: false,
        },
        calls,
    )
}

#[tokio::test]
async fn author_key_must_be_the_kt_key() {
    let (c, s1) = channel();
    let (verifier, _) = kt(&c);
    let good = author(
        Some(&s1),
        &c.admin,
        vec![add(&c.outsider, ChannelRole::Member, false)],
    );
    let v = verify_change(CH, Some(&s1), &good, &verifier)
        .await
        .unwrap();
    assert_eq!(v.became_active(Some(&s1)), vec![c.outsider.id.clone()]);

    // The admin's id, signed by a key the server minted: valid signature,
    // wrong key.
    let impostor = Who {
        id: c.admin.id.clone(),
        kp: KeyPair::generate(&mut rand::rng()),
    };
    let forged = author(
        Some(&s1),
        &impostor,
        vec![add(&c.outsider, ChannelRole::Member, false)],
    );
    assert_eq!(
        check_change(CH, Some(&s1), &forged).map(|_| ()),
        Ok(()),
        "only KT can catch this"
    );
    assert_eq!(
        verify_change(CH, Some(&s1), &forged, &verifier)
            .await
            .unwrap_err(),
        VerifyError::KtMismatch
    );

    let down = MockKt {
        down: true,
        ..kt(&c).0
    };
    let e = verify_change(CH, Some(&s1), &good, &down)
        .await
        .unwrap_err();
    assert!(
        !e.is_failure(),
        "KT unavailable is not a verification failure"
    );
}

#[tokio::test]
async fn kt_cache_keeps_positive_answers_only() {
    let (c, _) = channel();
    let (inner, calls) = kt(&c);
    let cached = CachedKtVerifier::new(inner, std::time::Duration::from_secs(60));
    for _ in 0..3 {
        assert!(
            cached
                .is_current_identity_key(&c.admin.id, &key(&c.admin.kp))
                .await
                .unwrap()
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let other = key(&KeyPair::generate(&mut rand::rng()));
    for _ in 0..2 {
        assert!(
            !cached
                .is_current_identity_key(&c.admin.id, &other)
                .await
                .unwrap()
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 3, "negatives are re-asked");
    cached.forget_user(&c.admin.id).await;
    assert!(
        cached
            .is_current_identity_key(&c.admin.id, &key(&c.admin.kp))
            .await
            .unwrap()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);

    // Past the TTL a positive answer is asked again.
    let (inner, zero_calls) = kt(&c);
    let zero = CachedKtVerifier::new(inner, std::time::Duration::ZERO);
    for _ in 0..2 {
        assert!(
            zero.is_current_identity_key(&c.admin.id, &key(&c.admin.kp))
                .await
                .unwrap()
        );
    }
    assert_eq!(zero_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn bootstrap_trusts_state_only_through_change_n() {
    let (c, s1) = channel();
    let (verifier, _) = kt(&c);
    let n = author(
        Some(&s1),
        &c.admin,
        vec![add(&c.outsider, ChannelRole::Member, false)],
    );
    let (base, v) = bootstrap(CH, Some(&s1.to_proto()), &n, &verifier)
        .await
        .unwrap();
    assert_eq!(base.as_ref(), Some(&s1));
    assert!(v.0.new_state.members.contains_key(&c.outsider.id));

    // A server-doctored N-1 (a ghost slipped in) no longer matches prev_state_hash.
    let mut doctored = s1.to_proto();
    doctored.members.push(pb::StateMember {
        user_id: who(66).id,
        role: ChannelRole::Admin as i32,
        identity_key: vec![5; 33],
        status: MemberStatus::Active as i32,
    });
    assert_eq!(
        bootstrap(CH, Some(&doctored), &n, &verifier)
            .await
            .unwrap_err(),
        VerifyError::PrevHashMismatch
    );

    // Genesis bootstraps from nothing.
    let g = author(None, &c.owner, vec![genesis(false)]);
    assert!(
        bootstrap(CH, None, &g, &verifier)
            .await
            .unwrap()
            .0
            .is_none()
    );
}

// ------------------------------------------------------- fork, unsigned, view

#[test]
fn fork_detection() {
    let (h1, h2) = ([1u8; 32], [2u8; 32]);
    assert_eq!(
        compare_peer_state(5, &h1, 5, &h1, None),
        PeerStateCheck::InSync
    );
    assert_eq!(
        compare_peer_state(5, &h1, 5, &h2, None),
        PeerStateCheck::Fork
    );
    assert_eq!(
        compare_peer_state(5, &h1, 6, &h2, None),
        PeerStateCheck::PeerAhead
    );
    assert_eq!(
        compare_peer_state(5, &h1, 4, &h2, None),
        PeerStateCheck::PeerBehind
    );
    assert_eq!(
        compare_peer_state(5, &h1, 4, &h2, Some(&h2)),
        PeerStateCheck::PeerBehind
    );
    assert_eq!(
        compare_peer_state(5, &h1, 4, &h2, Some(&h1)),
        PeerStateCheck::Fork
    );
}

#[test]
fn unsigned_events_follow_the_asymmetric_rule() {
    let (c, s1) = channel();
    let added = |w: &Who| UnsignedEvent::Added {
        user_id: w.id.clone(),
    };
    let removed = |w: &Who| UnsignedEvent::Removed {
        user_id: w.id.clone(),
    };
    assert_eq!(
        classify_unsigned(&s1, &added(&c.outsider)),
        UnsignedDisposition::IgnoreAndSurface
    );
    assert_eq!(
        classify_unsigned(&s1, &added(&c.member)),
        UnsignedDisposition::NoOp
    );
    assert_eq!(
        classify_unsigned(&s1, &removed(&c.member)),
        UnsignedDisposition::ExcludeFromDistribution
    );
    assert_eq!(
        classify_unsigned(&s1, &removed(&c.outsider)),
        UnsignedDisposition::NoOp
    );

    let mut view = VerifiedChannel {
        state_hash: s1.hash(),
        state: s1,
        excluded: Default::default(),
        status: VerificationStatus::Ok,
    };
    assert!(view.contains(&c.member.id));
    view.excluded.insert(c.member.id.clone());
    assert!(!view.contains(&c.member.id));
    assert!(!view.members().contains(&c.member.id));
    assert_eq!(view.members().len(), 3);
    assert_eq!(view.revision(), 1);
}

#[test]
fn removal_reports_who_left_active() {
    let (c, s1) = channel();
    let ch = author(
        Some(&s1),
        &c.moderator,
        vec![remove(&c.member, Reason::Kicked)],
    );
    let v = VerifiedChange(check_change(CH, Some(&s1), &ch).unwrap());
    assert_eq!(v.left_active(Some(&s1)), vec![c.member.id.clone()]);
    assert!(v.became_active(Some(&s1)).is_empty());
}

// --------------------------------------------------------------- golden vectors

fn hexd(s: &str) -> Vec<u8> {
    hex::decode(s).expect("hex")
}

fn vector_state(v: &serde_json::Value) -> pb::MembershipState {
    let n = |k: &str| v["policy"][k].as_i64().unwrap() as i32;
    pb::MembershipState {
        channel_id: v["channel_id"].as_str().unwrap().into(),
        revision: v["revision"].as_u64().unwrap(),
        members: v["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| pb::StateMember {
                user_id: m["user_id"].as_str().unwrap().into(),
                role: m["role"].as_i64().unwrap() as i32,
                identity_key: hexd(m["identity_key"].as_str().unwrap()),
                status: m["status"].as_i64().unwrap() as i32,
            })
            .collect(),
        policy: Some(pb::AccessPolicy {
            add_member: n("add_member"),
            remove_member: n("remove_member"),
            set_role: n("set_role"),
            manage_invites: n("manage_invites"),
            set_disappearing: n("set_disappearing"),
            join: n("join"),
        }),
        invites: v["invites"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| pb::InviteCapability {
                invite_pub: hexd(i["invite_pub"].as_str().unwrap()),
                role: i["role"].as_i64().unwrap() as i32,
                expires_at: i["expires_at"].as_u64().unwrap(),
                max_uses: i["max_uses"].as_u64().unwrap() as u32,
                needs_approval: i["needs_approval"].as_bool().unwrap(),
                uses: i["uses"].as_u64().unwrap() as u32,
            })
            .collect(),
        disappearing_seconds: v["disappearing_seconds"].as_u64().unwrap() as u32,
    }
}

/// `internal/services/channel/membership/testdata/membership_vectors.json`,
/// written by the server's Go verifier: canonical bytes and hashes must match
/// byte for byte, every Go-signed change must verify (or fail with the same
/// code) under libsignal's XEdDSA.
#[test]
fn golden_vectors_match_the_server_verifier() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../internal/services/channel/membership/testdata/membership_vectors.json"
    );
    let file = std::fs::read_to_string(path).expect("golden vectors (shared with the Go verifier)");
    let v: serde_json::Value = serde_json::from_str(&file).unwrap();
    assert_eq!(v["version"], 1);
    assert_eq!(hexd(v["domains"]["state"].as_str().unwrap()), STATE_DOMAIN);
    assert_eq!(
        hexd(v["domains"]["change"].as_str().unwrap()),
        CHANGE_DOMAIN
    );
    assert_eq!(
        hexd(v["domains"]["invite"].as_str().unwrap()),
        INVITE_DOMAIN
    );

    for k in v["keys"].as_array().unwrap() {
        let private = PrivateKey::deserialize(&hexd(k["private"].as_str().unwrap())).unwrap();
        assert_eq!(
            private.public_key().unwrap().serialize().to_vec(),
            hexd(k["public"].as_str().unwrap()),
            "key {}",
            k["name"]
        );
    }

    let mut states = HashMap::new();
    for s in v["states"].as_array().unwrap() {
        let name = s["name"].as_str().unwrap();
        let state = MembershipState::from_proto(&vector_state(s))
            .unwrap_or_else(|e| panic!("state {name}: {e}"));
        assert_eq!(
            canonical_state(&state),
            hexd(s["canonical"].as_str().unwrap()),
            "canonical {name}"
        );
        assert_eq!(
            state.hash().to_vec(),
            hexd(s["hash"].as_str().unwrap()),
            "hash {name}"
        );
        states.insert(name.to_string(), state);
    }
    assert!(states.len() >= 2);

    let changes = v["changes"].as_array().unwrap();
    assert!(!changes.is_empty());
    for c in changes {
        let name = c["name"].as_str().unwrap();
        let prev = match c["prev_state"].as_str().unwrap() {
            "" => None,
            p => Some(&states[p]),
        };
        let change = pb::ChannelChange {
            actions: hexd(c["actions"].as_str().unwrap()),
            signature: hexd(c["signature"].as_str().unwrap()),
            invite_proof: hexd(c["invite_proof"].as_str().unwrap()),
        };
        let got = check_change(c["channel_id"].as_str().unwrap(), prev, &change);
        if c["expect_ok"].as_bool().unwrap() {
            let got = got.unwrap_or_else(|e| panic!("change {name}: {e}"));
            let want = &states[c["expect_state"].as_str().unwrap()];
            assert_eq!(&got.new_state, want, "change {name}");
            assert_eq!(got.new_state_hash, want.hash(), "change {name}");
        } else {
            let e = got.expect_err(name);
            assert_eq!(
                e.code(),
                c["expect_error"].as_str().unwrap(),
                "change {name}: {e}"
            );
        }
    }
}
