use super::*;

/// A historical failed acceptance left only its initial group row. Include
/// inactive membership and ordering tombstones so repair never readmits a user.
fn uninitialized(
    conn: &rusqlite::Connection,
    group: &AnonGroup,
    me: &UserId,
) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM anon_groups g WHERE id=?1 AND owner_id=?2
         AND owner_id!=?3 AND my_role=?4 AND bootstrap_complete=0 AND status=0
         AND NOT EXISTS(SELECT 1 FROM anon_group_members WHERE group_id=g.id)
         AND NOT EXISTS(SELECT 1 FROM anon_group_mailboxes WHERE group_id=g.id)
         AND NOT EXISTS(SELECT 1 FROM anon_group_member_order WHERE group_id=g.id)
         AND NOT EXISTS(SELECT 1 FROM anon_group_order WHERE group_id=g.id)
         AND NOT EXISTS(SELECT 1 FROM anon_group_blocked_users WHERE group_id=g.id))",
        params![
            group.id.to_string(),
            group.owner_id.to_string(),
            me.to_string(),
            AnonGroupRole::Member.to_i32()
        ],
        |row| row.get(0),
    )?)
}

impl AnonGroupRepository {
    pub(crate) fn is_uninitialized_invited_group(
        &self,
        group: &AnonGroup,
        me: &UserId,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        uninitialized(&conn, group, me)
    }

    pub(crate) fn require_current_invite(
        &self,
        invite: &PendingAnonGroupInvite,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        super::invites::require_pending_invite(&conn, invite)
    }

    /// Publish all local acceptance state together, after remote registration.
    /// Plain INSERTs and a writer-transaction recheck protect concurrent state;
    /// never REPLACE a group (which cascades away its roster and mailbox).
    pub(crate) fn initialize_invited_group(
        &self,
        group: &AnonGroup,
        mailbox: &AnonGroupMailbox,
        own: &AnonGroupMember,
        inviter: &AnonGroupMember,
        now: u64,
        pending: Option<&PendingAnonGroupInvite>,
    ) -> Result<(), StorageError> {
        if own.group_id != group.id
            || inviter.group_id != group.id
            || mailbox.group_id != group.id
            || own.mailbox_pk != Some(mailbox.public_key)
            || own.user_id == inviter.user_id
        {
            return Err(StorageError::InvalidData(
                "Invalid invitation acceptance state".into(),
            ));
        }
        if let Some(invite) = pending {
            if invite.group_id != group.id
                || invite.inviter_id != inviter.user_id
                || Some(invite.inviter_device_id) != inviter.device_id
                || Some(invite.inviter_mailbox_pk) != inviter.mailbox_pk
            {
                return Err(StorageError::InvalidData(
                    "Invalid pending invitation binding".into(),
                ));
            }
        }
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        if let Some(invite) = pending {
            super::invites::require_pending_invite(&tx, invite)?;
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM anon_groups WHERE id=?1)",
            [group.id.to_string()],
            |row| row.get(0),
        )?;
        if exists {
            if inviter.user_id != group.owner_id || !uninitialized(&tx, group, &own.user_id)? {
                return Err(StorageError::InvalidData(
                    "Group acceptance state changed".into(),
                ));
            }
        } else {
            tx.execute(
                "INSERT INTO anon_groups(id,name,owner_id,my_role,bootstrap_complete,created_at,updated_at,status)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![group.id.to_string(),group.name,group.owner_id.to_string(),group.my_role.to_i32(),
                    group.bootstrap_complete as i32,group.created_at.to_rfc3339(),group.updated_at.to_rfc3339(),group.status.to_i32()],
            )?;
        }
        tx.execute(
            "INSERT INTO anon_group_mailboxes(id,group_id,mailbox_public_key,mailbox_private_key,token_id,created_at,expires_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![mailbox.id,group.id.to_string(),mailbox.public_key.as_slice(),mailbox.private_key.as_slice(),
                mailbox.token_id,mailbox.created_at.to_rfc3339(),mailbox.expires_at.to_rfc3339()],
        )?;
        super::ordering::accept_self_in_transaction(&tx, own, now)?;
        write_ordered_member(&tx, inviter)?;
        tx.commit()?;
        Ok(())
    }
}
