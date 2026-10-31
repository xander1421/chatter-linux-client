use super::*;

impl AnonGroupRepository {
    pub fn save_pending_invite(&self, invite: &PendingAnonGroupInvite) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM pending_anon_group_invites WHERE received_at < datetime('now', '-30 days')", [])?;
        tx.execute("INSERT INTO pending_anon_group_invites
            (group_id, group_name, inviter_id, inviter_device_id, inviter_mailbox_pk,
             profile_key, inviter_display_name, inviter_avatar, invite_epoch)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(group_id) DO UPDATE SET
                group_name=excluded.group_name, inviter_device_id=excluded.inviter_device_id,
                inviter_mailbox_pk=excluded.inviter_mailbox_pk, profile_key=excluded.profile_key,
                invite_epoch=excluded.invite_epoch,
                inviter_display_name=excluded.inviter_display_name,
                inviter_avatar=COALESCE(excluded.inviter_avatar, pending_anon_group_invites.inviter_avatar),
                received_at=CURRENT_TIMESTAMP
            WHERE inviter_id=excluded.inviter_id
              AND excluded.invite_epoch >= pending_anon_group_invites.invite_epoch",
            params![invite.group_id.to_string(), invite.group_name, invite.inviter_id.to_string(),
                invite.inviter_device_id, invite.inviter_mailbox_pk.as_slice(),
                invite.profile_key.as_ref().map(|v| v.as_slice()), invite.inviter_display_name,
                invite.inviter_avatar, invite.invite_epoch])?;
        tx.execute(
            "DELETE FROM pending_anon_group_invites WHERE group_id IN (
            SELECT group_id FROM pending_anon_group_invites
            ORDER BY received_at DESC, group_id DESC LIMIT -1 OFFSET 256
        )",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_pending_invites(&self) -> Result<Vec<PendingAnonGroupInvite>, StorageError> {
        let conn = self.db.connection()?;
        conn.execute("DELETE FROM pending_anon_group_invites WHERE received_at < datetime('now', '-30 days')", [])?;
        let mut stmt = conn.prepare(
            "SELECT group_id, group_name, inviter_id, inviter_device_id,
            inviter_mailbox_pk, profile_key, inviter_display_name, inviter_avatar, invite_epoch
            FROM pending_anon_group_invites ORDER BY received_at ASC, group_id ASC LIMIT 256",
        )?;
        let rows = stmt.query_map([], invite_columns)?;
        rows.map(|row| decode_invite(row?)).collect()
    }

    pub fn get_pending_invite(
        &self,
        group_id: &AnonGroupId,
    ) -> Result<Option<PendingAnonGroupInvite>, StorageError> {
        let conn = self.db.connection()?;
        conn.execute("DELETE FROM pending_anon_group_invites WHERE received_at < datetime('now', '-30 days')", [])?;
        let mut stmt = conn.prepare(
            "SELECT group_id, group_name, inviter_id, inviter_device_id,
            inviter_mailbox_pk, profile_key, inviter_display_name, inviter_avatar, invite_epoch
            FROM pending_anon_group_invites WHERE group_id = ?1",
        )?;
        let mut rows = stmt.query_map([group_id.to_string()], invite_columns)?;
        let invite = rows.next().transpose()?.map(decode_invite).transpose()?;
        Ok(invite)
    }

    pub fn remove_pending_invite(&self, group_id: &AnonGroupId) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        conn.execute(
            "DELETE FROM pending_anon_group_invites WHERE group_id=?1",
            [group_id.to_string()],
        )?;
        Ok(())
    }

    /// Linearize decline against the acceptance transaction before delivery.
    pub(crate) fn decline_exact_pending_invite(
        &self,
        invite: &PendingAnonGroupInvite,
    ) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        require_pending_invite(&tx, invite)?;
        let accepted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM anon_group_mailboxes WHERE group_id=?1)",
            [invite.group_id.to_string()],
            |row| row.get(0),
        )?;
        if accepted {
            return Err(StorageError::InvalidData(
                "Invitation already accepted locally".into(),
            ));
        }
        tx.execute(
            "DELETE FROM pending_anon_group_invites WHERE group_id=?1",
            [invite.group_id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Consume only the authenticated invitation incarnation the decision used.
    pub(crate) fn remove_exact_pending_invite(
        &self,
        invite: &PendingAnonGroupInvite,
    ) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        Ok(conn.execute(
            "DELETE FROM pending_anon_group_invites WHERE group_id=?1 AND invite_epoch=?2
             AND inviter_id=?3 AND inviter_device_id=?4 AND inviter_mailbox_pk=?5",
            params![
                invite.group_id.to_string(),
                invite.invite_epoch,
                invite.inviter_id.to_string(),
                invite.inviter_device_id,
                invite.inviter_mailbox_pk.as_slice()
            ],
        )? == 1)
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
}

type InviteColumns = (
    String,
    String,
    String,
    u32,
    Vec<u8>,
    Option<Vec<u8>>,
    Option<String>,
    Option<Vec<u8>>,
    u64,
);

fn invite_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<InviteColumns> {
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
        row.get::<_, u64>(8)?,
    ))
}

fn decode_invite(row: InviteColumns) -> Result<PendingAnonGroupInvite, StorageError> {
    let (
        group_id,
        group_name,
        inviter_id,
        inviter_device_id,
        mailbox,
        profile_key,
        inviter_display_name,
        inviter_avatar,
        invite_epoch,
    ) = row;
    Ok(PendingAnonGroupInvite {
        group_id: group_id
            .parse()
            .map_err(|_| StorageError::NotFound("Invalid invite group ID".into()))?,
        invite_epoch,
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
                key.try_into()
                    .map_err(|_| StorageError::NotFound("Invalid inviter profile key".into()))
            })
            .transpose()?,
        inviter_display_name,
        inviter_avatar,
    })
}

/// Called under the same database transaction that publishes acceptance.
pub(super) fn require_pending_invite(
    conn: &rusqlite::Connection,
    invite: &PendingAnonGroupInvite,
) -> Result<(), StorageError> {
    let current: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pending_anon_group_invites WHERE group_id=?1
         AND invite_epoch=?2 AND inviter_id=?3 AND inviter_device_id=?4 AND inviter_mailbox_pk=?5)",
        params![
            invite.group_id.to_string(),
            invite.invite_epoch,
            invite.inviter_id.to_string(),
            invite.inviter_device_id,
            invite.inviter_mailbox_pk.as_slice()
        ],
        |row| row.get(0),
    )?;
    if !current {
        return Err(StorageError::InvalidData(
            "Invitation no longer current".into(),
        ));
    }
    Ok(())
}
