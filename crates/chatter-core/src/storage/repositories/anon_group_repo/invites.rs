use super::*;

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
            FROM pending_anon_group_invites ORDER BY received_at ASC, group_id ASC",
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
}
