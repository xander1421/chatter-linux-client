use super::*;

impl AnonGroupRepository {
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

    /// Recheck a recipient selected for a post before encryption or retry.
    /// A pending invite can gain its mailbox after the post was composed;
    /// its later JoinAck must not invalidate the original recipient snapshot.
    pub fn current_post_recipient(
        &self,
        group: &AnonGroupId,
        user: &UserId,
        key: &[u8; 32],
    ) -> Result<bool, StorageError> {
        let conn = self.db.read_connection()?;
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM anon_group_members m
            LEFT JOIN anon_group_member_order o ON o.group_id=m.group_id AND o.user_id=m.user_id
            WHERE m.group_id=?1 AND m.user_id=?2 AND m.is_active=1 AND m.mailbox_pk=?3
            AND (COALESCE(o.removed_at,0)=0 OR o.admitted_at>o.removed_at))",
            params![group.to_string(), user.to_string(), key.as_slice()],
            |r| r.get(0),
        )?)
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

    /// The owner's admission for our current join, in the same clock domain
    /// as the other admissions in its roster. Local wall time is not usable
    /// as a cutoff for a post held before that roster arrived.
    pub fn local_join_owner_admission(
        &self,
        group: &AnonGroupId,
    ) -> Result<Option<u64>, StorageError> {
        let conn = self.db.read_connection()?;
        let admission: Option<i64> = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM settings WHERE key=?1",
                [format!("anon.group.local_owner_admission.v1:{group}")],
                |row| row.get(0),
            )
            .optional()?;
        Ok(admission
            .filter(|value| *value > 0)
            .map(|value| value as u64))
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
        let mut local_owner_admission = None;
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
            // recorded acceptance. A fresh owner roster listing our current
            // mailbox confirms the local generation. Defer the write until
            // after the roster's own admitted_at update below.
            if member.user_id == *me
                && old_admission > 0
                && (removed == 0 || old_admission > removed)
            {
                local_owner_admission = Some(admission);
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
        let local_admission_acknowledged = if let Some(owner_admission) = local_owner_admission {
            tx.execute(
                "UPDATE anon_group_member_order SET acknowledged_at=admitted_at
                 WHERE group_id=?1 AND user_id=?2 AND admitted_at>0",
                params![group.to_string(), me.to_string()],
            )? > 0
                && {
                    tx.execute(
                    "INSERT INTO settings(key,value,updated_at) VALUES (?1,?2,?3)
                     ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",
                    params![format!("anon.group.local_owner_admission.v1:{group}"), owner_admission.to_string(), Utc::now().to_rfc3339()],
                )?;
                    true
                }
        } else {
            false
        };
        tx.commit()?;
        tracing::info!(group_id = %group, local_admission_acknowledged, "owner group roster applied");
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
}
