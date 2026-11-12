use crate::error::StorageError;

#[derive(Clone)]
pub(crate) struct SendSession {
    state: crate::state::AuthState,
    owner: Option<crate::domain::ids::UserId>,
    revision: u64,
}
impl SendSession {
    pub(crate) fn capture(state: &crate::state::AuthState) -> Self {
        let snapshot = state.get();
        Self {
            state: state.clone(),
            owner: snapshot.current_user_id,
            revision: snapshot.session_revision,
        }
    }
    pub(crate) fn check(&self) -> Result<(), StorageError> {
        let current = self.state.get();
        if self.owner.is_none()
            || current.current_user_id != self.owner
            || current.session_revision != self.revision
        {
            return Err(StorageError::InvalidData(
                "Send session expired before submission".into(),
            ));
        }
        Ok(())
    }
}
tokio::task_local! {
    pub(crate) static SEND_SESSION: SendSession;
}
pub(crate) fn check_send_session() -> Result<(), StorageError> {
    SEND_SESSION.try_with(SendSession::check).unwrap_or(Ok(()))
}
