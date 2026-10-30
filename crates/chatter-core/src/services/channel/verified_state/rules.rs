//! The membership state machine: every action's authority check and
//! transition, as specified per action in `channel.proto` ("Actions").
//! Actions apply in order to a running copy of the previous state, and the
//! author's role and status are read from that running copy.
//!
//! The order of checks inside each action, and the error class each one
//! raises, mirror the server verifier
//! (`internal/services/channel/membership/membership.go` `applyOne`): the
//! golden vectors pin the error code, so the two must fail the same way.

use std::collections::BTreeMap;

use libsignal_protocol::PublicKey;

use super::canonical::{INVITE_DOMAIN, signed_message};
use super::{
    ChannelRole, Invite, JoinMode, KeyBytes, Member, MemberStatus, MembershipState, Policy,
    VerifyError, is_canonical_uuid, malformed,
};
use crate::proto::chat_v1 as pb;
use pb::channel_action::Action;
use pb::remove_member::Reason;

/// What the rules need beyond the decoded actions.
pub struct ApplyContext<'a> {
    pub channel_id: &'a str,
    pub revision: u64,
    pub author: &'a str,
    pub author_key: KeyBytes,
    /// `Actions.timestamp` (Unix nanos, author's clock): invite expiry is
    /// judged against it.
    pub timestamp: u64,
    pub proof: ProofCheck<'a>,
}

/// How a JoinViaInvite's `invite_proof` is treated.
#[derive(Clone, Copy)]
pub enum ProofCheck<'a> {
    /// Verification: the proof must verify over the exact signed `actions`
    /// bytes, and a proof on a change that needs none is malformed.
    Verify {
        actions_bytes: &'a [u8],
        invite_proof: &'a [u8],
    },
    /// Authoring ([`preview`]): the change is not signed yet, so there is no
    /// proof to check. Never used on a received change.
    AuthorPreview,
}

fn deny(what: &str) -> VerifyError {
    VerifyError::NotPermitted(what.to_string())
}

fn invalid(what: &str) -> VerifyError {
    VerifyError::InvalidAction(what.to_string())
}

fn unusable(what: &str) -> VerifyError {
    VerifyError::InviteUnusable(what.to_string())
}

/// MEMBER..=OWNER (no UNSPECIFIED, no unknown number).
fn valid_role(v: i32) -> Option<ChannelRole> {
    match ChannelRole::try_from(v) {
        Ok(ChannelRole::RoleUnspecified) | Err(_) => None,
        Ok(r) => Some(r),
    }
}

fn valid_key(b: &[u8]) -> Option<KeyBytes> {
    let k: KeyBytes = b.try_into().ok()?;
    (k[0] == 0x05).then_some(k)
}

/// A 33-byte array for a map lookup; a wrong-length value matches nothing.
fn lookup_key(b: &[u8]) -> Option<KeyBytes> {
    b.try_into().ok()
}

/// Apply `actions` to `prev` (None = before genesis). Returns the resulting
/// state at `ctx.revision`. Does not check the signature over the change.
pub fn apply(
    prev: Option<&MembershipState>,
    actions: &[pb::ChannelAction],
    ctx: &ApplyContext<'_>,
) -> Result<MembershipState, VerifyError> {
    if actions.is_empty() {
        return Err(malformed("no actions"));
    }
    let mut state: Option<MembershipState> = prev.cloned();

    for (index, action) in actions.iter().enumerate() {
        let Some(action) = action.action.as_ref() else {
            return Err(malformed("unknown action"));
        };
        if let Action::Genesis(g) = action {
            if index != 0 || ctx.revision != 1 || state.is_some() {
                return Err(invalid("genesis must be the first action of revision 1"));
            }
            let policy = g
                .policy
                .as_ref()
                .map(Policy::from_proto)
                .transpose()?
                .ok_or_else(|| malformed("genesis policy"))?;
            let mut members = BTreeMap::new();
            members.insert(
                ctx.author.to_string(),
                Member {
                    role: ChannelRole::Owner,
                    status: MemberStatus::Active,
                    identity_key: ctx.author_key,
                },
            );
            state = Some(MembershipState {
                channel_id: ctx.channel_id.to_string(),
                revision: 0,
                members,
                policy,
                invites: BTreeMap::new(),
                disappearing_seconds: g.disappearing_seconds,
            });
            continue;
        }
        let Some(st) = state.as_mut() else {
            return Err(invalid("no genesis yet"));
        };
        apply_one(st, action, actions.len(), ctx)?;
    }

    if let ProofCheck::Verify { invite_proof, .. } = ctx.proof {
        let invite_join = matches!(
            actions,
            [pb::ChannelAction { action: Some(Action::JoinViaInvite(j)) }] if !j.invite_pub.is_empty()
        );
        if !invite_proof.is_empty() && !invite_join {
            return Err(malformed(
                "invite_proof on a change that is not an invite join",
            ));
        }
    }
    let mut state = state.expect("non-empty actions leave a state");
    if !state.has_one_owner() {
        return Err(invalid("a state must have exactly one owner"));
    }
    state.revision = ctx.revision;
    Ok(state)
}

fn apply_one(
    st: &mut MembershipState,
    action: &Action,
    action_count: usize,
    ctx: &ApplyContext<'_>,
) -> Result<(), VerifyError> {
    let author = st.members.get(ctx.author).cloned();
    // The author's role counts only while ACTIVE.
    let role = author
        .as_ref()
        .filter(|m| m.status == MemberStatus::Active)
        .map(|m| m.role);
    let need_active = || role.ok_or_else(|| deny("author is not an active member"));

    match action {
        Action::Genesis(_) => unreachable!("handled by apply"),

        Action::AddMember(a) => {
            let role = need_active()?;
            let (Some(new_role), Some(identity_key)) =
                (valid_role(a.role), valid_key(&a.identity_key))
            else {
                return Err(malformed("add_member fields"));
            };
            if !is_canonical_uuid(&a.user_id) {
                return Err(malformed("add_member fields"));
            }
            if role < st.policy.add_member || new_role > role || new_role == ChannelRole::Owner {
                return Err(deny("add_member above own role or below policy"));
            }
            match st.members.get_mut(&a.user_id) {
                None => {
                    let status = if a.pending {
                        MemberStatus::Invited
                    } else {
                        MemberStatus::Active
                    };
                    st.members.insert(
                        a.user_id.clone(),
                        Member {
                            role: new_role,
                            status,
                            identity_key,
                        },
                    );
                }
                // Approving a join request: the joiner's own key (from their
                // JoinViaInvite) is kept, not replaced by the approver's view.
                Some(m) if m.status == MemberStatus::Requesting && !a.pending => {
                    m.status = MemberStatus::Active;
                    m.role = new_role;
                }
                Some(_) => return Err(invalid("add of a user already in the channel")),
            }
        }

        Action::RemoveMember(r) => {
            let Some(target) = st.members.get(&r.user_id) else {
                return Err(invalid("remove of a user not in the channel"));
            };
            if r.user_id == ctx.author {
                if r.reason != Reason::Left as i32 {
                    return Err(malformed("self-removal must be LEFT"));
                }
                if target.role == ChannelRole::Owner && target.status == MemberStatus::Active {
                    return Err(deny("the owner must transfer ownership before leaving"));
                }
            } else {
                let role = need_active()?;
                if r.reason != Reason::Kicked as i32 && r.reason != Reason::Banned as i32 {
                    return Err(malformed("removing someone else must be KICKED or BANNED"));
                }
                if role < st.policy.remove_member || target.role >= role {
                    return Err(deny("remove_member at or above own role or below policy"));
                }
            }
            st.members.remove(&r.user_id);
        }

        Action::SetRole(s) => {
            let role = need_active()?;
            let target_role = match st.members.get(&s.user_id) {
                Some(t) if t.status == MemberStatus::Active && s.user_id != ctx.author => t.role,
                _ => return Err(invalid("set_role target must be another active member")),
            };
            let Some(new_role) = valid_role(s.role) else {
                return Err(malformed("set_role role"));
            };
            if role < st.policy.set_role
                || target_role >= role
                || new_role > role
                || new_role == ChannelRole::Owner
            {
                return Err(deny("set_role at or above own role or below policy"));
            }
            if let Some(t) = st.members.get_mut(&s.user_id) {
                t.role = new_role;
            }
        }

        Action::CreateInvite(c) => {
            let role = need_active()?;
            let cap = c.capability.as_ref();
            let parsed =
                cap.and_then(|c| Some((valid_key(&c.invite_pub)?, valid_role(c.role)?, c)));
            let Some((invite_pub, invite_role, cap)) = parsed.filter(|(_, _, c)| c.uses == 0)
            else {
                return Err(malformed("capability"));
            };
            if role < st.policy.manage_invites
                || invite_role > role
                || invite_role == ChannelRole::Owner
            {
                return Err(deny("create_invite above own role or below policy"));
            }
            if st.invites.contains_key(&invite_pub) {
                return Err(invalid("invite already exists"));
            }
            st.invites.insert(
                invite_pub,
                Invite {
                    role: invite_role,
                    expires_at: cap.expires_at,
                    max_uses: cap.max_uses,
                    uses: 0,
                    needs_approval: cap.needs_approval,
                },
            );
        }

        Action::RevokeInvite(r) => {
            let role = need_active()?;
            if role < st.policy.manage_invites {
                return Err(deny("revoke_invite below policy"));
            }
            if lookup_key(&r.invite_pub)
                .and_then(|k| st.invites.remove(&k))
                .is_none()
            {
                return Err(invalid("no such invite"));
            }
        }

        Action::JoinViaInvite(j) => {
            if action_count != 1 {
                return Err(invalid("join_via_invite must be the only action"));
            }
            if author.is_some() {
                return Err(invalid("joiner is already in the channel"));
            }
            let mut joiner = Member {
                role: ChannelRole::Member,
                status: MemberStatus::Active,
                identity_key: ctx.author_key,
            };
            if j.invite_pub.is_empty() {
                if st.policy.join != JoinMode::Open {
                    return Err(deny("channel is not open"));
                }
                if let ProofCheck::Verify { invite_proof, .. } = ctx.proof {
                    if !invite_proof.is_empty() {
                        return Err(malformed("invite_proof without invite_pub"));
                    }
                }
            } else {
                let Some((invite_pub, cap)) =
                    lookup_key(&j.invite_pub).and_then(|k| st.invites.get_mut(&k).map(|c| (k, c)))
                else {
                    return Err(unusable("no such invite"));
                };
                if let ProofCheck::Verify {
                    actions_bytes,
                    invite_proof,
                } = ctx.proof
                {
                    verify_invite_proof(&invite_pub, actions_bytes, invite_proof)?;
                }
                if cap.expires_at != 0 && ctx.timestamp >= cap.expires_at {
                    return Err(unusable("invite expired"));
                }
                if cap.max_uses != 0 && cap.uses >= cap.max_uses {
                    return Err(unusable("invite used up"));
                }
                cap.uses += 1;
                joiner.role = cap.role;
                if cap.needs_approval {
                    joiner.status = MemberStatus::Requesting;
                }
            }
            st.members.insert(ctx.author.to_string(), joiner);
        }

        Action::AcceptInvite(_) => match st.members.get_mut(ctx.author) {
            Some(m) if m.status == MemberStatus::Invited => m.status = MemberStatus::Active,
            _ => return Err(invalid("author has no pending invite")),
        },

        Action::SetPolicy(p) => {
            if need_active()? != ChannelRole::Owner {
                return Err(deny("only the owner sets the policy"));
            }
            st.policy = p
                .policy
                .as_ref()
                .map(Policy::from_proto)
                .transpose()?
                .ok_or_else(|| malformed("policy"))?;
        }

        Action::SetDisappearing(d) => {
            if need_active()? < st.policy.set_disappearing {
                return Err(deny("set_disappearing below policy"));
            }
            st.disappearing_seconds = d.seconds;
        }

        Action::TransferOwnership(t) => {
            if need_active()? != ChannelRole::Owner {
                return Err(deny("only the owner transfers ownership"));
            }
            let to = &t.new_owner_user_id;
            match st.members.get_mut(to) {
                Some(m) if m.status == MemberStatus::Active && *to != ctx.author => {
                    m.role = ChannelRole::Owner
                }
                _ => return Err(invalid("new owner must be another active member")),
            }
            if let Some(a) = st.members.get_mut(ctx.author) {
                a.role = ChannelRole::Admin;
            }
        }
    }
    Ok(())
}

/// The state an author's actions would produce, for authoring: the
/// `new_state_hash` to commit to is `preview(..)?.hash()`. Runs every rule
/// except the invite proof, which does not exist before signing. Its result
/// is NOT verified state; only `verify::check_change` produces that.
pub fn preview(
    prev: Option<&MembershipState>,
    actions: &[pb::ChannelAction],
    channel_id: &str,
    revision: u64,
    author: &str,
    author_key: KeyBytes,
    timestamp: u64,
) -> Result<MembershipState, VerifyError> {
    apply(
        prev,
        actions,
        &ApplyContext {
            channel_id,
            revision,
            author,
            author_key,
            timestamp,
            proof: ProofCheck::AuthorPreview,
        },
    )
}

fn verify_invite_proof(
    invite_pub: &KeyBytes,
    actions: &[u8],
    proof: &[u8],
) -> Result<(), VerifyError> {
    let key = PublicKey::deserialize(invite_pub).map_err(|_| VerifyError::BadInviteProof)?;
    if proof.len() != 64 || !key.verify_signature(&signed_message(INVITE_DOMAIN, actions), proof) {
        return Err(VerifyError::BadInviteProof);
    }
    Ok(())
}
