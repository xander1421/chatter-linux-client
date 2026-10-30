use super::*;

pub struct PrivateChannelStore {
    db: Arc<Database>,
}
impl PrivateChannelStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }
    /// Redemption and nonce consumption share one SQL transaction.
    pub fn redeem(
        &self,
        invite: &Invite,
        identity: &MeshIdentity,
        member: Member,
    ) -> Result<Option<PrivateChannel>, MeshError> {
        use rusqlite::OptionalExtension;
        let mut conn = self.db.connection()?;
        let tx = conn.transaction().map_err(db_error)?;
        let bytes: Option<Vec<u8>> = tx
            .query_row(
                "SELECT state FROM mesh_private_channels WHERE channel_id=?1",
                params![invite.channel_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let Some(bytes) = bytes.map(Zeroizing::new) else {
            return Ok(None);
        };
        let mut state: PrivateChannel =
            serde_json::from_slice(&bytes).map_err(|_| failure("corrupt channel state"))?;
        if !state.verify()
            || state.creator != invite.creator
            || state.creator_key != invite.creator_key
            || state.creator_static != invite.creator_static
            || invite.expires_at < super::super::now_secs()
            || !state.pending_invites.iter().any(|issued| {
                issued.nonce == invite.nonce
                    && issued.target == invite.target
                    && issued.signature == invite.signature
            })
        {
            return Ok(None);
        }
        state.rotate(identity, Some(member), None)?;
        state
            .pending_invites
            .retain(|issued| issued.nonce != invite.nonce);
        let new_bytes = Zeroizing::new(
            serde_json::to_vec(&state).map_err(|_| failure("invalid channel state"))?,
        );
        tx.execute(
            "UPDATE mesh_private_channels SET state=?2 WHERE channel_id=?1",
            params![invite.channel_id, new_bytes.as_slice()],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(Some(state))
    }
    pub fn save(&self, state: &PrivateChannel) -> Result<(), MeshError> {
        use rusqlite::OptionalExtension;
        if !state.verify() {
            return Err(failure("invalid private channel state"));
        }
        let bytes = Zeroizing::new(
            serde_json::to_vec(state).map_err(|_| failure("invalid channel state"))?,
        );
        let mut conn = self.db.connection()?;
        let tx = conn.transaction().map_err(db_error)?;
        let previous: Option<Vec<u8>> = tx
            .query_row(
                "SELECT state FROM mesh_private_channels WHERE channel_id=?1",
                params![state.id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        if let Some(previous) = previous.map(Zeroizing::new) {
            let old: PrivateChannel =
                serde_json::from_slice(&previous).map_err(|_| failure("corrupt channel state"))?;
            if !old.verify()
                || state.epoch < old.epoch
                || (state.epoch == old.epoch && state.signature != old.signature)
            {
                return Err(failure("stale or conflicting channel epoch"));
            }
        }
        tx.execute(
            "INSERT INTO mesh_private_channels(channel_id,state) VALUES(?1,?2)
            ON CONFLICT(channel_id) DO UPDATE SET state=excluded.state",
            params![state.id, bytes.as_slice()],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(())
    }
    pub fn load(&self, id: &str) -> Result<Option<PrivateChannel>, MeshError> {
        use rusqlite::OptionalExtension;
        let conn = self.db.read_connection()?;
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT state FROM mesh_private_channels WHERE channel_id=?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let bytes = Zeroizing::new(bytes);
        // Pre-release sender-key rows have no epoch key and are abandoned.
        let Ok(state) = serde_json::from_slice::<PrivateChannel>(&bytes) else {
            return Ok(None);
        };
        if !state.verify() {
            return Err(failure("invalid group signature"));
        }
        Ok(Some(state))
    }
    pub fn list(&self) -> Result<Vec<PrivateChannel>, MeshError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn
            .prepare("SELECT state FROM mesh_private_channels")
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(db_error)?;
        let mut out = Vec::new();
        for row in rows {
            let bytes = Zeroizing::new(row.map_err(db_error)?);
            let Ok(state) = serde_json::from_slice::<PrivateChannel>(&bytes) else {
                continue;
            };
            if !state.verify() {
                return Err(failure("invalid channel signature"));
            }
            out.push(state);
        }
        Ok(out)
    }
    pub fn remove(&self, id: &str) -> Result<(), MeshError> {
        self.db
            .connection()?
            .execute(
                "DELETE FROM mesh_private_channels WHERE channel_id=?1",
                params![id],
            )
            .map_err(db_error)?;
        Ok(())
    }
    pub fn save_pending(&self, invite: &Invite) -> Result<(), MeshError> {
        let bytes = serde_json::to_vec(invite).map_err(|_| failure("invalid invite"))?;
        self.db
            .connection()?
            .execute(
                "INSERT INTO mesh_private_joins(channel_id,invite) VALUES(?1,?2)
            ON CONFLICT(channel_id) DO UPDATE SET invite=excluded.invite",
                params![invite.channel_id, bytes],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn save_pending_leave(&self, leave: &PendingLeave) -> Result<(), MeshError> {
        let bytes = serde_json::to_vec(leave).map_err(|_| failure("invalid leave notice"))?;
        self.db
            .connection()?
            .execute(
                "INSERT INTO mesh_private_joins(channel_id,invite) VALUES(?1,?2)
             ON CONFLICT(channel_id) DO UPDATE SET invite=excluded.invite",
                params![format!("leave:{}", leave.notice.id), bytes],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn pending_leaves_for(&self, recipient: &PeerId) -> Result<Vec<PendingLeave>, MeshError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn
            .prepare("SELECT invite FROM mesh_private_joins WHERE channel_id LIKE 'leave:%'")
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(db_error)?;
        let mut leaves = Vec::new();
        for row in rows {
            let bytes = row.map_err(db_error)?;
            let leave: PendingLeave =
                serde_json::from_slice(&bytes).map_err(|_| failure("corrupt pending leave"))?;
            if leave
                .recipients
                .iter()
                .any(|peer| peer == recipient.as_str())
                && !leave
                    .acknowledged
                    .iter()
                    .any(|peer| peer == recipient.as_str())
            {
                leaves.push(leave);
            }
        }
        Ok(leaves)
    }

    pub fn clear_pending_leave(
        &self,
        id: &str,
        epoch: u32,
        recipient: &PeerId,
    ) -> Result<(), MeshError> {
        for mut leave in self.pending_leaves_for(recipient)? {
            if leave.notice.id == id && leave.notice.epoch == epoch {
                leave.acknowledged.push(recipient.to_string());
                if leave.acknowledged.len() == leave.recipients.len() {
                    self.db
                        .connection()?
                        .execute(
                            "DELETE FROM mesh_private_joins WHERE channel_id=?1",
                            params![format!("leave:{id}")],
                        )
                        .map_err(db_error)?;
                } else {
                    self.save_pending_leave(&leave)?;
                }
            }
        }
        Ok(())
    }
    pub fn pending(&self, id: &str) -> Result<Option<Invite>, MeshError> {
        use rusqlite::OptionalExtension;
        let conn = self.db.read_connection()?;
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT invite FROM mesh_private_joins WHERE channel_id=?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        bytes
            .map(|b| serde_json::from_slice(&b).map_err(|_| failure("corrupt pending invite")))
            .transpose()
    }
    pub fn clear_pending(&self, id: &str) -> Result<(), MeshError> {
        self.db
            .connection()?
            .execute(
                "DELETE FROM mesh_private_joins WHERE channel_id=?1",
                params![id],
            )
            .map_err(db_error)?;
        Ok(())
    }
    pub fn wipe(&self) -> Result<(), MeshError> {
        self.db
            .connection()?
            .execute("DELETE FROM mesh_private_channels", [])
            .map_err(db_error)?;
        self.db
            .connection()?
            .execute("DELETE FROM mesh_private_joins", [])
            .map_err(db_error)?;
        Ok(())
    }
}
