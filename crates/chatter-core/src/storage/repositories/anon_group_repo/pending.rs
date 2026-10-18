use super::*;

impl AnonGroupRepository {
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
            Some(crate::proto::content_v1::content::Content::AnonGroupMemberList(roster)) => {
                roster.timestamp
            }
            _ => {
                return Err(StorageError::InvalidData(
                    "Pending content is not a roster".into(),
                ));
            }
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
                    if let Some(crate::proto::content_v1::content::Content::AnonGroupMemberList(
                        roster,
                    )) = crate::proto::content_v1::Content::decode(blob.as_slice())
                        .ok()
                        .and_then(|old| old.content)
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
