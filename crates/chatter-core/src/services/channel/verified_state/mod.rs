//! Verified channel membership (Design B: a signed, hash-chained change log).
//!
//! The server keeps plaintext membership for routing, SpiceDB and search, but
//! it is no longer trusted for who receives sender keys. Every membership
//! mutation is a `ChannelChange` signed by its author's ACI identity key and
//! chained to the previous state by `prev_state_hash`
//! (`pkg/proto/chat/v1/channel/channel.proto`, "Verifiable Membership"). This
//! module is the client's verifier: it parses the exact signed bytes, checks
//! chain, signature, authority and the author's commitment to the result, and
//! produces the next [`MembershipState`]. Everything here is pure except the
//! KT lookup, which sits behind [`IdentityKeyVerifier`].
//!
//! - [`canonical`]: the canonical byte encoding and `state_hash`.
//! - [`rules`]: the per-action state machine (authority + transitions).
//! - [`verify`]: `check_change` / `verify_change`, bootstrap, fork detection,
//!   the asymmetric rule for unsigned server events, the KT trait and cache.
//!
//! Storage is `storage::repositories::verified_membership_repo` (migration v082).

pub mod canonical;
pub mod rules;
pub mod verify;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use crate::proto::chat_v1 as pb;

pub use pb::access_policy::JoinMode;
pub use pb::state_member::Status as MemberStatus;
pub use pb::ChannelRole;
pub use verify::{
    bootstrap, check_change, classify_unsigned, compare_peer_state, verify_change, CachedKtVerifier,
    IdentityKeyVerifier, PeerStateCheck, PendingChange, UnsignedDisposition, UnsignedEvent,
    VerifiedChange,
};

/// A serialized identity (or invite) public key: `0x05 || Curve25519 key`.
pub type KeyBytes = [u8; 33];

/// SHA-256 state hash.
pub type StateHash = [u8; 32];

/// Why a change (or a received state) was not accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The change is for a revision past `local + 1`: fetch
    /// `GetChannelChanges(local + 1)` and verify in order. Not a failure.
    #[error("revision gap: expected {expected}, got {got}")]
    Gap { expected: u64, got: u64 },
    /// The change is at or below the local revision: already applied (or a
    /// competing fork; compare its hash with `channel_changes`). Not applied.
    #[error("stale revision {got} (local is {local})")]
    Stale { local: u64, got: u64 },
    /// The KT lookup could not be completed. Transient: retry later; this
    /// says nothing about the change.
    #[error("KT lookup unavailable: {0}")]
    KtUnavailable(String),
    /// Bytes that do not parse, or fields outside their domain.
    #[error("malformed: {0}")]
    Malformed(String),
    #[error("change is for another channel")]
    WrongChannel,
    #[error("prev_state_hash does not match the local state")]
    PrevHashMismatch,
    #[error("signature does not verify under the author identity key")]
    BadSignature,
    #[error("invite proof does not verify")]
    BadInviteProof,
    /// The author's identity key is not their current KT-verified key.
    #[error("author identity key is not the KT-verified key")]
    KtMismatch,
    /// An action the author's role / status / the policy does not allow.
    #[error("not permitted: {0}")]
    NotPermitted(String),
    /// An action whose target or context makes it meaningless (adding a
    /// present user, removing an absent one, genesis twice, ...).
    #[error("invalid action: {0}")]
    InvalidAction(String),
    /// A join through an invite that is absent, expired or used up.
    #[error("invite unusable: {0}")]
    InviteUnusable(String),
    #[error("new_state_hash does not match the recomputed state")]
    NewHashMismatch,
}

impl VerifyError {
    /// The rejection code shared with the server verifier and the golden
    /// vectors (`internal/services/channel/membership/membership.go` `Code`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Gap { .. } | Self::Stale { .. } => "wrong_revision",
            Self::KtUnavailable(_) => "kt_unavailable",
            Self::KtMismatch => "kt_mismatch",
            Self::Malformed(_) => "malformed",
            Self::WrongChannel => "wrong_channel",
            Self::PrevHashMismatch => "wrong_prev_hash",
            Self::BadSignature => "bad_signature",
            Self::BadInviteProof => "bad_invite_proof",
            Self::NotPermitted(_) => "role_not_permitted",
            Self::InvalidAction(_) => "invalid_action",
            Self::InviteUnusable(_) => "invite_unusable",
            Self::NewHashMismatch => "wrong_new_state_hash",
        }
    }

    /// A verification FAILURE: the channel is send-blocked ("membership could
    /// not be verified") until a later change verifies. `Gap`, `Stale` and
    /// `KtUnavailable` are not failures.
    pub fn is_failure(&self) -> bool {
        !matches!(self, Self::Gap { .. } | Self::Stale { .. } | Self::KtUnavailable(_))
    }
}

/// Who may do what (`AccessPolicy`); no field is UNSPECIFIED.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub add_member: ChannelRole,
    pub remove_member: ChannelRole,
    pub set_role: ChannelRole,
    pub manage_invites: ChannelRole,
    pub set_disappearing: ChannelRole,
    pub join: JoinMode,
}

impl Policy {
    /// The defaults a client authors into Genesis (`channel.proto`,
    /// `AccessPolicy` comment): ADMIN for add / role / invites / timer,
    /// MODERATOR for removing others; OPEN for public channels, else INVITE_ONLY.
    pub fn defaults(public: bool) -> Self {
        Self {
            add_member: ChannelRole::Admin,
            remove_member: ChannelRole::Moderator,
            set_role: ChannelRole::Admin,
            manage_invites: ChannelRole::Admin,
            set_disappearing: ChannelRole::Admin,
            join: if public { JoinMode::Open } else { JoinMode::InviteOnly },
        }
    }

    pub fn from_proto(p: &pb::AccessPolicy) -> Result<Self, VerifyError> {
        Ok(Self {
            add_member: role(p.add_member)?,
            remove_member: role(p.remove_member)?,
            set_role: role(p.set_role)?,
            manage_invites: role(p.manage_invites)?,
            set_disappearing: role(p.set_disappearing)?,
            join: match JoinMode::try_from(p.join) {
                Ok(m @ (JoinMode::InviteOnly | JoinMode::Open)) => m,
                _ => return Err(malformed("policy.join")),
            },
        })
    }

    pub fn to_proto(&self) -> pb::AccessPolicy {
        pb::AccessPolicy {
            add_member: self.add_member as i32,
            remove_member: self.remove_member as i32,
            set_role: self.set_role as i32,
            manage_invites: self.manage_invites as i32,
            set_disappearing: self.set_disappearing as i32,
            join: self.join as i32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub role: ChannelRole,
    pub status: MemberStatus,
    pub identity_key: KeyBytes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub role: ChannelRole,
    /// Unix nanoseconds; 0 = never.
    pub expires_at: u64,
    /// 0 = unlimited.
    pub max_uses: u32,
    pub uses: u32,
    pub needs_approval: bool,
}

/// A validated membership state. Maps keep members and invites in the
/// canonical (byte-ascending) order, and make duplicates unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipState {
    /// Lowercase hyphenated UUID.
    pub channel_id: String,
    pub revision: u64,
    /// Keyed by lowercase hyphenated user UUID.
    pub members: BTreeMap<String, Member>,
    pub policy: Policy,
    pub invites: BTreeMap<KeyBytes, Invite>,
    pub disappearing_seconds: u32,
}

impl MembershipState {
    /// `SHA-256("chatter-channel-state-v1\0" || canonical_state)`.
    pub fn hash(&self) -> StateHash {
        canonical::state_hash(self)
    }

    /// Validate a server-supplied `MembershipState` (bootstrap). Duplicates,
    /// unknown enum numbers, non-canonical UUIDs and bad key lengths are
    /// rejected, as the canonical encoding defines them invalid.
    pub fn from_proto(p: &pb::MembershipState) -> Result<Self, VerifyError> {
        if !is_canonical_uuid(&p.channel_id) {
            return Err(malformed("channel_id"));
        }
        if p.revision == 0 {
            return Err(malformed("revision 0 has no state"));
        }
        let policy = Policy::from_proto(p.policy.as_ref().ok_or_else(|| malformed("policy missing"))?)?;
        let mut members = BTreeMap::new();
        for m in &p.members {
            if !is_canonical_uuid(&m.user_id) {
                return Err(malformed("member user_id"));
            }
            let status = match MemberStatus::try_from(m.status) {
                Ok(s @ (MemberStatus::Active | MemberStatus::Invited | MemberStatus::Requesting)) => s,
                _ => return Err(malformed("member status")),
            };
            let member = Member { role: role(m.role)?, status, identity_key: key33(&m.identity_key)? };
            if members.insert(m.user_id.clone(), member).is_some() {
                return Err(malformed("duplicate member"));
            }
        }
        let mut invites = BTreeMap::new();
        for i in &p.invites {
            let invite = Invite {
                role: role(i.role)?,
                expires_at: i.expires_at,
                max_uses: i.max_uses,
                uses: i.uses,
                needs_approval: i.needs_approval,
            };
            if invites.insert(key33(&i.invite_pub)?, invite).is_some() {
                return Err(malformed("duplicate invite"));
            }
        }
        let state = Self {
            channel_id: p.channel_id.clone(),
            revision: p.revision,
            members,
            policy,
            invites,
            disappearing_seconds: p.disappearing_seconds,
        };
        if !state.has_one_owner() {
            return Err(VerifyError::InvalidAction("a state must have exactly one owner".into()));
        }
        Ok(state)
    }

    /// Exactly one member holds OWNER, and is ACTIVE. Genesis creates one,
    /// only TransferOwnership moves it, and the owner cannot leave; a state
    /// (server-supplied at bootstrap, or the result of a change) with none or
    /// several is invalid.
    pub fn has_one_owner(&self) -> bool {
        let mut owners = self.members.values().filter(|m| m.role == ChannelRole::Owner);
        matches!((owners.next(), owners.next()), (Some(m), None) if m.status == MemberStatus::Active)
    }

    pub fn to_proto(&self) -> pb::MembershipState {
        pb::MembershipState {
            channel_id: self.channel_id.clone(),
            revision: self.revision,
            members: self
                .members
                .iter()
                .map(|(id, m)| pb::StateMember {
                    user_id: id.clone(),
                    role: m.role as i32,
                    identity_key: m.identity_key.to_vec(),
                    status: m.status as i32,
                })
                .collect(),
            policy: Some(self.policy.to_proto()),
            invites: self
                .invites
                .iter()
                .map(|(k, i)| pb::InviteCapability {
                    invite_pub: k.to_vec(),
                    role: i.role as i32,
                    expires_at: i.expires_at,
                    max_uses: i.max_uses,
                    needs_approval: i.needs_approval,
                    uses: i.uses,
                })
                .collect(),
            disappearing_seconds: self.disappearing_seconds,
        }
    }

    /// ACTIVE members: the only status that receives sender keys.
    pub fn active_members(&self) -> impl Iterator<Item = (&String, &Member)> {
        self.members.iter().filter(|(_, m)| m.status == MemberStatus::Active)
    }
}

/// One channel's verified membership as the key-gating code reads it (W3).
pub trait VerifiedMembership {
    /// The users that may receive this channel's sender key: ACTIVE in the
    /// verified state and not excluded by an unsigned server removal.
    fn members(&self) -> Vec<String>;
    /// Whether `user_id` is in [`Self::members`].
    fn contains(&self, user_id: &str) -> bool;
    fn state(&self) -> &MembershipState;
    fn revision(&self) -> u64;
    fn state_hash(&self) -> StateHash;
}

/// Local health of a channel's verified state. Anything but `Ok` blocks
/// sending (`is_channel_send_blocked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VerificationStatus {
    Ok = 0,
    /// A change failed verification; cleared by the next change that verifies.
    VerificationFailed = 1,
    /// A peer showed a different hash at a revision we hold; cleared only by
    /// the user acknowledging it.
    Fork = 2,
}

impl VerificationStatus {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Ok),
            1 => Some(Self::VerificationFailed),
            2 => Some(Self::Fork),
            _ => None,
        }
    }
}

/// A channel's verified state as loaded from storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedChannel {
    pub state: MembershipState,
    pub state_hash: StateHash,
    /// Users the server alone said were removed (account deletion, a removal
    /// without a signed change): kept in the verified state, "pending
    /// signature", but out of the distribution set (fail-safe).
    pub excluded: BTreeSet<String>,
    pub status: VerificationStatus,
}

impl VerifiedMembership for VerifiedChannel {
    fn members(&self) -> Vec<String> {
        self.state
            .active_members()
            .filter(|(id, _)| !self.excluded.contains(*id))
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn contains(&self, user_id: &str) -> bool {
        !self.excluded.contains(user_id)
            && self.state.members.get(user_id).is_some_and(|m| m.status == MemberStatus::Active)
    }

    fn state(&self) -> &MembershipState {
        &self.state
    }

    fn revision(&self) -> u64 {
        self.state.revision
    }

    fn state_hash(&self) -> StateHash {
        self.state_hash
    }
}

/// Lowercase hyphenated form, exactly as `Uuid::hyphenated` prints it.
pub fn is_canonical_uuid(s: &str) -> bool {
    uuid::Uuid::parse_str(s).is_ok_and(|u| u.hyphenated().to_string() == s)
}

pub(crate) fn malformed(what: &str) -> VerifyError {
    VerifyError::Malformed(what.to_string())
}

/// A ChannelRole number other than UNSPECIFIED.
pub(crate) fn role(v: i32) -> Result<ChannelRole, VerifyError> {
    match ChannelRole::try_from(v) {
        Ok(ChannelRole::RoleUnspecified) | Err(_) => Err(malformed("role")),
        Ok(r) => Ok(r),
    }
}

/// 33 bytes, `0x05 ||` key: the libsignal DJB public key serialization.
pub(crate) fn key33(b: &[u8]) -> Result<KeyBytes, VerifyError> {
    let k: KeyBytes = b.try_into().map_err(|_| malformed("key length"))?;
    if k[0] != 0x05 {
        return Err(malformed("key type"));
    }
    Ok(k)
}
