use super::*;

impl CallSignalingService {
    /// Whether `user_id` is the logged-in user — i.e. a message from one of our
    /// own devices.
    pub fn is_current_user(&self, user_id: &UserId) -> bool {
        self.channel_service
            .current_user_id()
            .is_ok_and(|me| me == *user_id)
    }

    // ================================================================
    // End-call orchestration (hangup + history + leave)
    // ================================================================

    /// End our DM call, connected or still ringing: send hangup, leave.
    ///
    /// `call_worker` must be provided since it's behind a OnceLock in the client.
    /// `reason` is what the leave's `ActiveEnded` reports. The Call bubble is
    /// filed by `call_history_recorder` on that event — the one place every
    /// end of a DM call reaches.
    #[cfg(feature = "media")]
    pub async fn end_dm_call(
        &self,
        call_worker: &CallWorkerHandle,
        reason: CallEndReason,
    ) -> Result<(), CallSignalingError> {
        // A call ended while ringing is over: an answer that lands after this
        // must not complete it (`complete_dm_call` would mark a dead call
        // connected), nor may the ring timeout end it again.
        self.take_pending_dm_call();

        let call = call_worker
            .active_call()
            .await
            .ok_or(CallSignalingError::NoPendingCall)?;

        // Send hangup notification to peer
        if let Err(e) = self
            .send_call_hangup(
                &call.channel_id,
                &call.id.to_string(),
                CallHangupReason::Normal,
            )
            .await
        {
            tracing::error!(error = %e, "Failed to send hangup notification");
        }

        // Leave the call (orchestrator cleanup is now atomic inside the worker)
        if let Err(e) = call_worker.leave_call(reason).await {
            tracing::error!(error = %e, "Failed to leave call");
        }

        Ok(())
    }

    /// End an active group/channel call: file its Call row, then leave the
    /// room.
    ///
    /// No DM hangup is sent — a channel call has many members, not a single
    /// peer. Departure is signalled by the LiveKit `ParticipantDisconnected`
    /// event when we leave the room; the initiator rekeys for the remaining
    /// members via the participant-departed path. The last participant out
    /// posts ENDED to the channel (O9), so members who never joined see the
    /// call end too; anyone else only files their own row.
    #[cfg(feature = "media")]
    pub async fn end_group_call(
        &self,
        call_worker: &CallWorkerHandle,
        duration_secs: i64,
    ) -> Result<(), CallSignalingError> {
        use crate::services::message::ChannelCallPhase;

        let call = call_worker
            .active_call()
            .await
            .ok_or(CallSignalingError::NoPendingCall)?;
        let last_out = call_worker.remote_participant_identities().await.is_empty();
        let call_id = call.id.to_string();
        let duration = u32::try_from(duration_secs.max(0)).unwrap_or(u32::MAX);
        let is_video = matches!(call.call_type, crate::domain::call::CallType::Video);

        if last_out {
            // Our row is filed first, inside; the fan-out runs detached so a
            // slow member list never holds the hangup.
            let messages = self.message_service.clone();
            let channel = call.channel_id;
            tokio::spawn(async move {
                if let Err(e) = messages
                    .post_channel_call_event(
                        &channel,
                        &call_id,
                        ChannelCallPhase::Ended,
                        duration,
                        is_video,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Channel call ENDED not posted");
                }
            });
        } else if let Ok(me) = self.message_service.current_user_id() {
            let now = crate::util::time::TimestampNanos::now().as_u64();
            if let Err(e) = self.message_service.file_channel_call_event(
                &call.channel_id,
                &me,
                &call_id,
                ChannelCallPhase::Ended,
                duration,
                now,
            ) {
                tracing::warn!(error = %e, "Failed to file group call row");
            }
        }

        // Leave the room — this is what tears down media + audio.
        if let Err(e) = call_worker.leave_call(CallEndReason::LocalHangup).await {
            tracing::error!(error = %e, "Failed to leave group call");
        }

        Ok(())
    }

    /// Handle remote hangup: disconnect locally. The Call bubble is filed by
    /// `call_history_recorder` on the resulting `ActiveEnded`.
    #[cfg(feature = "media")]
    pub async fn handle_remote_hangup(
        &self,
        call_worker: &CallWorkerHandle,
        expected_call_id: Option<&str>,
    ) -> Result<(), CallSignalingError> {
        let active = call_worker.active_call().await;
        // A named hangup for a call we are not in changes nothing: with no
        // active call there is nothing of ours to end (a ringing one is
        // withdrawn by the orchestrator, not here).
        if active.is_none() && expected_call_id.is_some() {
            return Ok(());
        }
        if let (Some(call), Some(expected)) = (active, expected_call_id) {
            // Ignore hangups for a call we are not in. Moved here so the UI
            // doesn't need to reach into call state to gate the event.
            if call.id.to_string() != expected {
                tracing::debug!(
                    expected,
                    active = %call.id,
                    "Ignoring remote hangup for non-active call"
                );
                return Ok(());
            }
        }

        call_worker
            .disconnect_local(CallEndReason::RemoteHangup)
            .await;
        Ok(())
    }
}
