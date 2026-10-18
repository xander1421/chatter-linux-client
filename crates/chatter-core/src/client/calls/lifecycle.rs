use super::*;

impl ChatterClient {
    // ── Call lifecycle ─────────────────────────────────────────────

    /// End an active DM call: send hangup, leave. The Call bubble follows
    /// from the call ending (`call_history_recorder`).
    #[cfg(feature = "media")]
    pub async fn end_dm_call(&self) -> Result<(), ClientError> {
        let call_worker = self
            .call_service()
            .get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .end_dm_call(
                call_worker,
                crate::services::call_worker::CallEndReason::LocalHangup,
            )
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// End an active group/channel call: store history and leave the room.
    ///
    /// Unlike a DM call there is no single peer to notify — leaving the
    /// LiveKit room broadcasts `ParticipantDisconnected` to the other
    /// members, and the designated rotator rekeys the call for them. No
    /// DM hangup is sent (the DM hangup path resolves a single peer, which is
    /// meaningless for a multi-member channel).
    #[cfg(feature = "media")]
    pub async fn end_group_call(&self, duration_secs: i64) -> Result<(), ClientError> {
        let call_worker = self
            .call_service()
            .get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        self.call_signaling()
            .end_group_call(call_worker, duration_secs)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// End the active call regardless of kind.
    ///
    /// Resolves the call kind from the orchestrator — the connected call, or
    /// our DM call still ringing, so cancelling it sends the hangup — and
    /// dispatches to the correct wire-level tear-down. UI code should call
    /// this from its "End call" button instead of branching between
    /// `end_dm_call` / `end_group_call` based on view state.
    #[cfg(feature = "media")]
    pub async fn end_active_call(&self) -> Result<(), ClientError> {
        use crate::services::call_worker::OrchestratedCallKind;

        let worker = self
            .call_service()
            .get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;
        let kind = match worker.orch_active_kind().await {
            Some(k) => k,
            None => {
                tracing::debug!("end_active_call: no active call, nothing to do");
                return Ok(());
            }
        };
        // Core owns the connected-time baseline for both the live UI and
        // terminal metadata; never trust a separately accumulated UI timer.
        let duration_secs = worker
            .active_call()
            .await
            .map(|call| call.duration_seconds())
            .unwrap_or(0);
        match kind {
            OrchestratedCallKind::DmStandard => self.end_dm_call().await,
            OrchestratedCallKind::GroupChannel => self.end_group_call(duration_secs).await,
        }
    }
}
