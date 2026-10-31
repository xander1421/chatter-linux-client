//! Local ownership of cancellable presence work. Admission is serialized by
//! auth_lifecycle_lock; deregistration never needs an async lock.
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(super) struct Publications {
    state: Mutex<State>,
    finished: Notify,
}
#[derive(Default)]
struct State {
    owner: Option<u64>,
    closed_at: Option<u64>,
    cancel: CancellationToken,
    active: usize,
    resume: bool,
}
pub(super) struct Publication {
    registry: Arc<Publications>,
    pub(super) cancel: CancellationToken,
}
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Recovery {
    Open,
    Resuming,
    Retired,
}
impl Publications {
    /// Caller holds lifecycle and has verified the captured authenticated
    /// revision. A retirement revision still has credentials, but cannot reopen.
    pub(super) fn admit(self: &Arc<Self>, revision: u64) -> Option<Publication> {
        let mut state = self.state.lock();
        if state.closed_at.is_some() || state.owner != Some(revision) {
            if state.owner == Some(revision)
                || state.closed_at == Some(revision)
                || state.active != 0
            {
                return None;
            }
            state.owner = Some(revision);
            state.closed_at = None;
            state.cancel = CancellationToken::new();
        }
        state.active += 1;
        Some(Publication {
            registry: self.clone(),
            cancel: state.cancel.clone(),
        })
    }
    /// One coherent classification: last-ticket Drop may reopen the owner even
    /// while lifecycle is held by the caller observing recovery.
    pub(super) fn recovery(&self, revision: u64) -> Recovery {
        let state = self.state.lock();
        if state.owner == Some(revision) && state.closed_at.is_none() {
            Recovery::Open
        } else if state.closed_at == Some(revision) && state.resume {
            Recovery::Resuming
        } else {
            Recovery::Retired
        }
    }
    pub(super) fn accepts(&self, revision: u64) -> bool {
        let state = self.state.lock();
        state.owner == Some(revision) && state.closed_at.is_none()
    }
    /// Caller owns lifecycle; close ordinary admission before final Offline.
    pub(super) fn close(&self, retirement_revision: u64) {
        let mut state = self.state.lock();
        state.resume = false;
        state.closed_at = Some(retirement_revision);
        state.cancel.cancel();
    }
    pub(super) fn resume_when_drained(&self, revision: u64) {
        let mut state = self.state.lock();
        if state.closed_at != Some(revision) {
            return;
        }
        state.resume = true;
        Self::resume_if_idle(&mut state, revision);
    }
    fn resume_if_idle(state: &mut State, revision: u64) {
        if state.resume && state.active == 0 {
            state.owner = Some(revision);
            state.closed_at = None;
            state.cancel = CancellationToken::new();
            state.resume = false;
        }
    }
    pub(super) async fn drain(&self) {
        loop {
            let notified = self.finished.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state.lock().active == 0 {
                return;
            }
            notified.await;
        }
    }
}
impl Drop for Publication {
    fn drop(&mut self) {
        {
            let mut state = self.registry.state.lock();
            state.active -= 1;
            if let Some(revision) = state.closed_at {
                Publications::resume_if_idle(&mut state, revision);
            }
        }
        self.registry.finished.notify_waiters();
    }
}

/// Defaults to cancelled when an owner/producer is dropped or aborted.
pub(super) struct Observation(bool);
impl Observation {
    pub(super) fn new() -> Self {
        Self(false)
    }
    pub(super) fn complete(&mut self) {
        self.0 = true;
        tracing::info!(target: "call_setup::presence_completed", "Presence publication diagnostic");
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        if !self.0 {
            tracing::info!(target: "call_setup::presence_cancelled", "Presence publication diagnostic");
        }
    }
}

/// Temporary account-deletion pause. Failure or caller cancellation restores
/// this owner only while both session and credentials still match.
pub(super) struct Suspension<'a> {
    pub(super) client: &'a super::ChatterClient,
    pub(super) revision: u64,
    pub(super) generation: u64,
    pub(super) resume: bool,
}
impl Suspension<'_> {
    pub(super) fn commit(mut self) {
        self.resume = false;
    }
}
impl Drop for Suspension<'_> {
    fn drop(&mut self) {
        if self.resume
            && self.client.auth_state().session_is_current(self.revision)
            && self.client.auth().auth_interceptor.generation() == self.generation
        {
            self.client
                .presence_controller
                .publications
                .resume_when_drained(self.revision);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn recovery_snapshot_covers_last_registration_reopening() {
        let registry = Arc::new(Publications::default());
        let publication = registry.admit(7).unwrap();
        registry.close(7);
        registry.resume_when_drained(7);
        assert_eq!(registry.recovery(7), Recovery::Resuming);
        drop(publication);
        assert_eq!(registry.recovery(7), Recovery::Open);
        assert_eq!(registry.recovery(8), Recovery::Retired);
        registry.close(8);
        registry.drain().await;
        assert_eq!(registry.recovery(7), Recovery::Retired);
        assert_eq!(registry.recovery(8), Recovery::Retired);
        assert!(registry.admit(7).is_none());
        assert!(registry.admit(8).is_none());
        assert!(registry.admit(9).is_some());
    }
}
