use super::*;

impl GroupMailboxManager {
    /// Create a new GroupMailboxManager.
    ///
    /// # Arguments
    ///
    /// * `db` - Shared database connection
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Load complete group state from database.
    ///
    /// Returns None if no state exists for this group.
    pub fn load_group_state(
        &self,
        group_id: &ChannelId,
    ) -> Result<Option<GroupMailboxState>, MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let group_id_str = group_id.to_string();

        // Load our mailbox
        let our_mailbox = self.load_our_mailbox_internal(&conn, &group_id_str, group_id)?;

        // Load member mailboxes
        let member_mailboxes = self.load_member_mailboxes_internal(&conn, &group_id_str)?;

        // If no state exists, return None
        if our_mailbox.is_none() && member_mailboxes.is_empty() {
            return Ok(None);
        }

        Ok(Some(GroupMailboxState {
            group_id: *group_id,
            our_mailbox,
            member_mailboxes,
        }))
    }

    /// Load our mailbox from database (server-managed groups only).
    ///
    /// For anonymous groups, use `load_anon_group_mailbox` instead.
    fn load_our_mailbox_internal(
        &self,
        conn: &rusqlite::Connection,
        group_id_str: &str,
        group_id: &ChannelId,
    ) -> Result<Option<GroupMailbox>, MailboxError> {
        let mut stmt = conn
            .prepare(
                // During rotation two mailboxes coexist for a group; the NEWEST is
                // the active one that sends must target. (All non-expired rows are
                // still polled for receive via get_all_group_mailboxes.)
                "SELECT id, mailbox_public_key, mailbox_private_key, signature,
                        token_id, created_at, expires_at, last_polled_at
                 FROM group_mailboxes WHERE group_id = ?1
                 ORDER BY created_at DESC LIMIT 1",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let result = stmt.query_row(rusqlite::params![group_id_str], |row| {
            let id: String = row.get(0)?;
            let public_key: Vec<u8> = row.get(1)?;
            let private_key: Vec<u8> = row.get(2)?;
            let signature: Vec<u8> = row.get(3)?;
            let token_id: String = row.get(4)?;
            let created_at: String = row.get(5)?;
            let expires_at: String = row.get(6)?;
            let last_polled_at: Option<String> = row.get(7)?;

            Ok((
                id,
                public_key,
                private_key,
                signature,
                token_id,
                created_at,
                expires_at,
                last_polled_at,
            ))
        });

        match result {
            Ok((
                id,
                public_key,
                private_key,
                signature,
                token_id,
                created_at,
                expires_at,
                last_polled_at,
            )) => {
                let public_key_arr: [u8; 32] = public_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid public key length".into()))?;
                let private_key_arr: [u8; 64] = private_key
                    .try_into()
                    .map_err(|_| MailboxError::Storage("Invalid private key length".into()))?;

                let created_at = DateTime::parse_from_rfc3339(&created_at)
                    .map_err(|e| MailboxError::Storage(format!("Invalid created_at: {}", e)))?
                    .with_timezone(&Utc);
                let expires_at = DateTime::parse_from_rfc3339(&expires_at)
                    .map_err(|e| MailboxError::Storage(format!("Invalid expires_at: {}", e)))?
                    .with_timezone(&Utc);
                let last_polled_at = last_polled_at.and_then(|s| {
                    DateTime::parse_from_rfc3339(&s)
                        .map(|dt| dt.with_timezone(&Utc))
                        .ok()
                });

                Ok(Some(GroupMailbox {
                    id,
                    group_id: *group_id,
                    public_key: public_key_arr,
                    private_key: private_key_arr,
                    signature,
                    token_id,
                    created_at,
                    expires_at,
                    last_polled_at,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(MailboxError::Storage(e.to_string())),
        }
    }

    /// Load member mailboxes from database.
    fn load_member_mailboxes_internal(
        &self,
        conn: &rusqlite::Connection,
        group_id_str: &str,
    ) -> Result<HashMap<UserId, MemberMailbox>, MailboxError> {
        let mut stmt = conn
            .prepare(
                "SELECT user_id, mailbox_public_key, received_at, expires_at
                 FROM group_member_mailboxes WHERE group_id = ?1",
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut rows = stmt
            .query(rusqlite::params![group_id_str])
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let mut members = HashMap::new();

        while let Some(row) = rows
            .next()
            .map_err(|e| MailboxError::Storage(e.to_string()))?
        {
            let user_id_str: String = row
                .get(0)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let public_key: Vec<u8> = row
                .get(1)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let received_at_str: String = row
                .get(2)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;
            let expires_at_str: Option<String> = row
                .get(3)
                .map_err(|e| MailboxError::Storage(e.to_string()))?;

            let user_id: UserId = user_id_str.parse().map_err(|e: uuid::Error| {
                MailboxError::Storage(format!("Invalid user_id: {}", e))
            })?;

            let public_key_arr: [u8; 32] = public_key
                .try_into()
                .map_err(|_| MailboxError::Storage("Invalid member public key length".into()))?;

            let received_at = DateTime::parse_from_rfc3339(&received_at_str)
                .map_err(|e| MailboxError::Storage(format!("Invalid received_at: {}", e)))?
                .with_timezone(&Utc);

            let expires_at = expires_at_str.and_then(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .ok()
            });

            members.insert(
                user_id,
                MemberMailbox {
                    user_id,
                    public_key: public_key_arr,
                    received_at,
                    expires_at,
                },
            );
        }

        Ok(members)
    }

    /// Save/update our mailbox for a group.
    pub fn save_our_mailbox(&self, mailbox: &GroupMailbox) -> Result<(), MailboxError> {
        tracing::info!("[GROUP_MAILBOX] Saving our mailbox to database");

        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let group_id_str = mailbox.group_id.to_string();

        let rows_affected = conn
            .execute(
                "INSERT OR REPLACE INTO group_mailboxes
             (id, group_id, mailbox_public_key, mailbox_private_key, signature,
              token_id, created_at, expires_at, last_polled_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    mailbox.id,
                    group_id_str,
                    mailbox.public_key.as_slice(),
                    mailbox.private_key.as_slice(),
                    mailbox.signature,
                    mailbox.token_id,
                    mailbox.created_at.to_rfc3339(),
                    mailbox.expires_at.to_rfc3339(),
                    mailbox.last_polled_at.map(|dt| dt.to_rfc3339())
                ],
            )
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        tracing::info!(
            rows_affected = rows_affected,
            "[GROUP_MAILBOX] Mailbox saved successfully"
        );

        Ok(())
    }

    /// Save/update a member's mailbox address.
    pub fn save_member_mailbox(
        &self,
        group_id: &ChannelId,
        mailbox: &MemberMailbox,
    ) -> Result<(), MailboxError> {
        tracing::info!("[STORE_MEMBER_MAILBOX] Storing member mailbox");

        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let group_id_str = group_id.to_string();
        let user_id_str = mailbox.user_id.to_string();
        let id = Uuid::new_v4().to_string();

        conn.execute(
            "INSERT OR REPLACE INTO group_member_mailboxes
             (id, group_id, user_id, mailbox_public_key, received_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                id,
                group_id_str,
                user_id_str,
                mailbox.public_key.as_slice(),
                mailbox.received_at.to_rfc3339(),
                mailbox.expires_at.map(|dt| dt.to_rfc3339())
            ],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Remove a member's mailbox.
    pub fn remove_member_mailbox(
        &self,
        group_id: &ChannelId,
        user_id: &UserId,
    ) -> Result<(), MailboxError> {
        let conn = self
            .db
            .connection()
            .map_err(|e| MailboxError::Storage(e.to_string()))?;

        let group_id_str = group_id.to_string();
        let user_id_str = user_id.to_string();

        conn.execute(
            "DELETE FROM group_member_mailboxes WHERE group_id = ?1 AND user_id = ?2",
            rusqlite::params![group_id_str, user_id_str],
        )
        .map_err(|e| MailboxError::Storage(e.to_string()))?;

        Ok(())
    }

    /// Drop mailbox rows for users who are no longer members.
    ///
    /// Leaving a channel deletes the membership row and does not touch
    /// `group_member_mailboxes`, and nothing else calls
    /// [`Self::remove_member_mailbox`]. A fan-out over the leftover rows
    /// would hand a departed member the rotated mailbox key. `current` is
    /// the local membership list; an empty list deletes nothing — an empty
    /// read must not wipe every address.
    pub fn drop_departed_member_mailboxes(
        &self,
        group_id: &ChannelId,
        current: &[UserId],
    ) -> Result<u32, MailboxError> {
        if current.is_empty() {
            return Ok(0);
        }
        let Some(state) = self.load_group_state(group_id)? else {
            return Ok(0);
        };
        let mut dropped = 0u32;
        for user_id in state.member_mailboxes.keys() {
            if !current.contains(user_id) {
                self.remove_member_mailbox(group_id, user_id)?;
                dropped += 1;
            }
        }
        Ok(dropped)
    }
}
