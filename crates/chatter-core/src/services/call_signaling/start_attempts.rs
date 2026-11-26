use super::*;

/// The owner drains dispatched setup and rolls it back before releasing its gate.
#[cfg(feature = "media")]
#[derive(Clone, Default)]
pub struct CallStartAttempt {
    pub(crate) shield_confirmed: bool,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    notify: std::sync::Arc<tokio::sync::Notify>,
}
#[cfg(feature = "media")]
impl CallStartAttempt {
    pub fn new(shield_confirmed: bool) -> Self {
        Self {
            shield_confirmed,
            ..Self::default()
        }
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_one();
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.cancelled, &other.cancelled)
    }
    pub async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}
#[cfg(feature = "media")]
tokio::task_local! { pub(crate) static START_ATTEMPT: CallStartAttempt; }
pub(crate) fn start_cancelled() -> bool {
    #[cfg(feature = "media")]
    {
        START_ATTEMPT
            .try_with(CallStartAttempt::is_cancelled)
            .unwrap_or(false)
    }
    #[cfg(not(feature = "media"))]
    {
        false
    }
}

impl CallSignalingService {
    #[cfg(feature = "media")]
    pub(crate) fn own_start(
        &self,
        attempt: crate::services::call_signaling::CallStartAttempt,
    ) -> StartAttemptOwner {
        *self.inflight_start.lock() = Some(attempt);
        StartAttemptOwner(self.inflight_start.clone())
    }
    #[cfg(feature = "media")]
    pub(crate) async fn rollback_registered_attempt(
        &self,
        attempt: &crate::services::call_signaling::CallStartAttempt,
        worker: &CallWorkerHandle,
    ) {
        let ids: Vec<String> = self
            .start_attempts
            .lock()
            .iter()
            .filter(|(_, token)| token.same_owner(attempt))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.rollback_start(&id, false, worker).await;
        }
    }

    #[cfg(feature = "media")]
    pub(crate) fn register_start_attempt(&self, id: &str) {
        if let Ok(attempt) = crate::services::call_signaling::START_ATTEMPT.try_with(Clone::clone) {
            let mut attempts = self.start_attempts.lock();
            attempts.retain(|_, a| a.is_cancelled());
            self.start_groups
                .lock()
                .retain(|id| attempts.contains_key(id));
            attempts.insert(id.to_string(), attempt);
        }
    }
    #[cfg(feature = "media")]
    pub(crate) fn register_group_start_attempt(&self, id: &str) {
        self.register_start_attempt(id);
        self.start_groups.lock().insert(id.to_string());
    }
    #[cfg(feature = "media")]
    pub(crate) fn start_was_cancelled(&self, id: &str) -> bool {
        self.start_attempts
            .lock()
            .get(id)
            .is_some_and(|a| a.is_cancelled())
    }

    #[cfg(feature = "media")]
    pub(crate) fn completion_attempt(
        &self,
        id: &str,
    ) -> Option<crate::services::call_signaling::CallStartAttempt> {
        self.start_attempts.lock().get(id).cloned()
    }

    #[cfg(feature = "media")]
    pub(crate) fn cancel_start(&self, id: &str) -> bool {
        if let Some(attempt) = self.start_attempts.lock().get(id) {
            attempt.cancel();
            true
        } else {
            false
        }
    }

    /// Roll back only the named setup; delayed cleanup cannot disconnect a
    /// replacement room. Signaling and server notification are best effort.
    #[cfg(feature = "media")]
    pub(crate) async fn rollback_start(
        &self,
        call_id: &str,
        is_video: bool,
        worker: &CallWorkerHandle,
    ) {
        let id = &call_id.to_string();
        let pending = {
            let mut slot = self.pending_dm_call.lock();
            if slot.as_ref().is_some_and(|p| p.call_id.to_string() == *id) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(pending) = pending {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                self.send_call_hangup_to(&pending.peer_id, id, CallHangupReason::Normal),
            )
            .await;
        } else if !self.start_groups.lock().contains(id) {
            if let Some(call) = worker
                .active_call()
                .await
                .filter(|c| c.id.to_string() == *id)
            {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    self.send_call_hangup(&call.channel_id, id, CallHangupReason::Normal),
                )
                .await;
            }
        }
        // A channel host may already have sent some rings before cancellation.
        // Cancel those named rings without advertising any later STARTED event.
        if self.start_groups.lock().contains(id) {
            if let Some(call) = worker
                .active_call()
                .await
                .filter(|c| c.id.to_string() == *id)
            {
                if self.is_current_user(&call.initiator_id) {
                    let notify = async {
                        if let Ok(members) = self
                            .channel_service
                            .get_all_channel_members(&call.channel_id)
                            .await
                        {
                            for member in members {
                                if member.user_id != call.initiator_id {
                                    let _ = self
                                        .send_call_hangup_to(
                                            &member.user_id,
                                            id,
                                            CallHangupReason::Normal,
                                        )
                                        .await;
                                }
                            }
                        }
                        let _ = self
                            .message_service
                            .post_channel_call_event(
                                &call.channel_id,
                                id,
                                crate::services::message::ChannelCallPhase::Ended,
                                0,
                                is_video,
                            )
                            .await;
                    };
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), notify).await;
                }
            }
        }
        worker.rollback_setup(CallHandle::from(id.as_str())).await;
        self.start_attempts.lock().remove(id);
        self.start_groups.lock().remove(id);
        worker.orch_mark_ended(CallHandle::from(id.as_str()), CallEndReason::LocalHangup);
    }

    /// Store a pending outgoing DM call.
    /// Called only after the client has serialized credential teardown and
    /// stopped old producers. Dropping invite buffers zeroizes retained keys.
    pub(crate) fn clear_call_session(&self) {
        self.pending_dm_call.lock().take();
        #[cfg(feature = "media")]
        {
            if let Some(attempt) = self.inflight_start.lock().take() {
                attempt.cancel();
            }
            let mut attempts = self.start_attempts.lock();
            for attempt in attempts.values() {
                attempt.cancel();
            }
            attempts.clear();
        }
        #[cfg(feature = "media")]
        self.start_groups.lock().clear();
        self.group_call_invites.lock().clear();
        self.group_key_wait.lock().take();
        self.last_key_sent.lock().take();
    }
}
