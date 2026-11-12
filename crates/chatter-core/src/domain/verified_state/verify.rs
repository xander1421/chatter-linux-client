//! Change verification, bootstrap, fork detection and the asymmetric rule for
//! unsigned server membership events.
//!
//! A change N is accepted only if (channel.proto, design "Verification"):
//! 1. `revision == local + 1` and `prev_state_hash == H(local)` (genesis:
//!    revision 1, empty prev hash, no local state);
//! 2. the XEdDSA signature over `"chatter-channel-change-v1\0" || actions`
//!    verifies under `author_identity_key` (libsignal `verify_signature`);
//! 3. that key is the author's current KT-verified identity key;
//! 4. every action is allowed (`rules::apply`);
//! 5. `new_state_hash` equals the hash of the recomputed state.
//!
//! The `actions` bytes are decoded, never re-serialized.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use libsignal_protocol::PublicKey;
use tokio::sync::Mutex;

use super::canonical::{CHANGE_DOMAIN, signed_message};
use super::rules::{ApplyContext, ProofCheck, apply};
use super::{
    KeyBytes, MemberStatus, MembershipState, StateHash, VerifyError, is_canonical_uuid, key33,
    malformed,
};
use crate::proto::chat_v1 as pb;

/// A change that passed every check except the KT lookup of its author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChange {
    pub author_user_id: String,
    pub author_identity_key: KeyBytes,
    pub timestamp: u64,
    pub new_state: MembershipState,
    pub new_state_hash: StateHash,
}

/// A fully verified change: commit `new_state` with
/// `VerifiedMembershipRepository::commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedChange(pub PendingChange);

impl VerifiedChange {
    pub fn revision(&self) -> u64 {
        self.0.new_state.revision
    }

    /// Users ACTIVE before and not ACTIVE after: their removal (kick, ban,
    /// leave, demotion out of ACTIVE) requires a sender-key rotation.
    pub fn left_active(&self, prev: Option<&MembershipState>) -> Vec<String> {
        let Some(prev) = prev else { return Vec::new() };
        prev.active_members()
            .filter(|(id, _)| {
                self.0
                    .new_state
                    .members
                    .get(*id)
                    .is_none_or(|m| m.status != MemberStatus::Active)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Users ACTIVE after and not before: they need our sender key.
    pub fn became_active(&self, prev: Option<&MembershipState>) -> Vec<String> {
        self.0
            .new_state
            .active_members()
            .filter(|(id, _)| {
                prev.and_then(|p| p.members.get(*id))
                    .is_none_or(|m| m.status != MemberStatus::Active)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// Every check except KT (pure). `channel_id` is the channel the change
/// arrived for; `local` is our verified state (None before genesis or for a
/// bootstrap from revision 1).
pub fn check_change(
    channel_id: &str,
    local: Option<&MembershipState>,
    change: &pb::ChannelChange,
) -> Result<PendingChange, VerifyError> {
    // Check order (and so the error each case raises) follows the server
    // verifier `membership.Verify`: parse, channel, author fields, revision,
    // prev hash, signature, actions (+ invite proof), new state hash.
    if change.actions.is_empty() {
        return Err(malformed("empty change"));
    }
    let actions =
        <pb::channel_change::Actions as prost::Message>::decode(change.actions.as_slice())
            .map_err(|e| VerifyError::Malformed(format!("actions: {e}")))?;

    if actions.channel_id != channel_id || !is_canonical_uuid(channel_id) {
        return Err(VerifyError::WrongChannel);
    }
    if !is_canonical_uuid(&actions.author_user_id) || actions.actions.is_empty() {
        return Err(malformed("author or actions missing"));
    }
    let author_identity_key = key33(&actions.author_identity_key)?;
    if local.is_some_and(|l| l.channel_id != channel_id) {
        return Err(VerifyError::WrongChannel);
    }

    // 1. Chain position.
    let local_revision = local.map_or(0, |s| s.revision);
    let expected = local_revision + 1;
    if actions.revision > expected {
        return Err(VerifyError::Gap {
            expected,
            got: actions.revision,
        });
    }
    if actions.revision < expected {
        return Err(VerifyError::Stale {
            local: local_revision,
            got: actions.revision,
        });
    }
    match local {
        None if !actions.prev_state_hash.is_empty() => return Err(VerifyError::PrevHashMismatch),
        Some(s) if actions.prev_state_hash.as_slice() != s.hash().as_slice() => {
            return Err(VerifyError::PrevHashMismatch);
        }
        _ => {}
    }

    // 2. Signature by the claimed author key (libsignal XEdDSA verify).
    let author_key =
        PublicKey::deserialize(&author_identity_key).map_err(|_| malformed("author key"))?;
    if change.signature.len() != 64
        || !author_key.verify_signature(
            &signed_message(CHANGE_DOMAIN, &change.actions),
            &change.signature,
        )
    {
        return Err(VerifyError::BadSignature);
    }

    // 4. Authority + transitions.
    let new_state = apply(
        local,
        &actions.actions,
        &ApplyContext {
            channel_id,
            revision: actions.revision,
            author: &actions.author_user_id,
            author_key: author_identity_key,
            timestamp: actions.timestamp,
            proof: ProofCheck::Verify {
                actions_bytes: &change.actions,
                invite_proof: &change.invite_proof,
            },
        },
    )?;

    // 5. The author's commitment to the result.
    let new_state_hash = new_state.hash();
    if actions.new_state_hash.as_slice() != new_state_hash.as_slice() {
        return Err(VerifyError::NewHashMismatch);
    }

    Ok(PendingChange {
        author_user_id: actions.author_user_id,
        author_identity_key,
        timestamp: actions.timestamp,
        new_state,
        new_state_hash,
    })
}

/// [`check_change`] plus check 3: the author key is their current KT-verified
/// identity key.
pub async fn verify_change(
    channel_id: &str,
    local: Option<&MembershipState>,
    change: &pb::ChannelChange,
    kt: &dyn IdentityKeyVerifier,
) -> Result<VerifiedChange, VerifyError> {
    let pending = check_change(channel_id, local, change)?;
    if kt
        .is_current_identity_key(&pending.author_user_id, &pending.author_identity_key)
        .await?
    {
        Ok(VerifiedChange(pending))
    } else {
        Err(VerifyError::KtMismatch)
    }
}

/// Start a chain at change N without replaying history: `state` is the
/// server's `GetChannelState(N - 1)` (None when N is the genesis). It is
/// trusted only through change N: `H(state)` must equal N's `prev_state_hash`
/// and N must verify against it, author KT-checked. Returns the state the
/// chain starts from and the verified change N.
///
/// For an admin-added joiner, trust is rooted in the KT-verified admin who
/// signed N. For a joiner who authored N themselves (`JoinViaInvite`), N says
/// nothing about `state` beyond their own commitment: that state is as the
/// server presented it until a peer's `(revision, state_hash)` cross-check
/// ([`compare_peer_state`]) agrees.
pub async fn bootstrap(
    channel_id: &str,
    state: Option<&pb::MembershipState>,
    change: &pb::ChannelChange,
    kt: &dyn IdentityKeyVerifier,
) -> Result<(Option<MembershipState>, VerifiedChange), VerifyError> {
    let base = state.map(MembershipState::from_proto).transpose()?;
    let verified = verify_change(channel_id, base.as_ref(), change, kt).await?;
    Ok((base, verified))
}

/// What a peer's `(revision, state_hash)` stamp (SKDM / group DataMessage)
/// says about our view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStateCheck {
    InSync,
    /// Same revision, different hash — or a lower revision whose hash differs
    /// from the one we recorded for it. Hard alert; stop distributing
    /// (`VerificationStatus::Fork`).
    Fork,
    /// The peer is ahead: fetch `GetChannelChanges(local + 1)`.
    PeerAhead,
    /// The peer is behind and we cannot contradict it (no recorded hash for
    /// its revision). It gets our key only if it is in our verified state.
    PeerBehind,
}

/// `hash_at_peer_revision`: our recorded hash at `peer_revision`, when
/// `peer_revision < local_revision` (`VerifiedMembershipRepository::state_hash_at`).
pub fn compare_peer_state(
    local_revision: u64,
    local_hash: &StateHash,
    peer_revision: u64,
    peer_hash: &[u8],
    hash_at_peer_revision: Option<&StateHash>,
) -> PeerStateCheck {
    use std::cmp::Ordering::*;
    match peer_revision.cmp(&local_revision) {
        Equal if peer_hash == local_hash.as_slice() => PeerStateCheck::InSync,
        Equal => PeerStateCheck::Fork,
        Greater => PeerStateCheck::PeerAhead,
        Less => match hash_at_peer_revision {
            Some(h) if h.as_slice() != peer_hash => PeerStateCheck::Fork,
            _ => PeerStateCheck::PeerBehind,
        },
    }
}

/// A membership event only the server vouches for (legacy roster sync,
/// account deletion, re-registration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsignedEvent {
    Added { user_id: String },
    Removed { user_id: String },
}

/// What the caller does with an [`UnsignedEvent`] (the asymmetric rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsignedDisposition {
    /// Agrees with the verified state already; nothing to do.
    NoOp,
    /// An unsigned add: ignore it for keys and surface it ("unverified
    /// member").
    IgnoreAndSurface,
    /// An unsigned removal of a verified member: exclude them from the
    /// distribution set (`VerifiedMembershipRepository::exclude`) and show
    /// "removed by server". They stay in the verified state, pending a signed
    /// removal; only a signed RemoveMember then AddMember re-admits them.
    ExcludeFromDistribution,
}

pub fn classify_unsigned(state: &MembershipState, event: &UnsignedEvent) -> UnsignedDisposition {
    match event {
        UnsignedEvent::Added { user_id } => {
            if state
                .members
                .get(user_id)
                .is_some_and(|m| m.status == MemberStatus::Active)
            {
                UnsignedDisposition::NoOp
            } else {
                UnsignedDisposition::IgnoreAndSurface
            }
        }
        UnsignedEvent::Removed { user_id } => {
            if state.members.contains_key(user_id) {
                UnsignedDisposition::ExcludeFromDistribution
            } else {
                UnsignedDisposition::NoOp
            }
        }
    }
}

/// The KT lookup behind check 3. W3/W4 implement it over
/// `services/signal/session.rs` `verify_peer_key_transparency`.
#[async_trait]
pub trait IdentityKeyVerifier: Send + Sync {
    /// `Ok(true)` iff `identity_key` is `user_id`'s current KT-verified ACI
    /// identity key; `Ok(false)` if KT proves another key (or none);
    /// `Err(VerifyError::KtUnavailable)` when the lookup could not complete.
    async fn is_current_identity_key(
        &self,
        user_id: &str,
        identity_key: &KeyBytes,
    ) -> Result<bool, VerifyError>;
}

/// Caches positive KT answers per `(user, key)` for `ttl`. Negative answers
/// and failures are never cached. The TTL bounds how long a key the user has
/// since rotated away from keeps verifying changes signed with it.
pub struct CachedKtVerifier<V> {
    inner: V,
    ttl: Duration,
    cache: Mutex<HashMap<(String, KeyBytes), Instant>>,
}

impl<V: IdentityKeyVerifier> CachedKtVerifier<V> {
    pub fn new(inner: V, ttl: Duration) -> Self {
        Self {
            inner,
            ttl,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Drop every cached answer for `user_id` (their identity changed).
    pub async fn forget_user(&self, user_id: &str) {
        self.cache.lock().await.retain(|(u, _), _| u != user_id);
    }
}

#[async_trait]
impl<V: IdentityKeyVerifier> IdentityKeyVerifier for CachedKtVerifier<V> {
    async fn is_current_identity_key(
        &self,
        user_id: &str,
        identity_key: &KeyBytes,
    ) -> Result<bool, VerifyError> {
        let key = (user_id.to_string(), *identity_key);
        if let Some(at) = self.cache.lock().await.get(&key) {
            if at.elapsed() < self.ttl {
                return Ok(true);
            }
        }
        let ok = self
            .inner
            .is_current_identity_key(user_id, identity_key)
            .await?;
        let mut cache = self.cache.lock().await;
        if ok {
            cache.insert(key, Instant::now());
        } else {
            cache.remove(&key);
        }
        Ok(ok)
    }
}
