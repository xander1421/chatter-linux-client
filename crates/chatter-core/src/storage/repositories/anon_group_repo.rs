//! Anonymous group management repository.
//!
//! Data access for NDSS'21 style anonymous groups.
//! Message operations are handled by the unified MessageRepository.

use crate::domain::UserId;
use crate::domain::anon_group::{
    AnonGroup, AnonGroupId, AnonGroupMailbox, AnonGroupMember, AnonGroupPending, AnonGroupRole,
    PendingActionType,
};
use crate::error::StorageError;
use crate::storage::Database;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use std::sync::Arc;

/// Anonymous group data repository.
///
/// Provides CRUD operations for anonymous groups and their members.
pub struct AnonGroupRepository {
    db: Arc<Database>,
}

/// Invitation retained until the recipient accepts or declines it.
#[derive(Debug, Clone)]
pub struct PendingAnonGroupInvite {
    pub group_id: AnonGroupId,
    pub group_name: String,
    pub inviter_id: UserId,
    pub inviter_device_id: u32,
    pub inviter_mailbox_pk: [u8; 32],
    pub profile_key: Option<[u8; 32]>,
    pub inviter_display_name: Option<String>,
    pub inviter_avatar: Option<Vec<u8>>,
}

impl AnonGroupRepository {
    pub fn save_pending_invite(&self, invite: &PendingAnonGroupInvite) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute("INSERT INTO pending_anon_group_invites
            (group_id, group_name, inviter_id, inviter_device_id, inviter_mailbox_pk,
             profile_key, inviter_display_name, inviter_avatar)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(group_id) DO UPDATE SET
                group_name=excluded.group_name, inviter_device_id=excluded.inviter_device_id,
                inviter_mailbox_pk=excluded.inviter_mailbox_pk, profile_key=excluded.profile_key,
                inviter_display_name=excluded.inviter_display_name,
                inviter_avatar=COALESCE(excluded.inviter_avatar, pending_anon_group_invites.inviter_avatar)
            WHERE inviter_id=excluded.inviter_id",
            params![invite.group_id.to_string(), invite.group_name, invite.inviter_id.to_string(),
                invite.inviter_device_id, invite.inviter_mailbox_pk.as_slice(),
                invite.profile_key.as_ref().map(|v| v.as_slice()), invite.inviter_display_name,
                invite.inviter_avatar])?;
        Ok(())
    }

    pub fn list_pending_invites(&self) -> Result<Vec<PendingAnonGroupInvite>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT group_id, group_name, inviter_id, inviter_device_id,
            inviter_mailbox_pk, profile_key, inviter_display_name, inviter_avatar
            FROM pending_anon_group_invites ORDER BY received_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let group_id: String = row.get(0)?;
            let inviter_id: String = row.get(2)?;
            let mailbox: Vec<u8> = row.get(4)?;
            let profile: Option<Vec<u8>> = row.get(5)?;
            Ok((
                group_id,
                row.get::<_, String>(1)?,
                inviter_id,
                row.get::<_, u32>(3)?,
                mailbox,
                profile,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<Vec<u8>>>(7)?,
            ))
        })?;
        rows.map(|row| {
            let (
                group_id,
                group_name,
                inviter_id,
                inviter_device_id,
                mailbox,
                profile_key,
                inviter_display_name,
                inviter_avatar,
            ) = row?;
            Ok(PendingAnonGroupInvite {
                group_id: group_id
                    .parse()
                    .map_err(|_| StorageError::NotFound("Invalid invite group ID".into()))?,
                group_name,
                inviter_id: inviter_id
                    .parse()
                    .map_err(|_| StorageError::NotFound("Invalid inviter ID".into()))?,
                inviter_device_id,
                inviter_mailbox_pk: mailbox
                    .try_into()
                    .map_err(|_| StorageError::NotFound("Invalid inviter mailbox".into()))?,
                profile_key: profile_key
                    .map(|key| {
                        key.try_into().map_err(|_| {
                            StorageError::NotFound("Invalid inviter profile key".into())
                        })
                    })
                    .transpose()?,
                inviter_display_name,
                inviter_avatar,
            })
        })
        .collect()
    }

    pub fn remove_pending_invite(&self, group_id: &AnonGroupId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM pending_anon_group_invites WHERE group_id=?1",
            [group_id.to_string()],
        )?;
        Ok(())
    }

    pub fn update_pending_invite_avatar(
        &self,
        group_id: &AnonGroupId,
        avatar: &[u8],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE pending_anon_group_invites SET inviter_avatar=?2 WHERE group_id=?1",
            params![group_id.to_string(), avatar],
        )?;
        Ok(())
    }
    /// Create a new AnonGroupRepository with the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn current_recipient(
        &self,
        group: &AnonGroupId,
        user: &UserId,
        key: &[u8; 32],
        timestamp: u64,
    ) -> Result<bool, StorageError> {
        let timestamp = membership_timestamp(timestamp)?;
        let conn = self.db.connection()?;
        Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM anon_group_members m JOIN anon_groups g ON g.id = m.group_id
            LEFT JOIN anon_group_member_order o ON o.group_id = m.group_id AND o.user_id = m.user_id
            WHERE m.group_id = ?1 AND m.user_id = ?2 AND m.is_active = 1 AND m.mailbox_pk = ?3
            AND (COALESCE(o.removed_at,0) = 0 OR o.admitted_at > o.removed_at)
            AND COALESCE(o.admitted_at,0) <= ?4)",params![group.to_string(),user.to_string(),key.as_slice(),timestamp],|r|r.get(0))?)
    }

    /// A local join waits until an owner roster explicitly includes this
    /// admission. Local and owner wall clocks cannot be compared for this.
    pub fn awaiting_join_roster(
        &self,
        group: &AnonGroupId,
        me: &UserId,
    ) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let waiting: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM anon_groups g
             JOIN anon_group_member_order m ON m.group_id=g.id AND m.user_id=?2
             WHERE g.id=?1 AND g.owner_id != ?2
             AND m.acknowledged_at < m.admitted_at)",
            params![group.to_string(), me.to_string()],
            |row| row.get(0),
        )?;
        Ok(waiting != 0)
    }

    pub fn member_at(
        &self,
        group: &AnonGroupId,
        user: &UserId,
        timestamp: u64,
    ) -> Result<bool, StorageError> {
        let timestamp = membership_timestamp(timestamp)?;
        let conn = self.db.read_connection()?;
        let member: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM anon_group_members m
             LEFT JOIN anon_group_member_order o ON o.group_id=m.group_id AND o.user_id=m.user_id
             WHERE m.group_id=?1 AND m.user_id=?2 AND m.is_active=1
             AND (COALESCE(o.removed_at,0)=0 OR o.admitted_at>o.removed_at)
             AND COALESCE(o.admitted_at,0)<=?3)",
            params![group.to_string(), user.to_string(), timestamp],
            |row| row.get(0),
        )?;
        Ok(member != 0)
    }

    pub fn next_membership_timestamp(
        &self,
        group: &AnonGroupId,
        now: u64,
    ) -> Result<u64, StorageError> {
        use rusqlite::OptionalExtension;
        let conn = self.db.connection()?;
        let member_clock: i64 = conn.query_row("SELECT COALESCE(MAX(MAX(admitted_at,CASE WHEN removed_at = 9223372036854775807 THEN 0 ELSE removed_at END,CASE WHEN changed_at = 9223372036854775807 THEN 0 ELSE changed_at END)),0) FROM anon_group_member_order WHERE group_id = ?1", [group.to_string()], |r|r.get(0))?;
        let group_clock: i64 = conn
            .query_row(
                "SELECT MAX(roster_at,update_at) FROM anon_group_order WHERE group_id = ?1",
                [group.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let next = member_clock
            .max(group_clock)
            .checked_add(1)
            .ok_or_else(|| StorageError::NotFound("Anonymous membership clock exhausted".into()))?;
        Ok(membership_timestamp(now)?.max(next) as u64)
    }

    /// Snapshot and clock allocation share the writer transaction: a rename,
    /// role change, invitation or removal cannot race into an older snapshot
    /// that is then labelled with a newer timestamp.
    pub fn ordered_roster_snapshot(
        &self,
        group: &AnonGroupId,
        owner: &UserId,
        now: u64,
    ) -> Result<(Vec<(AnonGroupMember, u64)>, u64), StorageError> {
        use rusqlite::OptionalExtension;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let actual_owner: String = tx.query_row(
            "SELECT owner_id FROM anon_groups WHERE id = ?1",
            [group.to_string()],
            |r| r.get(0),
        )?;
        if actual_owner != owner.to_string() {
            return Err(StorageError::NotFound(
                "Anonymous group owner changed".into(),
            ));
        }
        let members = {
            let mut stmt = tx.prepare("SELECT id,group_id,user_id,role,mailbox_pk,joined_at,profile_key,display_name,avatar,device_id FROM anon_group_members WHERE group_id = ?1 AND is_active = 1 ORDER BY joined_at")?;
            let rows = stmt.query_map([group.to_string()], row_to_member)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut snapshot = Vec::with_capacity(members.len());
        let mut clock = membership_timestamp(now)?;
        for member in members {
            let (admitted,removed,changed): (i64,i64,i64) = tx.query_row("SELECT admitted_at,removed_at,changed_at FROM anon_group_member_order WHERE group_id = ?1 AND user_id = ?2",
                params![group.to_string(),member.user_id.to_string()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.unwrap_or((0,0,0));
            if removed > 0 && admitted <= removed {
                continue;
            }
            clock = clock.max(
                admitted
                    .max(removed)
                    .max(changed)
                    .checked_add(1)
                    .ok_or_else(|| {
                        StorageError::NotFound("Anonymous membership clock exhausted".into())
                    })?,
            );
            if removed == 0 || admitted > removed {
                snapshot.push((member, admitted as u64));
            }
        }
        let group_clock: i64 = tx
            .query_row(
                "SELECT MAX(roster_at,update_at) FROM anon_group_order WHERE group_id = ?1",
                [group.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        clock = clock.max(group_clock.checked_add(1).ok_or_else(|| {
            StorageError::NotFound("Anonymous membership clock exhausted".into())
        })?);
        tx.execute("INSERT INTO anon_group_order(group_id,roster_at) VALUES (?1,?2) ON CONFLICT(group_id) DO UPDATE SET roster_at = excluded.roster_at",
            params![group.to_string(),clock])?;
        tx.commit()?;
        Ok((snapshot, clock as u64))
    }

    pub fn apply_ordered_update(
        &self,
        group: &AnonGroupId,
        sender: &UserId,
        me: &UserId,
        update: &crate::proto::content_v1::AnonGroupUpdate,
    ) -> Result<bool, StorageError> {
        use crate::proto::content_v1::anon_group_update::Update;
        use rusqlite::OptionalExtension;
        let timestamp = membership_timestamp(update.timestamp)?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let state: Option<(String, i32)> = tx
            .query_row(
                "SELECT owner_id,my_role FROM anon_groups WHERE id = ?1",
                [group.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((owner, my_role)) = state else {
            return Ok(false);
        };
        let role: Option<i32> = tx
            .query_row(
                "SELECT role FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2 AND is_active = 1",
                params![group.to_string(), sender.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if sender.to_string() != owner
            && !role.is_some_and(|r| AnonGroupRole::from_i32(r).can_kick())
        {
            return Ok(false);
        }
        let clock: i64 = tx
            .query_row(
                "SELECT MAX(roster_at,update_at) FROM anon_group_order WHERE group_id = ?1",
                [group.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if timestamp <= clock && clock > 0 {
            return Ok(false);
        }
        match &update.update {
            Some(Update::NewName(name)) => {
                tx.execute(
                    "UPDATE anon_groups SET name = ?1 WHERE id = ?2",
                    params![name, group.to_string()],
                )?;
            }
            Some(Update::RoleChange(change)) => {
                if owner != sender.to_string() {
                    return Ok(false);
                }
                let Some(target) = UserId::try_from_slice(&change.user_id) else {
                    return Ok(false);
                };
                if target == *sender || change.timestamp != update.timestamp {
                    return Ok(false);
                }
                let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2 AND is_active = 1)",params![group.to_string(),target.to_string()], |r|r.get(0))?;
                if !exists {
                    return Ok(false);
                }
                let changed: i64 = tx.query_row("SELECT changed_at FROM anon_group_member_order WHERE group_id = ?1 AND user_id = ?2", params![group.to_string(),target.to_string()], |r|r.get(0)).optional()?.unwrap_or(0);
                if timestamp <= changed && changed > 0 {
                    return Ok(false);
                }
                let role = AnonGroupRole::from_i32(change.new_role);
                if role == AnonGroupRole::Owner {
                    let my_role = if target == *me {
                        AnonGroupRole::Owner.to_i32()
                    } else if *sender == *me {
                        AnonGroupRole::Admin.to_i32()
                    } else {
                        my_role
                    };
                    tx.execute(
                        "UPDATE anon_groups SET owner_id = ?1,my_role = ?2 WHERE id = ?3",
                        params![target.to_string(), my_role, group.to_string()],
                    )?;
                    tx.execute("UPDATE anon_group_members SET role = ?1 WHERE group_id = ?2 AND user_id = ?3",params![AnonGroupRole::Admin.to_i32(),group.to_string(),sender.to_string()])?;
                    record_member_change(&tx, group, sender, timestamp)?;
                } else if target == *me {
                    tx.execute(
                        "UPDATE anon_groups SET my_role = ?1 WHERE id = ?2",
                        params![role.to_i32(), group.to_string()],
                    )?;
                }
                tx.execute(
                    "UPDATE anon_group_members SET role = ?1 WHERE group_id = ?2 AND user_id = ?3",
                    params![role.to_i32(), group.to_string(), target.to_string()],
                )?;
                record_member_change(&tx, group, &target, timestamp)?;
            }
            None => return Ok(false),
        }
        tx.execute("INSERT INTO anon_group_order(group_id,update_at) VALUES (?1,?2) ON CONFLICT(group_id) DO UPDATE SET update_at = excluded.update_at",params![group.to_string(),timestamp])?;
        tx.commit()?;
        Ok(true)
    }

    /// JoinAck updates only a current incarnation; it never inserts a member.
    pub fn apply_ordered_join_ack(
        &self,
        member: &AnonGroupMember,
        timestamp: u64,
    ) -> Result<bool, StorageError> {
        use rusqlite::OptionalExtension;
        let timestamp = membership_timestamp(timestamp)?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let can_handle: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM anon_groups WHERE id = ?1 AND my_role IN (1,2))",
            [member.group_id.to_string()],
            |r| r.get(0),
        )?;
        if !can_handle {
            return Ok(false);
        }
        let existing: Option<Option<Vec<u8>>> = tx
            .query_row(
                "SELECT mailbox_pk FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2 AND is_active = 1",
                params![member.group_id.to_string(), member.user_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(existing_key) = existing else {
            return Ok(false);
        };
        let (admitted,removed,acknowledged): (i64,i64,i64) = tx.query_row("SELECT admitted_at,removed_at,acknowledged_at FROM anon_group_member_order WHERE group_id = ?1 AND user_id = ?2",
            params![member.group_id.to_string(),member.user_id.to_string()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?.unwrap_or((0,0,0));
        let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM anon_group_pending WHERE group_id = ?1 AND user_id = ?2 AND action = 'invite')",
            params![member.group_id.to_string(),member.user_id.to_string()], |r|r.get(0))?;
        if (removed > 0
            && (admitted <= removed
                || timestamp <= admitted
                || (!pending && existing_key.is_none())))
            || (timestamp <= acknowledged && acknowledged > 0)
        {
            return Ok(false);
        }
        let local_clock: i64 = tx.query_row("SELECT MAX(COALESCE((SELECT MAX(roster_at,update_at) FROM anon_group_order WHERE group_id = ?1),0),COALESCE((SELECT MAX(changed_at,admitted_at) FROM anon_group_member_order WHERE group_id = ?1 AND user_id = ?2),0))", params![member.group_id.to_string(),member.user_id.to_string()], |r|r.get(0))?;
        let changed_at = Utc::now().timestamp_nanos_opt().unwrap_or(0).max(
            local_clock.checked_add(1).ok_or_else(|| {
                StorageError::InvalidData("Anonymous membership clock exhausted".into())
            })?,
        );
        record_member_change(&tx, &member.group_id, &member.user_id, changed_at)?;
        tx.execute("UPDATE anon_group_members SET mailbox_pk = ?1,profile_key = COALESCE(?2,profile_key),display_name = COALESCE(?3,display_name),avatar = COALESCE(?4,avatar),device_id = COALESCE(?7,device_id) WHERE group_id = ?5 AND user_id = ?6 AND is_active = 1",
            params![member.mailbox_pk.as_ref().map(|v|v.as_slice()),member.profile_key.as_ref().map(|v|v.as_slice()),member.display_name,member.avatar,member.group_id.to_string(),member.user_id.to_string(),member.device_id])?;
        tx.execute("INSERT INTO anon_group_member_order(group_id,user_id,acknowledged_at) VALUES (?1,?2,?3) ON CONFLICT(group_id,user_id) DO UPDATE SET acknowledged_at = excluded.acknowledged_at",
            params![member.group_id.to_string(),member.user_id.to_string(),timestamp])?;
        tx.execute("DELETE FROM anon_group_pending WHERE group_id = ?1 AND user_id = ?2 AND action = 'invite'",params![member.group_id.to_string(),member.user_id.to_string()])?;
        tx.commit()?;
        Ok(true)
    }

    /// Whether an owner roster has already initialized this local group.
    pub fn has_applied_roster(&self, group: &AnonGroupId) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let roster_at: i64 = conn.query_row(
            "SELECT COALESCE((SELECT roster_at FROM anon_group_order WHERE group_id = ?1), 0)",
            [group.to_string()],
            |row| row.get(0),
        )?;
        Ok(roster_at > 0)
    }

    /// Apply one owner-authenticated snapshot atomically with its ordering state.
    /// A fresh snapshot is not evidence of a fresh invitation: only admission
    /// minted by an explicit invitation may supersede a removal tombstone.
    pub fn apply_ordered_roster(
        &self,
        group: &AnonGroupId,
        owner: &UserId,
        me: &UserId,
        timestamp: u64,
        members: &[(AnonGroupMember, u64)],
    ) -> Result<bool, StorageError> {
        use rusqlite::OptionalExtension;
        let timestamp = membership_timestamp(timestamp)?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let actual_owner: Option<String> = tx
            .query_row(
                "SELECT owner_id FROM anon_groups WHERE id = ?1",
                [group.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if actual_owner.as_deref() != Some(owner.to_string().as_str()) {
            return Ok(false);
        }
        let roster_at: i64 = tx
            .query_row(
                "SELECT roster_at FROM anon_group_order WHERE group_id = ?1",
                [group.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if timestamp == 0 || timestamp <= roster_at {
            return Ok(false);
        }
        for (member, admission) in members {
            let admission = membership_timestamp(*admission)?;
            if admission > timestamp {
                return Ok(false);
            }
            let (old_admission, removed, changed): (i64, i64, i64) = tx.query_row(
                "SELECT admitted_at, removed_at, changed_at FROM anon_group_member_order WHERE group_id = ?1 AND user_id = ?2",
                params![group.to_string(), member.user_id.to_string()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            ).optional()?.unwrap_or((0, 0, 0));
            // The owner can have minted the admission before our local clock
            // recorded acceptance. Confirm the local generation separately;
            // never make the owner's roster timestamp compete with that clock.
            if member.user_id == *me
                && old_admission > roster_at
                && (removed == 0 || admission > removed)
            {
                tx.execute(
                    "UPDATE anon_group_member_order SET acknowledged_at=admitted_at
                    WHERE group_id=?1 AND user_id=?2 AND admitted_at=?3",
                    params![group.to_string(), member.user_id.to_string(), old_admission],
                )?;
            }
            if (removed > 0 && (admission <= removed || timestamp <= removed))
                || admission < old_admission
                || timestamp < changed
                || (timestamp == changed && changed > 0)
            {
                continue;
            }
            // Preserve the owner's authority; a list cannot invent another owner.
            if (member.role == AnonGroupRole::Owner) != (member.user_id == *owner) {
                return Ok(false);
            }
            write_ordered_member(&tx, member)?;
            tx.execute("INSERT INTO anon_group_member_order(group_id,user_id,admitted_at,changed_at)
                VALUES (?1,?2,?3,?4) ON CONFLICT(group_id,user_id) DO UPDATE SET
                admitted_at = MAX(admitted_at,excluded.admitted_at), changed_at = MAX(changed_at,excluded.changed_at)",
                params![group.to_string(), member.user_id.to_string(), admission, timestamp])?;
        }
        tx.execute(
            "INSERT INTO anon_group_order(group_id,roster_at) VALUES (?1,?2)
            ON CONFLICT(group_id) DO UPDATE SET roster_at = MAX(roster_at,excluded.roster_at)",
            params![group.to_string(), timestamp],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Verify current authority/key again under the same writer transaction as
    /// the removal. None means a pairwise-authenticated self leave.
    pub fn remove_ordered_member(
        &self,
        group: &AnonGroupId,
        target: &UserId,
        timestamp: u64,
        kicker: Option<(&UserId, &[u8; 32])>,
        delete_group: bool,
    ) -> Result<bool, StorageError> {
        use rusqlite::OptionalExtension;
        let timestamp = membership_timestamp(timestamp)?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let owner: Option<String> = tx
            .query_row(
                "SELECT owner_id FROM anon_groups WHERE id = ?1",
                [group.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let Some(owner) = owner else {
            return Ok(false);
        };
        if owner == target.to_string() && !(kicker.is_none() && delete_group) {
            return Ok(false);
        }
        if owner == target.to_string() {
            let others: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM anon_group_members WHERE group_id = ?1 AND user_id != ?2 AND is_active = 1)",params![group.to_string(),target.to_string()],|r|r.get(0))?;
            if others {
                return Ok(false);
            }
        }
        if let Some((actor, verified_key)) = kicker {
            let authority: Option<(i32, Option<Vec<u8>>)> = tx.query_row(
                "SELECT role,mailbox_pk FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2 AND is_active = 1",
                params![group.to_string(), actor.to_string()], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
            if !authority.is_some_and(|(role, key)| {
                (actor.to_string() == owner || AnonGroupRole::from_i32(role).can_kick())
                    && key.as_deref() == Some(verified_key.as_slice())
            }) {
                return Ok(false);
            }
        }
        let (admitted, removed): (i64, i64) = tx.query_row(
            "SELECT admitted_at,removed_at FROM anon_group_member_order WHERE group_id = ?1 AND user_id = ?2",
            params![group.to_string(), target.to_string()], |r| Ok((r.get(0)?,r.get(1)?))).optional()?.unwrap_or((0,0));
        // Old removals cannot evict a later explicitly invited incarnation.
        if timestamp == 0 || timestamp < admitted || timestamp <= removed {
            return Ok(false);
        }
        tx.execute("INSERT INTO anon_group_member_order(group_id,user_id,removed_at,changed_at)
            VALUES (?1,?2,?3,?3) ON CONFLICT(group_id,user_id) DO UPDATE SET
            removed_at = MAX(removed_at,excluded.removed_at), changed_at = MAX(changed_at,excluded.changed_at)",
            params![group.to_string(), target.to_string(), timestamp])?;
        tx.execute(
            "UPDATE anon_group_members SET is_active = 0, mailbox_pk = NULL, device_id = NULL WHERE group_id = ?1 AND user_id = ?2",
            params![group.to_string(), target.to_string()],
        )?;
        tx.execute(
            "DELETE FROM anon_group_pending WHERE group_id = ?1 AND user_id = ?2",
            params![group.to_string(), target.to_string()],
        )?;
        tx.execute("DELETE FROM pending_anon_group_messages WHERE group_id = ?1 AND recipient_user_id = ?2",
            params![group.to_string(), target.to_string()])?;
        if delete_group {
            tx.execute("DELETE FROM anon_groups WHERE id = ?1", [group.to_string()])?;
            tx.execute(
                "DELETE FROM pending_anon_group_messages WHERE group_id = ?1",
                [group.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Local user consent to an invitation is a self-only replay barrier.
    /// It is not evidence that any remote recipient has been readmitted.
    pub fn accept_ordered_self(
        &self,
        member: &AnonGroupMember,
        now: u64,
    ) -> Result<(), StorageError> {
        use rusqlite::OptionalExtension;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let previous: i64 = tx.query_row("SELECT MAX(admitted_at,removed_at,changed_at) FROM anon_group_member_order WHERE group_id = ?1 AND user_id = ?2",
            params![member.group_id.to_string(),member.user_id.to_string()],|r|r.get(0)).optional()?.unwrap_or(0);
        let group_clock: i64 = tx
            .query_row(
                "SELECT MAX(roster_at,update_at) FROM anon_group_order WHERE group_id = ?1",
                [member.group_id.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let admission =
            membership_timestamp(now)?.max(previous.max(group_clock).checked_add(1).ok_or_else(
                || StorageError::InvalidData("Anonymous membership clock exhausted".into()),
            )?);
        write_ordered_member(&tx, member)?;
        tx.execute("INSERT INTO anon_group_member_order(group_id,user_id,admitted_at,changed_at) VALUES (?1,?2,?3,?3)
            ON CONFLICT(group_id,user_id) DO UPDATE SET admitted_at = excluded.admitted_at,changed_at = excluded.changed_at",
            params![member.group_id.to_string(),member.user_id.to_string(),admission])?;
        tx.commit()?;
        Ok(())
    }

    /// Explicit local invitation is the ONLY producer of a new admission.
    /// Allocation advances above this member's clocks and group roster/update clocks.
    pub fn invite_ordered_member(
        &self,
        member: &AnonGroupMember,
        actor: &UserId,
        now: u64,
    ) -> Result<bool, StorageError> {
        use rusqlite::OptionalExtension;
        let now = membership_timestamp(now)?;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let role: Option<i32> = tx
            .query_row(
                "SELECT role FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2 AND is_active = 1",
                params![member.group_id.to_string(), actor.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        // Invitations bootstrap the sender as owner on v1 peers. Check the
        // persisted owner and role inside the admission transaction; an admin
        // cannot create an authoritative admission on the owner's behalf.
        let owner: Option<String> = tx
            .query_row(
                "SELECT owner_id FROM anon_groups WHERE id = ?1",
                [member.group_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if owner.as_deref() != Some(actor.to_string().as_str())
            || role != Some(AnonGroupRole::Owner.to_i32())
        {
            return Ok(false);
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2 AND is_active = 1)",
            params![member.group_id.to_string(), member.user_id.to_string()],
            |r| r.get(0),
        )?;
        if exists {
            return Ok(false);
        }
        let member_clock: i64 = tx.query_row("SELECT COALESCE(MAX(MAX(admitted_at,removed_at,changed_at)),0) FROM anon_group_member_order WHERE group_id = ?1 AND user_id = ?2",
            params![member.group_id.to_string(), member.user_id.to_string()], |r| r.get(0))?;
        let group_clock: i64 = tx
            .query_row(
                "SELECT MAX(roster_at,update_at) FROM anon_group_order WHERE group_id = ?1",
                [member.group_id.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let admission = now.max(
            member_clock
                .max(group_clock)
                .checked_add(1)
                .ok_or_else(|| {
                    StorageError::NotFound("Anonymous membership clock exhausted".into())
                })?,
        );
        write_ordered_member(&tx, member)?;
        tx.execute("INSERT INTO anon_group_member_order(group_id,user_id,admitted_at,changed_at) VALUES (?1,?2,?3,?3)
            ON CONFLICT(group_id,user_id) DO UPDATE SET admitted_at = excluded.admitted_at,changed_at = excluded.changed_at",
            params![member.group_id.to_string(), member.user_id.to_string(), admission])?;
        let pending = AnonGroupPending::new_invite(member.group_id, member.user_id);
        tx.execute("INSERT OR REPLACE INTO anon_group_pending(id,group_id,user_id,action,created_at,expires_at,retry_count) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![pending.id, member.group_id.to_string(),member.user_id.to_string(),pending.action.as_str(),pending.created_at.to_rfc3339(),pending.expires_at.to_rfc3339(),pending.retry_count])?;
        tx.commit()?;
        Ok(true)
    }

    // ========================================================================
    // Group CRUD
    // ========================================================================

    /// Insert a new anonymous group.
    pub fn insert_group(&self, group: &AnonGroup) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT OR REPLACE INTO anon_groups (id, name, owner_id, my_role, bootstrap_complete, created_at, updated_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                group.id.to_string(),
                group.name,
                group.owner_id.to_string(),
                group.my_role.to_i32(),
                group.bootstrap_complete as i32,
                group.created_at.to_rfc3339(),
                group.updated_at.to_rfc3339(),
                group.status.to_i32(),
            ],
        )?;
        Ok(())
    }

    /// Get an anonymous group by ID.
    pub fn get_group(&self, id: &AnonGroupId) -> Result<Option<AnonGroup>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, my_role, bootstrap_complete, created_at, updated_at, status, owner_id
             FROM anon_groups WHERE id = ?1",
        )?;

        let result = stmt.query_row(params![id.to_string()], row_to_group);

        match result {
            Ok(group) => Ok(Some(group)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Update only the group name.
    pub fn update_group_name(&self, id: &AnonGroupId, name: &str) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = chrono::Utc::now();
        let rows = conn.execute(
            "UPDATE anon_groups SET name = ?1, updated_at = ?2 WHERE id = ?3",
            params![name, now.to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonGroup {}", id)));
        }
        Ok(())
    }

    /// Record a new owner, and our own role after the hand-off.
    pub fn set_owner(
        &self,
        id: &AnonGroupId,
        owner: &UserId,
        my_role: crate::domain::anon_group::AnonGroupRole,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_groups SET owner_id = ?1, my_role = ?2, updated_at = ?3 WHERE id = ?4",
            params![
                owner.to_string(),
                my_role.to_i32(),
                chrono::Utc::now().to_rfc3339(),
                id.to_string()
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonGroup {}", id)));
        }
        Ok(())
    }

    /// Delete an anonymous group.
    pub fn delete_group(&self, id: &AnonGroupId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "DELETE FROM anon_groups WHERE id = ?1",
            params![id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonGroup {}", id)));
        }
        Ok(())
    }

    /// List all anonymous groups.
    pub fn list_groups(&self) -> Result<Vec<AnonGroup>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, my_role, bootstrap_complete, created_at, updated_at, status, owner_id
             FROM anon_groups ORDER BY updated_at DESC",
        )?;

        let mut rows = stmt.query([])?;
        super::repo_utils::collect_decoded_rows(&mut rows, "anonymous_group", |row| {
            row_to_group(row).map_err(StorageError::Database)
        })
    }

    // ========================================================================
    // Member CRUD
    // ========================================================================

    /// Add a member to a group. Uses upsert so accept_invite is idempotent on retry.
    pub fn add_member(&self, member: &AnonGroupMember) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT INTO anon_group_members (id, group_id, user_id, role, mailbox_pk, joined_at, profile_key, display_name, avatar, device_id, is_active)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)
             ON CONFLICT(group_id, user_id) DO UPDATE SET
                is_active = 1,
                role = excluded.role,
                mailbox_pk = COALESCE(excluded.mailbox_pk, mailbox_pk),
                device_id = COALESCE(excluded.device_id, device_id),
                profile_key = COALESCE(excluded.profile_key, profile_key),
                display_name = COALESCE(excluded.display_name, display_name),
                avatar = COALESCE(excluded.avatar, avatar)",
            params![
                member.id,
                member.group_id.to_string(),
                member.user_id.to_string(),
                member.role.to_i32(),
                member.mailbox_pk.as_ref().map(|pk| pk.as_slice()),
                member.joined_at.to_rfc3339(),
                member.profile_key.as_ref().map(|pk| pk.as_slice()),
                member.display_name,
                member.avatar,
                member.device_id,
            ],
        )?;
        Ok(())
    }

    /// Remove a member from the live roster while keeping their profile for history.
    pub fn remove_member(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_group_members SET is_active = 0, mailbox_pk = NULL, device_id = NULL
             WHERE group_id = ?1 AND user_id = ?2 AND is_active = 1",
            params![group_id.to_string(), user_id.to_string(),],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Member {} in group {}",
                user_id, group_id
            )));
        }
        Ok(())
    }

    /// Update a member's mailbox public key and profile (when JoinAck is received).
    ///
    /// Uses COALESCE to preserve existing profile data when new values are NULL.
    /// This prevents member list updates from clearing display names that were
    /// correctly stored during invite acceptance.
    pub fn update_member_mailbox_and_profile(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        mailbox_pk: &[u8; 32],
        profile_key: Option<[u8; 32]>,
        display_name: Option<String>,
        avatar: Option<Vec<u8>>,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_group_members
             SET mailbox_pk = ?1,
                 profile_key = COALESCE(?4, profile_key),
                 display_name = COALESCE(?5, display_name),
                 avatar = COALESCE(?6, avatar)
             WHERE group_id = ?2 AND user_id = ?3 AND is_active = 1",
            params![
                mailbox_pk.as_slice(),
                group_id.to_string(),
                user_id.to_string(),
                profile_key.as_ref().map(|pk| pk.as_slice()),
                display_name,
                avatar,
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Member {} in group {}",
                user_id, group_id
            )));
        }
        Ok(())
    }

    /// Update a member's profile (display name, avatar) without touching mailbox key.
    ///
    /// Uses COALESCE to preserve existing profile data when new values are NULL.
    pub fn update_member_profile(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        profile_key: Option<[u8; 32]>,
        display_name: Option<String>,
        avatar: Option<Vec<u8>>,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_group_members
             SET profile_key = COALESCE(?1, profile_key),
                 display_name = COALESCE(?2, display_name),
                 avatar = COALESCE(?3, avatar)
             WHERE group_id = ?4 AND user_id = ?5 AND is_active = 1",
            params![
                profile_key.as_ref().map(|pk| pk.as_slice()),
                display_name,
                avatar,
                group_id.to_string(),
                user_id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Member {} in group {}",
                user_id, group_id
            )));
        }
        Ok(())
    }

    /// Update a member's mailbox public key without touching the profile.
    pub fn update_member_mailbox(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        mailbox_pk: &[u8; 32],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_group_members SET mailbox_pk = ?1 WHERE group_id = ?2 AND user_id = ?3 AND is_active = 1",
            params![
                mailbox_pk.as_slice(),
                group_id.to_string(),
                user_id.to_string(),
            ],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Member {} in group {}",
                user_id, group_id
            )));
        }
        Ok(())
    }

    /// Update a member's role.
    pub fn update_member_role(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        role: AnonGroupRole,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let rows = conn.execute(
            "UPDATE anon_group_members SET role = ?1 WHERE group_id = ?2 AND user_id = ?3 AND is_active = 1",
            params![role.to_i32(), group_id.to_string(), user_id.to_string(),],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!(
                "Member {} in group {}",
                user_id, group_id
            )));
        }
        Ok(())
    }

    /// Get all members of a group.
    pub fn get_members(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Vec<AnonGroupMember>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, user_id, role, mailbox_pk, joined_at, profile_key, display_name, avatar, device_id
             FROM anon_group_members WHERE group_id = ?1 AND is_active = 1 ORDER BY joined_at",
        )?;

        let mut rows = stmt.query(params![group_id.to_string()])?;
        let mut members = Vec::new();

        while let Some(row) = rows.next()? {
            members.push(row_to_member(row)?);
        }

        Ok(members)
    }

    /// Departed profiles remain stored for historical names. Snapshot their
    /// inactive state before applying a roster so re-admission is observable.
    pub fn inactive_member_ids(&self, group_id: &AnonGroupId) -> Result<Vec<UserId>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn
            .prepare("SELECT user_id FROM anon_group_members WHERE group_id=?1 AND is_active=0")?;
        let rows = stmt.query_map([group_id.to_string()], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            row?.parse()
                .map_err(|_| StorageError::InvalidData("Invalid group member ID".into()))
        })
        .collect()
    }

    /// Get members that have confirmed their mailbox key (for sending messages).
    pub fn get_members_with_mailbox(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Vec<AnonGroupMember>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, user_id, role, mailbox_pk, joined_at, profile_key, display_name, avatar, device_id
             FROM anon_group_members WHERE group_id = ?1 AND is_active = 1 AND mailbox_pk IS NOT NULL
             ORDER BY joined_at",
        )?;

        let mut rows = stmt.query(params![group_id.to_string()])?;
        let mut members = Vec::new();

        while let Some(row) = rows.next()? {
            members.push(row_to_member(row)?);
        }

        Ok(members)
    }

    /// Get members that do NOT yet have a confirmed mailbox key.
    ///
    /// These are invitees whose JoinAck has not yet been received. Messages
    /// destined for them must be queued via `enqueue_pending_message` and
    /// drained once the JoinAck arrives.
    pub fn get_members_without_mailbox(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Vec<AnonGroupMember>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, user_id, role, mailbox_pk, joined_at, profile_key, display_name, avatar, device_id
             FROM anon_group_members WHERE group_id = ?1 AND is_active = 1 AND mailbox_pk IS NULL
             ORDER BY joined_at",
        )?;

        let mut rows = stmt.query(params![group_id.to_string()])?;
        let mut members = Vec::new();

        while let Some(row) = rows.next()? {
            members.push(row_to_member(row)?);
        }

        Ok(members)
    }

    /// Get a member of a group.
    pub fn get_member(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<AnonGroupMember>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, user_id, role, mailbox_pk, joined_at, profile_key, display_name, avatar, device_id
             FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2 AND is_active = 1",
        )?;

        let result = stmt.query_row(
            params![group_id.to_string(), user_id.to_string(),],
            row_to_member,
        );

        match result {
            Ok(member) => Ok(Some(member)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Include departed members when resolving message history. Never use this
    /// accessor for membership checks or delivery.
    pub fn get_member_avatar(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let conn = self.db.read_connection()?;
        let avatar: Option<Option<Vec<u8>>> = conn
            .query_row(
                "SELECT avatar FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2",
                params![group_id.to_string(), user_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(avatar.flatten())
    }

    /// Check if a user is a member of a group.
    pub fn is_member(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i32 = conn.query_row(
            "SELECT COUNT(*) FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2 AND is_active = 1",
            params![group_id.to_string(), user_id.to_string(),],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get a member's role in a group.
    pub fn get_member_role(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<AnonGroupRole>, StorageError> {
        let conn = self.db.read_connection()?;
        let result: Result<i32, _> = conn.query_row(
            "SELECT role FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2 AND is_active = 1",
            params![group_id.to_string(), user_id.to_string(),],
            |row| row.get(0),
        );

        match result {
            Ok(role) => Ok(Some(AnonGroupRole::from_i32(role))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Get a member's display name from their profile info in the group.
    /// Return any non-empty display name recorded for `user_id` across any group.
    ///
    /// Used by `UserService::get_display_name` to avoid raw-UUID fallback for
    /// anon group members who aren't in the main user directory.
    pub fn find_display_name_for_user(
        &self,
        user_id: &UserId,
    ) -> Result<Option<String>, StorageError> {
        let conn = self.db.read_connection()?;
        let result: Result<Option<String>, _> = conn.query_row(
            "SELECT display_name FROM anon_group_members \
             WHERE user_id = ?1 AND is_active = 1 AND display_name IS NOT NULL AND display_name != '' LIMIT 1",
            params![user_id.to_string()],
            |row| row.get(0),
        );
        match result {
            Ok(name) => Ok(name),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    pub fn get_member_display_name(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<Option<String>, StorageError> {
        let conn = self.db.read_connection()?;
        let current: Option<Option<String>> = conn
            .query_row(
                "SELECT display_name FROM anon_group_members WHERE group_id = ?1 AND user_id = ?2",
                params![group_id.to_string(), user_id.to_string(),],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(name) = current.flatten().filter(|name| !name.is_empty()) {
            return Ok(Some(name));
        }

        // Groups that lost a member before the profile was retained can still
        // name old messages from the authenticated leave notice's snapshot.
        let departed: Option<String> = conn
            .query_row(
                "SELECT json_extract(content_json, '$.System.AnonGroupUserLeft.left_name')
                 FROM messages WHERE conversation_type = 'anon_group'
                 AND conversation_id = ?1 AND sender_id = ?2
                 AND json_type(content_json, '$.System.AnonGroupUserLeft.left_name') = 'text'
                 ORDER BY sent_at DESC LIMIT 1",
                params![group_id.to_string(), user_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(departed.filter(|name| !name.is_empty()))
    }

    // ========================================================================
    // Mailbox CRUD
    // ========================================================================

    /// Insert a group mailbox.
    pub fn insert_mailbox(&self, mailbox: &AnonGroupMailbox) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT OR REPLACE INTO anon_group_mailboxes (id, group_id, mailbox_public_key, mailbox_private_key, token_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                mailbox.id,
                mailbox.group_id.to_string(),
                mailbox.public_key.as_slice(),
                mailbox.private_key.as_slice(),
                mailbox.token_id,
                mailbox.created_at.to_rfc3339(),
                mailbox.expires_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Get the mailbox for a group.
    pub fn get_mailbox(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Option<AnonGroupMailbox>, StorageError> {
        let conn = self.db.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, mailbox_public_key, mailbox_private_key, token_id, created_at, expires_at
             FROM anon_group_mailboxes WHERE group_id = ?1",
        )?;

        let result = stmt.query_row(params![group_id.to_string()], row_to_mailbox);

        match result {
            Ok(mailbox) => Ok(Some(mailbox)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }

    /// Delete a group mailbox.
    pub fn delete_mailbox(&self, group_id: &AnonGroupId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM anon_group_mailboxes WHERE group_id = ?1",
            params![group_id.to_string()],
        )?;
        Ok(())
    }

    // ========================================================================
    // Pending Actions CRUD
    // ========================================================================

    /// Retry only an existing, live pending admission; never allocate an incarnation.
    pub fn can_retry_invitation(
        &self,
        group: &AnonGroupId,
        user: &UserId,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM anon_group_members m
             JOIN anon_group_pending p ON p.group_id=m.group_id AND p.user_id=m.user_id AND p.action='invite'
             LEFT JOIN anon_group_member_order o ON o.group_id=m.group_id AND o.user_id=m.user_id
             WHERE m.group_id=?1 AND m.user_id=?2 AND m.mailbox_pk IS NULL
             AND m.is_active = 1 AND p.expires_at > ?3 AND COALESCE(o.admitted_at,0) >= COALESCE(o.removed_at,0)
             AND (COALESCE(o.removed_at,0)=0 OR o.admitted_at > o.removed_at))",
            params![group.to_string(), user.to_string(), Utc::now().to_rfc3339()],
            |r| r.get(0),
        )?)
    }

    /// Persist the bootstrap retry budget before network I/O. This self-targeted
    /// pending row is bookkeeping only: it cannot admit any member.
    pub fn claim_bootstrap_reannouncement(
        &self,
        group: &AnonGroupId,
        user: &UserId,
    ) -> Result<bool, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let pending = AnonGroupPending::new_invite(*group, *user);
        tx.execute(
            "INSERT OR IGNORE INTO anon_group_pending(id,group_id,user_id,action,created_at,expires_at,retry_count)
             SELECT ?1,?2,?3,'invite',?4,?5,0 WHERE EXISTS
             (SELECT 1 FROM anon_group_members WHERE group_id=?2 AND user_id=?3 AND is_active = 1)",
            params![pending.id, group.to_string(), user.to_string(), pending.created_at.to_rfc3339(), pending.expires_at.to_rfc3339()],
        )?;
        // At most three attempts across restarts, spaced by at least five minutes
        // from creation. Exhaustion/expiry retains all user messages and drafts.
        let claimed = tx.execute(
            "UPDATE anon_group_pending SET retry_count=retry_count+1
             WHERE group_id=?1 AND user_id=?2 AND action='invite' AND retry_count < 3
             AND expires_at > ?3
             AND julianday(?3) >= julianday(created_at) + retry_count * (5.0 / 1440.0)",
            params![group.to_string(), user.to_string(), Utc::now().to_rfc3339()],
        )? > 0;
        tx.commit()?;
        Ok(claimed)
    }

    /// Insert a pending action.
    pub fn insert_pending(&self, pending: &AnonGroupPending) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "INSERT OR REPLACE INTO anon_group_pending (id, group_id, user_id, action, created_at, expires_at, retry_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                pending.id,
                pending.group_id.to_string(),
                pending.user_id.to_string(),
                pending.action.as_str(),
                pending.created_at.to_rfc3339(),
                pending.expires_at.to_rfc3339(),
                pending.retry_count,
            ],
        )?;
        Ok(())
    }

    /// Delete a pending action.
    pub fn delete_pending(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        action: PendingActionType,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM anon_group_pending WHERE group_id = ?1 AND user_id = ?2 AND action = ?3",
            params![group_id.to_string(), user_id.to_string(), action.as_str(),],
        )?;
        Ok(())
    }

    // Message CRUD methods have been removed.
    // All message operations now use the unified MessageRepository.

    // ========================================================================
    // Group Status / Blocking
    // ========================================================================

    /// Update group status (active/muted/blocked).
    pub fn update_status(
        &self,
        id: &AnonGroupId,
        status: crate::domain::anon_group::AnonGroupStatus,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now();
        let rows = conn.execute(
            "UPDATE anon_groups SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status.to_i32(), now.to_rfc3339(), id.to_string()],
        )?;
        if rows == 0 {
            return Err(StorageError::NotFound(format!("AnonGroup {}", id)));
        }
        Ok(())
    }

    /// List only active (non-blocked) groups.
    pub fn list_active_groups(&self) -> Result<Vec<AnonGroup>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, my_role, bootstrap_complete, created_at, updated_at, status, owner_id
             FROM anon_groups WHERE status NOT IN (2, 3) ORDER BY updated_at DESC",
        )?;

        let mut rows = stmt.query([])?;
        super::repo_utils::collect_decoded_rows(&mut rows, "anonymous_group", |row| {
            row_to_group(row).map_err(StorageError::Database)
        })
    }

    // ========================================================================
    // Per-User Blocking within Groups
    // ========================================================================

    /// Block a user within a group.
    pub fn block_user(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
        reason: Option<&str>,
    ) -> Result<(), StorageError> {
        use crate::domain::anon_group::AnonGroupBlockedUser;

        let blocked = AnonGroupBlockedUser::new(*group_id, *user_id, reason.map(String::from));

        let conn = self.db.connection()?;
        conn.execute(
            "INSERT OR REPLACE INTO anon_group_blocked_users (id, group_id, user_id, blocked_at, reason)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                blocked.id,
                group_id.to_string(),
                user_id.to_string(),
                blocked.blocked_at.to_rfc3339(),
                blocked.reason,
            ],
        )?;
        Ok(())
    }

    /// Unblock a user within a group.
    pub fn unblock_user(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM anon_group_blocked_users WHERE group_id = ?1 AND user_id = ?2",
            params![group_id.to_string(), user_id.to_string(),],
        )?;
        Ok(())
    }

    /// Check if a user is blocked within a group.
    pub fn is_user_blocked(
        &self,
        group_id: &AnonGroupId,
        user_id: &UserId,
    ) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM anon_group_blocked_users WHERE group_id = ?1 AND user_id = ?2",
            params![group_id.to_string(), user_id.to_string(),],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get all blocked users in a group.
    pub fn get_blocked_users(&self, group_id: &AnonGroupId) -> Result<Vec<UserId>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt =
            conn.prepare("SELECT user_id FROM anon_group_blocked_users WHERE group_id = ?1")?;

        let mut rows = stmt.query(params![group_id.to_string()])?;
        let mut users = Vec::new();

        while let Some(row) = rows.next()? {
            let user_id_str: String = row.get(0)?;
            let user_id = user_id_str
                .parse()
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            users.push(user_id);
        }

        Ok(users)
    }

    // ========================================================================
    // Rate Limiting
    // ========================================================================

    /// Get rate limit entry for a group.
    pub fn get_rate_limit(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Option<crate::domain::AnonRateLimitEntry>, StorageError> {
        super::repo_utils::get_rate_limit(&self.db, "group", group_id.as_bytes().as_slice())
    }

    /// Increment rate limit counter, resetting window if expired.
    pub fn increment_rate_limit(
        &self,
        group_id: &AnonGroupId,
        window_duration: chrono::Duration,
    ) -> Result<u32, StorageError> {
        super::repo_utils::increment_rate_limit(
            &self.db,
            "group",
            group_id.as_bytes().as_slice(),
            window_duration,
        )
    }

    // ========================================================================
    // Pending Outbound Messages (awaiting recipient JoinAck)
    // ========================================================================

    /// Enqueue an outbound message for a member that does not yet have a
    /// confirmed mailbox key.
    ///
    /// `content_blob` is the encoded `Content` proto (same bytes `send_message`
    /// would otherwise pass to `send_to_anon_mailbox`). The blob is re-encrypted
    /// per-recipient when drained (Signal Protocol is pairwise).
    pub fn enqueue_pending_message(
        &self,
        group_id: &AnonGroupId,
        recipient_user_id: &UserId,
        content_blob: &[u8],
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let now = Utc::now().to_rfc3339();
        use prost::Message as _;
        let content = crate::proto::content_v1::Content::decode(content_blob)
            .map_err(|_| StorageError::InvalidData("Invalid pending anonymous content".into()))?;
        let timestamp = membership_timestamp(content.timestamp)?;
        let inserted = conn.execute(
            "INSERT INTO pending_anon_group_messages
               (group_id, recipient_user_id, encrypted_content_blob, created_at, attempts)
             SELECT ?1, ?2, ?3, ?4, 0 WHERE EXISTS (
                SELECT 1 FROM anon_group_members m JOIN anon_groups g ON g.id = m.group_id
                LEFT JOIN anon_group_member_order o ON o.group_id = m.group_id AND o.user_id = m.user_id
                WHERE m.group_id = ?1 AND m.user_id = ?2 AND m.is_active = 1
                AND (COALESCE(o.removed_at,0) = 0 OR o.admitted_at > o.removed_at)
                AND COALESCE(o.admitted_at,0) <= ?5)",
            params![
                group_id.to_string(),
                recipient_user_id.to_string(),
                content_blob,
                now,
                timestamp,
            ],
        )?;
        if inserted == 0 {
            return Err(StorageError::NotFound(
                "Anonymous recipient removed or reinvited".into(),
            ));
        }
        Ok(())
    }

    /// Atomically replace older owner rosters for each recipient with this
    /// snapshot. The existing pending-message drain then survives restarts.
    pub fn replace_pending_roster(
        &self,
        group_id: &AnonGroupId,
        recipients: &[UserId],
        content_blob: &[u8],
    ) -> Result<(), StorageError> {
        use prost::Message as _;
        let content = crate::proto::content_v1::Content::decode(content_blob)
            .map_err(|_| StorageError::InvalidData("Invalid roster content".into()))?;
        let roster_at = match content.content {
            Some(crate::proto::content_v1::content::Content::AnonGroupMemberList(roster)) => roster.timestamp,
            _ => return Err(StorageError::InvalidData("Pending content is not a roster".into())),
        };
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        for recipient in recipients {
            let mut old = Vec::new();
            let mut newer_queued = false;
            {
                let mut stmt = tx.prepare("SELECT id, encrypted_content_blob FROM pending_anon_group_messages WHERE group_id=?1 AND recipient_user_id=?2")?;
                let rows = stmt.query_map(
                    params![group_id.to_string(), recipient.to_string()],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )?;
                for row in rows {
                    let (id, blob) = row?;
                    if let Some(crate::proto::content_v1::content::Content::AnonGroupMemberList(roster)) =
                        crate::proto::content_v1::Content::decode(blob.as_slice()).ok().and_then(|old| old.content)
                    {
                        if roster.timestamp >= roster_at {
                            newer_queued = true;
                        } else {
                            old.push(id);
                        }
                    }
                }
            }
            for id in old {
                tx.execute("DELETE FROM pending_anon_group_messages WHERE id=?1", [id])?;
            }
            if newer_queued {
                continue;
            }
            tx.execute("INSERT INTO pending_anon_group_messages (group_id,recipient_user_id,encrypted_content_blob,created_at,attempts) VALUES (?1,?2,?3,?4,0)",
                params![group_id.to_string(), recipient.to_string(), content_blob, Utc::now().to_rfc3339()])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Get all pending messages queued for a specific (group, recipient).
    ///
    /// Returns (row_id, content_blob, attempts) ordered by insertion order so
    /// the drain preserves send order.
    pub fn get_pending_messages_for_member(
        &self,
        group_id: &AnonGroupId,
        recipient_user_id: &UserId,
    ) -> Result<Vec<(i64, Vec<u8>, u32)>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, encrypted_content_blob, attempts FROM pending_anon_group_messages
             WHERE group_id = ?1 AND recipient_user_id = ?2
             ORDER BY id ASC",
        )?;

        let mut rows = stmt.query(params![group_id.to_string(), recipient_user_id.to_string(),])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            let attempts: u32 = row.get(2)?;
            out.push((id, blob, attempts));
        }
        Ok(out)
    }

    /// Delete a single pending message row after successful delivery.
    pub fn delete_pending_message(&self, row_id: i64) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM pending_anon_group_messages WHERE id = ?1",
            params![row_id],
        )?;
        Ok(())
    }

    /// Increment the attempts counter for a pending message row; the drain
    /// gives a row up after `MAX_PARKED_ATTEMPTS`.
    pub fn increment_pending_message_attempts(&self, row_id: i64) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "UPDATE pending_anon_group_messages SET attempts = attempts + 1 WHERE id = ?1",
            params![row_id],
        )?;
        Ok(())
    }
}

// ============================================================================
// Helper functions
// ============================================================================

use super::repo_utils::{parse_datetime, require_array_32, require_array_64, vec_to_array_32};

fn row_to_group(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroup> {
    use crate::domain::anon_group::AnonGroupStatus;

    let id_str: String = row.get(0)?;
    let id = id_str.parse().map_err(|_| rusqlite::Error::InvalidQuery)?;

    // Status column may not exist in older schemas, default to Active
    let status: i32 = row.get(6).unwrap_or(0);

    let owner_id_str: String = row.get(7)?;
    let owner_id = owner_id_str
        .parse()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    Ok(AnonGroup {
        id,
        name: row.get(1)?,
        owner_id,
        my_role: AnonGroupRole::from_i32(row.get(2)?),
        status: AnonGroupStatus::from_i32(status),
        bootstrap_complete: row.get::<_, i32>(3)? != 0,
        created_at: parse_datetime(&row.get::<_, String>(4)?),
        updated_at: parse_datetime(&row.get::<_, String>(5)?),
    })
}

fn row_to_member(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroupMember> {
    let group_id_str: String = row.get(1)?;
    let group_id = group_id_str
        .parse()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    let user_id_str: String = row.get(2)?;
    let user_id = user_id_str
        .parse()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    let mailbox_pk = vec_to_array_32(row.get(4)?);
    let profile_key = vec_to_array_32(row.get(6)?);

    Ok(AnonGroupMember {
        id: row.get(0)?,
        group_id,
        user_id,
        role: AnonGroupRole::from_i32(row.get(3)?),
        mailbox_pk,
        profile_key,
        display_name: row.get(7)?,
        avatar: row.get(8)?,
        joined_at: parse_datetime(&row.get::<_, String>(5)?),
        device_id: row.get(9)?,
    })
}

fn row_to_mailbox(row: &rusqlite::Row<'_>) -> rusqlite::Result<AnonGroupMailbox> {
    let group_id_str: String = row.get(1)?;
    let group_id = group_id_str
        .parse()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    let pk_bytes: Vec<u8> = row.get(2)?;
    let sk_bytes: Vec<u8> = row.get(3)?;

    let public_key = require_array_32(&pk_bytes)
        .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid public key".into()))?;
    let private_key = require_array_64(&sk_bytes)
        .map_err(|_| rusqlite::Error::InvalidParameterName("Invalid private key".into()))?;

    Ok(AnonGroupMailbox {
        id: row.get(0)?,
        group_id,
        public_key,
        private_key,
        token_id: row.get(4)?,
        created_at: parse_datetime(&row.get::<_, String>(5)?),
        expires_at: parse_datetime(&row.get::<_, String>(6)?),
    })
}

// SQLite timestamps are deliberately bounded before any structural mutation.
fn membership_timestamp(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value)
        .map_err(|_| StorageError::NotFound("Invalid anonymous membership timestamp".into()))
}

fn write_ordered_member(
    conn: &rusqlite::Connection,
    member: &AnonGroupMember,
) -> Result<(), StorageError> {
    conn.execute("INSERT INTO anon_group_members(id,group_id,user_id,role,mailbox_pk,joined_at,profile_key,display_name,avatar,device_id,is_active)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,1) ON CONFLICT(group_id,user_id) DO UPDATE SET
        role = excluded.role, mailbox_pk = COALESCE(excluded.mailbox_pk,mailbox_pk), device_id = COALESCE(excluded.device_id,device_id),
        profile_key = COALESCE(excluded.profile_key,profile_key), display_name = COALESCE(excluded.display_name,display_name), avatar = COALESCE(excluded.avatar,avatar), is_active = 1",
        params![member.id,member.group_id.to_string(),member.user_id.to_string(),member.role.to_i32(),
            member.mailbox_pk.as_ref().map(|v|v.as_slice()),member.joined_at.to_rfc3339(),
            member.profile_key.as_ref().map(|v|v.as_slice()),member.display_name,member.avatar,member.device_id])?;
    Ok(())
}

fn record_member_change(
    conn: &rusqlite::Connection,
    group: &AnonGroupId,
    user: &UserId,
    timestamp: i64,
) -> Result<(), StorageError> {
    conn.execute("INSERT INTO anon_group_member_order(group_id,user_id,changed_at) VALUES (?1,?2,?3) ON CONFLICT(group_id,user_id) DO UPDATE SET changed_at = MAX(changed_at,excluded.changed_at)",params![group.to_string(),user.to_string(),timestamp])?;
    Ok(())
}
