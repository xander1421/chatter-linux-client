//! Verified channel membership: the state each channel's signed change log
//! has reached on this device, plus the log itself (migration v082). Written
//! only with states `services::channel::verified_state` has verified.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rusqlite::{params, OptionalExtension};

use crate::domain::ids::ChannelId;
use crate::error::StorageError;
use crate::proto::chat_v1 as pb;
use crate::services::channel::verified_state::{
    ChannelRole, Invite, JoinMode, KeyBytes, Member, MemberStatus, MembershipState, Policy, StateHash,
    VerificationStatus, VerifiedChange, VerifiedChannel,
};
use crate::storage::Database;

pub struct VerifiedMembershipRepository {
    db: Arc<Database>,
}

fn bad(what: &str) -> StorageError {
    StorageError::InvalidData(format!("verified_channel: {what}"))
}

fn role_of(v: i64) -> Result<ChannelRole, StorageError> {
    match i32::try_from(v).ok().and_then(|v| ChannelRole::try_from(v).ok()) {
        Some(ChannelRole::RoleUnspecified) | None => Err(bad("role")),
        Some(r) => Ok(r),
    }
}

fn key_of(v: Vec<u8>) -> Result<KeyBytes, StorageError> {
    v.try_into().map_err(|_| bad("key length"))
}

impl VerifiedMembershipRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Persist a verified change and the state it produced, atomically.
    /// Refuses (InvalidData) unless the stored revision is the change's
    /// predecessor, or nothing is stored yet (genesis / bootstrap) — so a
    /// concurrent apply cannot interleave. A `VerificationFailed` status is
    /// cleared — returns `true` when it was, so the caller can say so; `Fork`
    /// is kept until acknowledged.
    pub fn commit(
        &self,
        channel_id: &ChannelId,
        change: &pb::ChannelChange,
        verified: &VerifiedChange,
    ) -> Result<bool, StorageError> {
        let cid = channel_id.to_string();
        let st = &verified.0.new_state;
        if st.channel_id != cid {
            return Err(bad("state for another channel"));
        }
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        let stored: Option<(i64, u8)> = tx
            .query_row(
                "SELECT revision, status FROM verified_channel_state WHERE channel_id = ?1",
                params![cid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let cleared_failure = matches!(stored, Some((_, s)) if s == VerificationStatus::VerificationFailed as u8);
        let status = match stored {
            Some((rev, _)) if (rev as u64).wrapping_add(1) != st.revision => {
                return Err(bad("commit out of order"));
            }
            Some((_, s)) if s == VerificationStatus::Fork as u8 => VerificationStatus::Fork,
            _ => VerificationStatus::Ok,
        };
        let p = &st.policy;
        tx.execute(
            "INSERT INTO verified_channel_state (channel_id, revision, state_hash, policy_add_member,
                policy_remove_member, policy_set_role, policy_manage_invites, policy_set_disappearing,
                policy_join, disappearing_seconds, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(channel_id) DO UPDATE SET revision = ?2, state_hash = ?3,
                policy_add_member = ?4, policy_remove_member = ?5, policy_set_role = ?6,
                policy_manage_invites = ?7, policy_set_disappearing = ?8, policy_join = ?9,
                disappearing_seconds = ?10, status = ?11",
            params![
                cid,
                st.revision as i64,
                verified.0.new_state_hash.as_slice(),
                p.add_member as i32,
                p.remove_member as i32,
                p.set_role as i32,
                p.manage_invites as i32,
                p.set_disappearing as i32,
                p.join as i32,
                st.disappearing_seconds,
                status as u8,
            ],
        )?;
        tx.execute("DELETE FROM verified_channel_members WHERE channel_id = ?1", params![cid])?;
        for (user_id, m) in &st.members {
            tx.execute(
                "INSERT INTO verified_channel_members (channel_id, user_id, role, status, identity_key)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![cid, user_id, m.role as i32, m.status as i32, m.identity_key.as_slice()],
            )?;
        }
        tx.execute("DELETE FROM verified_channel_invites WHERE channel_id = ?1", params![cid])?;
        for (invite_pub, i) in &st.invites {
            tx.execute(
                "INSERT INTO verified_channel_invites
                    (channel_id, invite_pub, role, expires_at, max_uses, uses, needs_approval)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    cid,
                    invite_pub.as_slice(),
                    i.role as i32,
                    i.expires_at as i64,
                    i.max_uses,
                    i.uses,
                    i.needs_approval
                ],
            )?;
        }
        // An exclusion lasts until the user leaves the verified state.
        tx.execute(
            "DELETE FROM verified_channel_exclusions WHERE channel_id = ?1 AND user_id NOT IN
               (SELECT user_id FROM verified_channel_members WHERE channel_id = ?1)",
            params![cid],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO channel_changes
                (channel_id, revision, actions, signature, invite_proof, state_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                cid,
                st.revision as i64,
                change.actions,
                change.signature,
                change.invite_proof,
                verified.0.new_state_hash.as_slice()
            ],
        )?;
        tx.commit()?;
        Ok(cleared_failure)
    }

    /// The channel's verified state, or None if it has none. The hash is
    /// recomputed from the rows and must match the stored one.
    pub fn load(&self, channel_id: &ChannelId) -> Result<Option<VerifiedChannel>, StorageError> {
        let cid = channel_id.to_string();
        let conn = self.db.connection()?;
        let head = conn
            .query_row(
                "SELECT revision, state_hash, policy_add_member, policy_remove_member, policy_set_role,
                        policy_manage_invites, policy_set_disappearing, policy_join,
                        disappearing_seconds, status
                 FROM verified_channel_state WHERE channel_id = ?1",
                params![cid],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        [r.get::<_, i64>(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?],
                        r.get::<_, i32>(7)?,
                        r.get::<_, u32>(8)?,
                        r.get::<_, u8>(9)?,
                    ))
                },
            )
            .optional()?;
        let Some((revision, stored_hash, roles, join, disappearing_seconds, status)) = head else {
            return Ok(None);
        };
        let policy = Policy {
            add_member: role_of(roles[0])?,
            remove_member: role_of(roles[1])?,
            set_role: role_of(roles[2])?,
            manage_invites: role_of(roles[3])?,
            set_disappearing: role_of(roles[4])?,
            join: match JoinMode::try_from(join) {
                Ok(m @ (JoinMode::InviteOnly | JoinMode::Open)) => m,
                _ => return Err(bad("join mode")),
            },
        };

        let mut members = BTreeMap::new();
        let mut stmt = conn.prepare(
            "SELECT user_id, role, status, identity_key FROM verified_channel_members WHERE channel_id = ?1",
        )?;
        let rows = stmt.query_map(params![cid], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i32>(2)?, r.get::<_, Vec<u8>>(3)?))
        })?;
        for row in rows {
            let (user_id, role, status, key) = row?;
            let status = match MemberStatus::try_from(status) {
                Ok(s @ (MemberStatus::Active | MemberStatus::Invited | MemberStatus::Requesting)) => s,
                _ => return Err(bad("member status")),
            };
            members.insert(user_id, Member { role: role_of(role)?, status, identity_key: key_of(key)? });
        }
        drop(stmt);

        let mut invites = BTreeMap::new();
        let mut stmt = conn.prepare(
            "SELECT invite_pub, role, expires_at, max_uses, uses, needs_approval
             FROM verified_channel_invites WHERE channel_id = ?1",
        )?;
        let rows = stmt.query_map(params![cid], |r| {
            Ok((
                r.get::<_, Vec<u8>>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, u32>(3)?,
                r.get::<_, u32>(4)?,
                r.get::<_, bool>(5)?,
            ))
        })?;
        for row in rows {
            let (invite_pub, role, expires_at, max_uses, uses, needs_approval) = row?;
            invites.insert(
                key_of(invite_pub)?,
                Invite { role: role_of(role)?, expires_at: expires_at as u64, max_uses, uses, needs_approval },
            );
        }
        drop(stmt);

        let mut stmt = conn.prepare("SELECT user_id FROM verified_channel_exclusions WHERE channel_id = ?1")?;
        let excluded: BTreeSet<String> =
            stmt.query_map(params![cid], |r| r.get::<_, String>(0))?.collect::<Result<_, _>>()?;
        drop(stmt);

        let state = MembershipState {
            channel_id: cid,
            revision: revision as u64,
            members,
            policy,
            invites,
            disappearing_seconds,
        };
        let state_hash = state.hash();
        if stored_hash.as_slice() != state_hash.as_slice() {
            return Err(bad("stored state does not match its hash"));
        }
        let status = VerificationStatus::from_u8(status).ok_or_else(|| bad("status"))?;
        Ok(Some(VerifiedChannel { state, state_hash, excluded, status }))
    }

    /// Cheap eligibility check for the key-gating hot path: ACTIVE in the
    /// verified state and not excluded. Equals `VerifiedMembership::contains`.
    pub fn is_distribution_member(&self, channel_id: &ChannelId, user_id: &str) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        Ok(conn
            .query_row(
                "SELECT 1 FROM verified_channel_members m
                 WHERE m.channel_id = ?1 AND m.user_id = ?2 AND m.status = ?3
                   AND NOT EXISTS (SELECT 1 FROM verified_channel_exclusions e
                                   WHERE e.channel_id = m.channel_id AND e.user_id = m.user_id)",
                params![channel_id.to_string(), user_id, MemberStatus::Active as i32],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Channels whose distribution set holds `user_id` (ACTIVE, not excluded):
    /// the channels that must rotate when the server says the account is gone.
    pub fn channels_distributing_to(&self, user_id: &str) -> Result<Vec<ChannelId>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT m.channel_id FROM verified_channel_members m
             WHERE m.user_id = ?1 AND m.status = ?2
               AND NOT EXISTS (SELECT 1 FROM verified_channel_exclusions e
                               WHERE e.channel_id = m.channel_id AND e.user_id = m.user_id)",
        )?;
        let ids: Vec<String> =
            stmt.query_map(params![user_id, MemberStatus::Active as i32], |r| r.get(0))?.collect::<Result<_, _>>()?;
        ids.iter().map(|id| id.parse().map_err(|_| bad("channel id"))).collect()
    }

    /// Apply an unsigned server removal (`UnsignedDisposition::ExcludeFromDistribution`).
    /// A no-op for a user not in the verified state.
    pub fn exclude(&self, channel_id: &ChannelId, user_id: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT OR IGNORE INTO verified_channel_exclusions (channel_id, user_id)
             SELECT channel_id, user_id FROM verified_channel_members WHERE channel_id = ?1 AND user_id = ?2",
            params![channel_id.to_string(), user_id],
        )?;
        Ok(())
    }

    /// Set the local status (a failed verification, a detected fork, or the
    /// user acknowledging one). No-op for a channel with no verified state.
    pub fn set_status(&self, channel_id: &ChannelId, status: VerificationStatus) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE verified_channel_state SET status = ?2 WHERE channel_id = ?1",
            params![channel_id.to_string(), status as u8],
        )?;
        Ok(())
    }

    /// The state hash recorded for `revision`, if this device accepted that
    /// change (for `compare_peer_state` against a peer that is behind).
    pub fn state_hash_at(&self, channel_id: &ChannelId, revision: u64) -> Result<Option<StateHash>, StorageError> {
        let conn = self.db.connection()?;
        let h: Option<Vec<u8>> = conn
            .query_row(
                "SELECT state_hash FROM channel_changes WHERE channel_id = ?1 AND revision = ?2",
                params![channel_id.to_string(), revision as i64],
                |r| r.get(0),
            )
            .optional()?;
        h.map(|h| h.try_into().map_err(|_| bad("hash length"))).transpose()
    }

    /// The accepted change at `revision`, as received.
    pub fn change_at(&self, channel_id: &ChannelId, revision: u64) -> Result<Option<pb::ChannelChange>, StorageError> {
        let conn = self.db.connection()?;
        Ok(conn
            .query_row(
                "SELECT actions, signature, invite_proof FROM channel_changes
                 WHERE channel_id = ?1 AND revision = ?2",
                params![channel_id.to_string(), revision as i64],
                |r| Ok(pb::ChannelChange { actions: r.get(0)?, signature: r.get(1)?, invite_proof: r.get(2)? }),
            )
            .optional()?)
    }

    /// Forget a channel entirely (we left it, or it was deleted).
    pub fn delete(&self, channel_id: &ChannelId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute("DELETE FROM verified_channel_state WHERE channel_id = ?1", params![channel_id.to_string()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::channel::verified_state::{
        canonical::{signed_message, CHANGE_DOMAIN},
        check_change, rules::preview, VerifiedMembership,
    };
    use libsignal_protocol::KeyPair;
    use pb::channel_action::Action;
    use prost::Message;

    fn repo() -> (tempfile::TempDir, VerifiedMembershipRepository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("v.db"), "testpass").expect("open"));
        db.run_migrations().expect("migrations");
        (dir, VerifiedMembershipRepository::new(db))
    }

    fn key(kp: &KeyPair) -> Vec<u8> {
        kp.public_key.serialize().to_vec()
    }

    /// `by` authors `acts` on `prev` for `cid`; returns the change and its verification.
    fn change(
        cid: &str,
        prev: Option<&MembershipState>,
        by: (&str, &KeyPair),
        acts: Vec<Action>,
    ) -> (pb::ChannelChange, VerifiedChange) {
        let revision = prev.map_or(1, |p| p.revision + 1);
        let actions: Vec<_> = acts.into_iter().map(|a| pb::ChannelAction { action: Some(a) }).collect();
        let k: KeyBytes = key(by.1).try_into().unwrap();
        let new = preview(prev, &actions, cid, revision, by.0, k, 1).unwrap();
        let bytes = pb::channel_change::Actions {
            channel_id: cid.into(),
            revision,
            prev_state_hash: prev.map(|p| p.hash().to_vec()).unwrap_or_default(),
            author_user_id: by.0.into(),
            author_identity_key: key(by.1),
            timestamp: 1,
            actions,
            new_state_hash: new.hash().to_vec(),
        }
        .encode_to_vec();
        let signature = by.1.private_key.calculate_signature(&signed_message(CHANGE_DOMAIN, &bytes), &mut rand::rng()).unwrap();
        let c = pb::ChannelChange { actions: bytes, signature: signature.to_vec(), invite_proof: vec![] };
        let v = VerifiedChange(check_change(cid, prev, &c).unwrap());
        (c, v)
    }

    #[test]
    fn commit_load_roundtrip_and_ordering() {
        let (_dir, repo) = repo();
        let channel = ChannelId::new();
        let cid = channel.to_string();
        let owner = (uuid::Uuid::new_v4().to_string(), KeyPair::generate(&mut rand::rng()));
        let bob = (uuid::Uuid::new_v4().to_string(), KeyPair::generate(&mut rand::rng()));
        let link = KeyPair::generate(&mut rand::rng());
        assert!(repo.load(&channel).unwrap().is_none());

        let (c1, v1) = change(
            &cid,
            None,
            (&owner.0, &owner.1),
            vec![
                Action::Genesis(pb::Genesis { policy: Some(Policy::defaults(false).to_proto()), disappearing_seconds: 30 }),
                Action::AddMember(pb::AddMember { user_id: bob.0.clone(), role: ChannelRole::Member as i32, identity_key: key(&bob.1), pending: false }),
                Action::CreateInvite(pb::CreateInvite {
                    capability: Some(pb::InviteCapability {
                        invite_pub: key(&link),
                        role: ChannelRole::Member as i32,
                        expires_at: u64::MAX, // stored bit-cast, read back exactly
                        max_uses: 2,
                        needs_approval: true,
                        uses: 0,
                    }),
                }),
            ],
        );
        repo.commit(&channel, &c1, &v1).unwrap();
        let loaded = repo.load(&channel).unwrap().unwrap();
        assert_eq!(loaded.state, v1.0.new_state);
        assert_eq!(loaded.state_hash, v1.0.new_state_hash);
        assert_eq!(loaded.status, VerificationStatus::Ok);
        assert_eq!(repo.state_hash_at(&channel, 1).unwrap(), Some(v1.0.new_state_hash));
        assert_eq!(repo.change_at(&channel, 1).unwrap(), Some(c1.clone()));
        assert!(repo.is_distribution_member(&channel, &bob.0).unwrap());

        // Re-committing revision 1 over revision 1 is out of order.
        assert!(repo.commit(&channel, &c1, &v1).is_err());

        // Unsigned removal: excluded, still in state; status survives a commit
        // only when it is a fork.
        repo.exclude(&channel, &bob.0).unwrap();
        repo.exclude(&channel, "00000000-0000-4000-8000-000000000000").unwrap();
        let view = repo.load(&channel).unwrap().unwrap();
        assert!(!view.contains(&bob.0));
        assert!(view.state.members.contains_key(&bob.0));
        assert_eq!(view.excluded.len(), 1, "only verified members can be excluded");
        assert!(!repo.is_distribution_member(&channel, &bob.0).unwrap());

        repo.set_status(&channel, VerificationStatus::Fork).unwrap();
        let s1 = view.state.clone();
        let (c2, v2) = change(
            &cid,
            Some(&s1),
            (&owner.0, &owner.1),
            vec![Action::SetDisappearing(pb::SetDisappearing { seconds: 5 })],
        );
        repo.commit(&channel, &c2, &v2).unwrap();
        let after = repo.load(&channel).unwrap().unwrap();
        assert_eq!(after.status, VerificationStatus::Fork);
        assert!(after.excluded.contains(&bob.0), "exclusion lasts while the user is in state");

        // A signed removal ends the exclusion.
        repo.set_status(&channel, VerificationStatus::VerificationFailed).unwrap();
        let (c3, v3) = change(
            &cid,
            Some(&after.state),
            (&owner.0, &owner.1),
            vec![Action::RemoveMember(pb::RemoveMember { user_id: bob.0.clone(), reason: pb::remove_member::Reason::Kicked as i32 })],
        );
        assert!(repo.commit(&channel, &c3, &v3).unwrap(), "clearing a failure is reported");
        let removed = repo.load(&channel).unwrap().unwrap();
        assert!(removed.excluded.is_empty());
        assert_eq!(removed.status, VerificationStatus::Ok, "a verified change clears a failure");
        assert_eq!(removed.revision(), 3);

        repo.delete(&channel).unwrap();
        assert!(repo.load(&channel).unwrap().is_none());
        assert!(repo.state_hash_at(&channel, 1).unwrap().is_none());
    }
}
