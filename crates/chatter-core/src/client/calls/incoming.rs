use super::*;

/// An accepted incoming call, as the UI renders it — no orchestrator internals.
#[cfg(feature = "media")]
#[derive(Debug)]
pub enum AcceptedIncomingCall {
    /// Standard DM call (Signal-style ephemeral key exchange).
    Dm {
        setup: crate::domain::call::CallSetupResult,
        peer_id: crate::domain::ids::UserId,
        peer_name: String,
        is_video: bool,
    },
    /// Channel group call joined from its ring.
    Group {
        setup: crate::domain::call::CallSetupResult,
        /// The channel's name, for the call view and return-to-call bar.
        title: String,
    },
}

/// Presentation snapshot: membership comes from the live room, never Call::participants.

impl ChatterClient {
    /// Complete an outgoing DM call after receiving the peer's ephemeral key answer.
    ///
    /// Returns verification data (emojis) for the UI.
    #[cfg(feature = "media")]
    pub async fn complete_dm_call(
        &self,
        peer_ephemeral_pk: [u8; 32],
        call_id: &str,
        peer_id: &crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        let revision = self.auth_state().get().session_revision;
        self.complete_dm_call_for_session(peer_ephemeral_pk, call_id, peer_id, revision)
            .await
    }

    /// Retain answer ownership before UI dispatch, even if logout clears its registry.
    #[cfg(feature = "media")]
    pub fn call_completion_attempt(&self, call_id: &str) -> Option<CallStartAttempt> {
        self.call_signaling().completion_attempt(call_id)
    }

    #[cfg(feature = "media")]
    pub async fn complete_dm_call_for_session(
        &self,
        peer_ephemeral_pk: [u8; 32],
        call_id: &str,
        peer_id: &crate::domain::ids::UserId,
        revision: u64,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        if !self.call_session_is_current(revision) {
            return Err(ClientError::Validation("call session changed".into()));
        }
        let call_worker = self
            .call_service()
            .get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .for_presentation_session(revision);
        let result = self
            .call_signaling()
            .complete_dm_call(
                peer_ephemeral_pk,
                call_id,
                peer_id,
                self.signal(),
                &call_worker,
            )
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()));
        if !self.call_session_is_current(revision) {
            return Err(ClientError::Validation("call session changed".into()));
        }
        result
    }

    /// Accept an incoming DM call: generate ephemeral key, derive shared secret, join call.
    ///
    /// Returns verification data (emojis) for the UI.
    #[cfg(feature = "media")]
    pub async fn accept_dm_call(
        &self,
        caller_ephemeral_pk: [u8; 32],
        call_id: &str,
        caller_id: &crate::domain::ids::UserId,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        let call_worker = self
            .call_service()
            .get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .accept_dm_call(
                caller_ephemeral_pk,
                call_id,
                caller_id,
                self.signal(),
                &call_worker,
                user_id,
            )
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    // ── High-level orchestrated accept / decline ─────────────────────────
    //
    // UI callbacks should call these instead of assembling the orchestrator
    // transitions + wire-level accept + mark-connected by hand. All call
    // state lives in the worker actor; the UI is a thin renderer.

    /// Accept a presented incoming call (the one the orchestrator fired
    /// `IncomingPresented` for). Routes the orchestrator transition, fires
    /// the wire-level hangup on the old active call when `replaces_active`,
    /// runs the appropriate accept flow (DM / group channel), and
    /// promotes the orchestrator's active slot on success.
    #[cfg(feature = "media")]
    pub async fn accept_presented_incoming_call(
        &self,
        call_id: &str,
        replaces_active: bool,
    ) -> Result<AcceptedIncomingCall, ClientError> {
        self.accept_presented_incoming_call_in_session(
            call_id,
            replaces_active,
            self.auth_state().get().session_revision,
        )
        .await
    }

    #[cfg(feature = "media")]
    pub async fn accept_presented_incoming_call_in_session(
        &self,
        call_id: &str,
        replaces_active: bool,
        revision: u64,
    ) -> Result<AcceptedIncomingCall, ClientError> {
        tracing::info!(target: "call_setup::accept_core_begin", "Call setup diagnostic");
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        tracing::info!(target: "call_setup::accept_lifecycle_acquired", "Call setup diagnostic");
        if !self.call_session_is_current(revision) {
            return Err(ClientError::Inbox("Call session expired".into()));
        }
        use crate::services::call_worker::{CallEndReason, CallHandle, OrchestratedCallKind};

        let worker = self
            .call_service()
            .get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .for_presentation_session(revision);

        let handle = CallHandle::from(call_id);
        let snapshot = crate::calls::setup_diagnostics::observe(
            crate::calls::setup_diagnostics::Phase::OrchestratorAccept,
            worker.orch_accept_presented(
                handle.clone(),
                replaces_active,
                self.call_signaling().group_call_invites.clone(),
            ),
        )
        .await
        .map_err(|e| match e {
            crate::error::CallError::NotInCall => ClientError::CallEnded,
            other => ClientError::Inbox(other.to_string()),
        })?;
        // The call has left the ringing set, so its ring notification goes —
        // whether the answer came from the in-app modal or the notification's
        // own Accept, and whether or not the connect below succeeds.
        let _ = self
            .services
            .notification_service
            .cancel_incoming_call(call_id);

        if replaces_active {
            // Fire wire-level hangup on the current active call so the old
            // peer sees the call end. Best-effort — the new call proceeds
            // regardless.
            if let Err(e) = worker.end_call(Some("replaced")).await {
                tracing::debug!(error = %e, "End-call (replaced) failed");
            }
        }

        let kind = snapshot.kind;
        let peer_id = snapshot.peer_id;
        let is_video = snapshot.is_video;

        let accepted = async {
            Ok::<_, ClientError>(match kind {
                OrchestratedCallKind::DmStandard => {
                    let ephemeral = snapshot.ephemeral_public_key.ok_or_else(|| {
                        ClientError::Inbox(
                            "DM accept missing ephemeral key in orchestrator snapshot".into(),
                        )
                    })?;
                    let user_id = self
                        .session_current_user_id()
                        .ok_or_else(|| ClientError::Inbox("Not logged in".into()))?;
                    let setup = self
                        .accept_dm_call(ephemeral, call_id, &peer_id, user_id)
                        .await?;
                    worker
                        .orch_mark_connected_reliably(
                            CallHandle::from(setup.call_id.to_string()),
                            kind,
                            peer_id,
                            is_video,
                        )
                        .await
                        .map_err(|e| ClientError::Inbox(e.to_string()))?;
                    AcceptedIncomingCall::Dm {
                        setup,
                        peer_id,
                        peer_name: snapshot.peer_name,
                        is_video,
                    }
                }
                OrchestratedCallKind::GroupChannel => {
                    let setup = self.join_group_call(&worker, call_id).await?;
                    worker
                        .orch_mark_connected_reliably(
                            CallHandle::from(call_id),
                            kind,
                            peer_id,
                            setup.is_video,
                        )
                        .await
                        .map_err(|e| ClientError::Inbox(e.to_string()))?;
                    AcceptedIncomingCall::Group {
                        setup,
                        title: snapshot.channel_context.unwrap_or_default(),
                    }
                }
            })
        }
        .await;

        // The call left the ringing set above, so nothing else will ever end
        // it: the history row stayed "missed" and the caller — told we
        // answered, since the answer goes out before the join — sat in a room
        // nobody joins. End it here: `ActiveEnded(Error)` files the row as
        // failed and takes down the call view, and the hangup ends the caller's.
        if let Err(e) = &accepted {
            worker.orch_mark_ended(handle, CallEndReason::Error(e.to_string()));
            if kind == OrchestratedCallKind::GroupChannel {
                self.call_signaling().take_group_call_invite(call_id);
            }
            if kind == OrchestratedCallKind::DmStandard {
                if let Err(e) = self
                    .call_signaling()
                    .send_call_hangup(
                        &crate::domain::ids::ChannelId::from(peer_id),
                        call_id,
                        crate::services::gateway::CallHangupReason::NetworkError,
                    )
                    .await
                {
                    tracing::warn!(error = %e, call_id, "Failed-accept hangup not delivered");
                }
            }
        }
        accepted
    }

    /// Decline a presented incoming call. Runs the orchestrator transition
    /// (which emits `IncomingWithdrawn { Declined }`) and then fires the
    /// wire-level decline on the appropriate transport.
    #[cfg(feature = "media")]
    pub async fn decline_presented_incoming_call(&self, call_id: &str) -> Result<(), ClientError> {
        self.decline_presented_incoming_call_in_session(
            call_id,
            self.auth_state().get().session_revision,
        )
        .await
    }

    #[cfg(feature = "media")]
    pub async fn decline_presented_incoming_call_in_session(
        &self,
        call_id: &str,
        revision: u64,
    ) -> Result<(), ClientError> {
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
        if !self.call_session_is_current(revision) {
            return Err(ClientError::Inbox("Call session expired".into()));
        }
        use crate::services::call_worker::{CallHandle, OrchestratedCallKind};

        let worker = self
            .call_service()
            .get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();

        let snapshot = worker
            .orch_decline_incoming(CallHandle::from(call_id))
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;

        match snapshot.kind {
            OrchestratedCallKind::DmStandard => {
                // The caller hears the decline end-to-end; our other
                // devices get its DECLINED_ELSEWHERE copy and stop ringing.
                if let Err(e) = self
                    .call_signaling()
                    .send_call_decline(&snapshot.peer_id, call_id)
                    .await
                {
                    tracing::warn!(error = %e, call_id, "Call decline not delivered");
                }
                self.decline_call(call_id)
                    .await
                    .map_err(|e| ClientError::Inbox(format!("decline_call: {e}")))?;
            }
            // A group call goes on without us (Signal): declining only stops
            // our ringing. Nothing goes on the wire — we never joined the call
            // a server-side leave would take us out of.
            OrchestratedCallKind::GroupChannel => {
                self.call_signaling().take_group_call_invite(call_id);
            }
        }
        Ok(())
    }
}
