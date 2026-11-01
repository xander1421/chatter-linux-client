use crate::domain::ids::UserId;

/// Owned session proof carried across async preparation and queued DB work.
#[derive(Clone)]
pub(crate) struct ReceiveSession {
    state: super::AuthState,
    owner: UserId,
    revision: u64,
}
impl ReceiveSession {
    pub(crate) fn capture(state: &super::AuthState) -> Result<Self, crate::error::StorageError> {
        let state = state.clone();
        let snapshot = state.get();
        let owner = snapshot.current_user_id.ok_or_else(|| {
            crate::error::StorageError::InvalidData("Receive session unavailable".into())
        })?;
        Ok(Self {
            state,
            owner,
            revision: snapshot.session_revision,
        })
    }
    pub(crate) fn check(&self) -> Result<(), crate::error::StorageError> {
        let now = self.state.get();
        if now.current_user_id != Some(self.owner) || now.session_revision != self.revision {
            return Err(crate::error::StorageError::InvalidData(
                "Receive session expired".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn check_owner(
        &self,
        conn: &rusqlite::Connection,
    ) -> Result<(), crate::error::StorageError> {
        self.check()?;
        let owner: String = conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [crate::storage::repositories::settings_repo::keys::CURRENT_USER_ID],
            |r| r.get(0),
        )?;
        if serde_json::from_str::<String>(&owner).ok().as_deref()
            != Some(self.owner.to_string().as_str())
        {
            return Err(crate::error::StorageError::InvalidData(
                "Receive owner changed".into(),
            ));
        }
        Ok(())
    }
}
