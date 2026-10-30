use super::*;

impl AnonGroupRepository {
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
}
