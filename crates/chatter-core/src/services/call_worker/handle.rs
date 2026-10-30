use super::*;

impl CallWorkerHandle {
    #[cfg(test)]
    pub(crate) async fn test_prime_call(&self, call: Call) {
        let (response, done) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::TestPrimeCall { call, response })
            .await
            .unwrap();
        done.await.unwrap();
    }
    #[cfg(test)]
    pub(crate) async fn test_queue_prime_call(&self, call: Call) -> oneshot::Receiver<()> {
        let (response, done) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::TestPrimeCall { call, response })
            .await
            .unwrap();
        done
    }
    #[cfg(test)]
    pub(crate) async fn test_dm_answer_backend(
        &self,
        publish: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    ) {
        let (response, done) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::TestDmAnswerBackend { publish, response })
            .await
            .unwrap();
        done.await.unwrap();
    }
    #[cfg(test)]
    pub(crate) async fn test_dm_answer_effects(&self) -> (usize, usize) {
        let (response, done) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::TestDmAnswerEffects { response })
            .await
            .unwrap();
        done.await.unwrap()
    }
    pub(crate) fn for_dm_answer(
        &self,
        call: CallHandle,
        attempt: Option<crate::client::calls::CallStartAttempt>,
    ) -> Self {
        let mut worker = self.clone();
        worker.answer_scope = Some(DmAnswerScope {
            call,
            attempt,
            session_revision: self.presentation_session,
        });
        worker
    }

    /// Spawn a new call worker on a dedicated thread.
    pub fn spawn(call_client: CallClient) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let call_events_tx = CallEventsTx::new(64);
        let (room_events_broadcast, _) = broadcast::channel(128);

        // Create shared video frames storage (shared between worker and UI)
        let shared_video_frames = SharedVideoFrames::new();
        let shared_frames_for_worker = shared_video_frames.clone();
        let call_events_for_worker = call_events_tx.clone();
        let incoming_presentation = IncomingCallPresentationState::new();
        let presentation_for_worker = incoming_presentation.clone();
        let room_events_broadcast_for_worker = room_events_broadcast.clone();
        let internal_tx = cmd_tx.clone();

        // Spawn dedicated thread for call operations
        std::thread::Builder::new()
            .name("call-worker".into())
            .spawn(move || {
                run_call_worker(
                    cmd_rx,
                    internal_tx,
                    events_tx,
                    call_client,
                    shared_frames_for_worker,
                    call_events_for_worker,
                    room_events_broadcast_for_worker,
                    presentation_for_worker,
                );
            })
            .expect("Failed to spawn call worker thread");

        Self {
            cmd_tx,
            room_events_rx: Arc::new(RwLock::new(Some(events_rx))),
            room_events_broadcast,
            shared_video_frames,
            call_events_tx,
            incoming_presentation,
            presentation_session: None,
            answer_scope: None,
        }
    }

    /// Swap the worker's gRPC call client so subsequent signaling RPCs
    /// (create/join/leave/end) egress over the pool's current transport.
    /// Used on anon-mode toggle. The worker applies it between complete call
    /// operations, so no in-flight call is corrupted. Awaits enqueue so the
    /// swap is not silently dropped under buffer pressure; a closed channel
    /// (worker gone) is logged and ignored.
    pub async fn swap_call_client(&self, client: CallClient) {
        if self
            .cmd_tx
            .send(CallCommand::SwapCallClient { client })
            .await
            .is_err()
        {
            tracing::warn!("call worker: failed to enqueue call client swap (worker unavailable)");
        }
    }

    // ========================================================================
    // Orchestrator API
    // ========================================================================

    /// Subscribe to orchestrator events (incoming-call presentation, active
    /// call transitions, withdrawals). Each subscriber gets its own receiver.
    pub fn subscribe_call_events(&self) -> broadcast::Receiver<CallOrchestratorEvent> {
        self.call_events_tx.subscribe()
    }

    /// Subscribe to orchestrator events without the broadcast's lag: every
    /// event is delivered, in order, until the worker and every handle are
    /// gone. For a consumer whose record must not miss a transition.
    pub(crate) fn subscribe_call_events_lossless(
        &self,
    ) -> mpsc::UnboundedReceiver<CallOrchestratorEvent> {
        self.call_events_tx.subscribe_lossless()
    }

    pub fn for_presentation_session(&self, revision: u64) -> Self {
        let mut worker = self.clone();
        worker.presentation_session = Some(revision);
        worker
    }

    pub fn presentation_session_is_current(&self) -> bool {
        self.incoming_presentation
            .accepts_session(self.presentation_session)
    }

    /// Synchronous session lease for secret restoration; logout fences this write.
    pub fn with_current_presentation_session<R>(&self, apply: impl FnOnce() -> R) -> Option<R> {
        let state = self.incoming_presentation.snapshot.read();
        if state.session_revision != self.presentation_session
            || (self.presentation_session.is_none()
                && self
                    .incoming_presentation
                    .requires_session
                    .load(std::sync::atomic::Ordering::SeqCst))
        {
            return None;
        }
        Some(apply())
    }

    pub fn is_current_active_presentation(&self, revision: u64, call: &str) -> bool {
        let state = self.incoming_presentation.snapshot.read();
        state.session_revision == Some(revision)
            && self
                .incoming_presentation
                .active_identity
                .read()
                .as_ref()
                .is_some_and(|id| id.as_str() == call)
    }

    pub fn with_current_active_presentation<R>(
        &self,
        revision: u64,
        call: &str,
        apply: impl FnOnce() -> R,
    ) -> Option<R> {
        let session = self.incoming_presentation.snapshot.read();
        let active = self.incoming_presentation.active_identity.read();
        if session.session_revision != Some(revision)
            || !active.as_ref().is_some_and(|id| id.as_str() == call)
        {
            return None;
        }
        Some(apply())
    }

    pub async fn reset_presentation_session(
        &self,
        revision: u64,
    ) -> Result<Vec<String>, CallError> {
        // Synchronously fence old producers and queued UI callbacks before
        // waiting for the actor queue; reset removes only the retired owner.
        self.incoming_presentation.end_session(revision);
        let (response, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::ResetPresentationSession { revision, response })
            .await
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)
    }

    pub fn incoming_presentation_state(&self) -> IncomingCallPresentationState {
        match self.presentation_session {
            Some(revision) => self.incoming_presentation.for_session(revision),
            None => self.incoming_presentation.clone(),
        }
    }

    /// Subscribe before reading this snapshot. Buffered presentation events are
    /// invalidation hints only: always reread authority, never replay their data.
    pub async fn incoming_presentation_snapshot(
        &self,
    ) -> Result<IncomingCallPresentationSnapshot, CallError> {
        let (response, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::OrchIncomingSnapshot { response })
            .await
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)
    }

    /// Subscribe to LiveKit room events as a broadcast (multi-consumer).
    /// Used by core-side policy hooks that need to react to room state (e.g.
    /// key rotation on `ParticipantLeft`) alongside the UI.
    pub fn subscribe_room_events(&self) -> broadcast::Receiver<LiveKitRoomEvent> {
        self.room_events_broadcast.subscribe()
    }

    /// Register a newly received incoming call offer with the orchestrator.
    /// The orchestrator decides whether to present it fresh or flag it as
    /// replacing the active call, and emits `IncomingPresented`.
    #[allow(clippy::too_many_arguments)]
    pub fn orch_present_incoming(
        &self,
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        peer_name: String,
        is_video: bool,
        is_identity_verified: bool,
        identity_warning: String,
        ephemeral_public_key: Option<[u8; 32]>,
        channel_context: Option<String>,
        conversation_id: String,
    ) {
        self.orch_present_incoming_offer(call, kind, peer_id, peer_name, is_video,
            is_identity_verified, identity_warning, ephemeral_public_key, channel_context,
            conversation_id, 0);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn orch_present_incoming_offer(
        &self, call: CallHandle, kind: OrchestratedCallKind, peer_id: UserId,
        peer_name: String, is_video: bool, is_identity_verified: bool,
        identity_warning: String, ephemeral_public_key: Option<[u8; 32]>,
        channel_context: Option<String>, conversation_id: String, timestamp: u64,
    ) {
        let incoming = PendingIncoming {
            deadline: tokio::time::Instant::now() + incoming_ring_budget(timestamp, crate::util::time::TimestampNanos::now().as_u64()),
            session_revision: self.presentation_session,
            call,
            kind,
            peer_id,
            peer_name,
            is_video,
            is_identity_verified,
            identity_warning,
            ephemeral_public_key,
            channel_context,
            conversation_id,
        };
        let _ = self.cmd_tx.try_send(CallCommand::OrchPresentIncoming { incoming });
    }

    /// Record an incoming call whose offer is too old to ring. Touches no
    /// orchestrator state, so it is emitted straight onto the event stream.
    pub fn orch_missed_incoming(
        &self,
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_name: String,
        is_video: bool,
        conversation_id: String,
    ) {
        tracing::info!(call = %call.as_str(), "Incoming call offer is past the ring timeout — recording as missed, not ringing");
        let _ = self
            .call_events_tx
            .send(CallOrchestratorEvent::IncomingMissed {
                call,
                kind,
                peer_name,
                is_video,
                conversation_id,
            });
    }

    /// Withdraw a previously presented incoming call.
    pub fn orch_withdraw_incoming(&self, call: CallHandle, reason: CallEndReason) {
        let _ = self.cmd_tx.try_send(CallCommand::OrchWithdrawIncoming {
            session_revision: self.presentation_session,
            call,
            reason,
            response: None,
        });
    }

    /// Wait for queue space and actor completion; shutdown is reported.
    pub async fn orch_withdraw_incoming_reliably(
        &self,
        call: CallHandle,
        reason: CallEndReason,
    ) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::OrchWithdrawIncoming {
                session_revision: self.presentation_session,
                call,
                reason,
                response: Some(tx),
            })
            .await
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)
    }

    pub(crate) async fn orch_accept_presented(
        &self,
        call: CallHandle,
        replaces_active: bool,
        invites: Arc<crate::services::call_signaling::GroupCallInvites>,
    ) -> Result<PendingIncomingSnapshot, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::OrchAcceptPresented {
                call,
                replaces_active,
                invites,
                response: tx,
            })
            .await
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await
            .map_err(|_| CallError::WorkerShutdown)?
            .map(PendingIncomingSnapshot::from)
    }

    /// Register an outgoing call that was just dialed.
    pub fn orch_register_outgoing(
        &self,
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        peer_name: String,
        is_video: bool,
    ) {
        let outgoing = PendingOutgoing {
            session_revision: self
                .presentation_session
                .or_else(|| self.incoming_presentation.session_revision()),
            call,
            kind,
            peer_id,
            peer_name,
            is_video,
        };
        let _ = self.cmd_tx.try_send(CallCommand::OrchRegisterOutgoing {
            outgoing,
            session_revision: self.presentation_session,
        });
    }

    /// Notify the orchestrator that a pending call has connected.
    pub fn orch_mark_connected(
        &self,
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        is_video: bool,
    ) {
        let _ = self.cmd_tx.try_send(CallCommand::OrchMarkConnected {
            answer_scope: self.answer_scope.clone(),
            require_media_match: false,
            response: None,
            session_revision: self.presentation_session,
            call,
            kind,
            peer_id,
            is_video,
        });
    }

    pub async fn orch_mark_connected_reliably(
        &self,
        call: CallHandle,
        kind: OrchestratedCallKind,
        peer_id: UserId,
        is_video: bool,
    ) -> Result<(), CallError> {
        let (response, done) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::OrchMarkConnected {
                answer_scope: self.answer_scope.clone(),
                require_media_match: true,
                session_revision: self.presentation_session,
                response: Some(response),
                call,
                kind,
                peer_id,
                is_video,
            })
            .await
            .map_err(|_| CallError::WorkerShutdown)?;
        done.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Notify the orchestrator that the active call ended.
    pub fn orch_mark_ended(&self, call: CallHandle, reason: CallEndReason) {
        let _ = self.cmd_tx.try_send(CallCommand::OrchMarkEnded {
            call,
            reason,
            session_revision: self.presentation_session,
        });
    }

    /// Accept a presented incoming call. Returns the pending incoming record
    /// so the caller can drive the wire-level accept flow, or
    /// `Err(CallError::AlreadyInCall)` if an active call is holding the slot
    /// (caller must use [`orch_accept_incoming_replacing_active`](Self::orch_accept_incoming_replacing_active)).
    pub async fn orch_accept_incoming(
        &self,
        call: CallHandle,
    ) -> Result<PendingIncomingSnapshot, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::OrchAcceptIncoming { call, response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await
            .map_err(|_| CallError::WorkerShutdown)?
            .map(PendingIncomingSnapshot::from)
    }

    /// Accept a presented incoming call while ending the currently-active
    /// call. The orchestrator emits `ActiveEnded { ReplacedByIncoming }` for
    /// the old call and hands back the pending incoming so the caller can
    /// drive the wire-level accept.
    pub async fn orch_accept_incoming_replacing_active(
        &self,
        call: CallHandle,
    ) -> Result<PendingIncomingSnapshot, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::OrchAcceptIncomingReplacingActive { call, response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await
            .map_err(|_| CallError::WorkerShutdown)?
            .map(PendingIncomingSnapshot::from)
    }

    /// Decline a presented incoming call. Returns the pending record so the
    /// caller can send the wire-level decline.
    pub async fn orch_decline_incoming(
        &self,
        call: CallHandle,
    ) -> Result<PendingIncomingSnapshot, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::OrchDeclineIncoming { call, response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await
            .map_err(|_| CallError::WorkerShutdown)?
            .map(PendingIncomingSnapshot::from)
    }

    /// Kind of the call we are in — connected, or our outgoing call still
    /// ringing — or `None`. What `end_active_call` dispatches on, so a caller
    /// who cancels while it rings still takes the hangup path.
    pub async fn orch_active_kind(&self) -> Option<OrchestratedCallKind> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::OrchActiveKind { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Get access to shared video frames for UI rendering.
    pub fn video_frames(&self) -> &SharedVideoFrames {
        &self.shared_video_frames
    }

    /// Take the room events receiver (can only be called once).
    pub fn take_room_events(&self) -> Option<mpsc::Receiver<LiveKitRoomEvent>> {
        self.room_events_rx.write().take()
    }

    /// Initiate a new call.
    pub async fn initiate_call(
        &self,
        channel_id: ChannelId,
        title: &str,
        call_type: CallType,
        encryption_root_key: &[u8; 32],
        initiator_id: UserId,
    ) -> Result<Call, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::InitiateCall {
                attempt: crate::client::calls::START_ATTEMPT
                    .try_with(Clone::clone)
                    .ok(),
                channel_id,
                title: title.to_string(),
                call_type,
                encryption_root_key: *encryption_root_key,
                initiator_id,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Join an existing call.
    pub async fn join_call(
        &self,
        call_id: CallId,
        encryption_root_key: &[u8; 32],
        joiner_id: UserId,
    ) -> Result<Call, CallError> {
        if self.presentation_session.is_some() && !self.presentation_session_is_current() {
            return Err(CallError::WorkerShutdown);
        }
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::JoinCall {
                session_revision: self.presentation_session,
                call_id,
                encryption_root_key: Some(*encryption_root_key),
                joiner_id,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Join a channel group call holding no key: connect under a random key
    /// of our own that is never sent, and publish nothing until the key
    /// authority's rotation is installed ([`Self::install_group_key`]) or we
    /// rotate ourselves ([`Self::rotate_key`]).
    pub async fn join_group_call(
        &self,
        call_id: CallId,
        joiner_id: UserId,
    ) -> Result<Call, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::JoinCall {
                session_revision: self.presentation_session,
                call_id,
                encryption_root_key: None,
                joiner_id,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Leave the current call. `reason` is what the emitted `ActiveEnded`
    /// reports.
    pub async fn leave_call(&self, reason: CallEndReason) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::LeaveCall {
                reason,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// End the current call (creator only).
    pub async fn end_call(&self, reason: Option<&str>) -> Result<(), CallError> {
        if self.presentation_session.is_some() && !self.presentation_session_is_current() {
            return Err(CallError::WorkerShutdown);
        }
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::EndCall {
                session_revision: self.presentation_session,
                reason: reason.map(|s| s.to_string()),
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Disconnect locally without server notification. `reason` is what the
    /// emitted `ActiveEnded` reports, so the UI can say why the call went away.
    pub async fn disconnect_local(&self, reason: CallEndReason) {
        let (tx, rx) = oneshot::channel();
        let _ = self.cmd_tx.try_send(CallCommand::DisconnectLocal {
            notify_server: false,
            session_revision: self.presentation_session,
            call: None,
            reason,
            response: tx,
        });
        let _ = rx.await;
    }

    /// Fire-and-forget variant of [`disconnect_local`] for use in sync
    /// event handlers that can't `.await`.
    /// The worker will disconnect LiveKit, clear `active_call`, clear the
    /// orchestrator slot, and emit `ActiveEnded` — same as the awaited
    /// version, just without backpressure. With `call`, a no-op unless that
    /// is the active call.
    pub fn disconnect_local_nonblocking(&self, call: Option<CallHandle>, reason: CallEndReason) {
        let (tx, _rx) = oneshot::channel();
        let _ = self.cmd_tx.try_send(CallCommand::DisconnectLocal {
            notify_server: false,
            session_revision: self.presentation_session,
            call,
            reason,
            response: tx,
        });
    }

    /// Actor-atomic rollback: compare identity, notify server, disconnect media.
    pub async fn rollback_setup(&self, call: CallHandle) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(CallCommand::DisconnectLocal {
                notify_server: true,
                session_revision: self.presentation_session,
                call: Some(call),
                reason: CallEndReason::LocalHangup,
                response: tx,
            })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    /// Bind DM hangups to their peer; a group caller may only cancel its
    /// pending invitation, never end another participant's accepted media.
    pub(crate) async fn authenticate_signal_hangup(
        &self,
        call: &str,
        peer: UserId,
        own_elsewhere: bool,
    ) -> SignalHangupDisposition {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(CallCommand::AuthenticateDmHangup {
                session_revision: self
                    .presentation_session
                    .or_else(|| self.incoming_presentation.session_revision()),
                call: CallHandle::from(call),
                peer,
                own_elsewhere,
                response: tx,
            })
            .await
            .is_err()
        {
            return SignalHangupDisposition::Reject;
        }
        rx.await.unwrap_or(SignalHangupDisposition::Reject)
    }

    /// Get the currently active call.
    pub async fn active_call(&self) -> Option<Call> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::GetActiveCall { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Atomically read call identity and verification derived from installed media state.
    pub async fn verification_snapshot(&self) -> Option<CallVerificationSnapshot> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::GetVerificationSnapshot { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Check if currently in a call.
    pub async fn is_in_call(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::IsInCall { response: tx })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Identities (user ids) of the remote peers currently in the LiveKit room.
    ///
    /// This is the real room roster. `Call::participants` is not — nothing
    /// tracks remote joins/leaves into it — so anything that must act on "who
    /// is actually in the call right now" (notably key rotation) reads this.
    pub async fn remote_participant_identities(&self) -> Vec<String> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::GetRemoteParticipants { response: tx })
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Who has been in the LiveKit room longest — what the group-call key
    /// authority is chosen by. `None` when not connected.
    pub async fn room_seniority(&self) -> Option<RoomSeniority> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::GetRoomSeniority { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// The active call's group-key standing: which call, the generation of
    /// the newest key from outside our own chain, and whether our media is
    /// still held back for want of a key. `None` when not in a call.
    pub async fn group_key_status(&self) -> Option<GroupKeyStatus> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::GetGroupKeyStatus { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Read connection identity and phase in one actor turn.
    pub async fn room_connection(&self) -> Option<(u64, RoomState)> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CallCommand::GetRoomConnection { response: tx })
            .await
            .ok()?;
        rx.await.ok()
    }

    /// Get room connection state.
    pub async fn room_state(&self) -> RoomState {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::GetRoomState { response: tx })
            .is_err()
        {
            return RoomState::Disconnected;
        }
        rx.await.unwrap_or(RoomState::Disconnected)
    }

    /// Publish microphone audio.
    pub async fn publish_microphone(&self, device_id: Option<&str>) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::PublishMicrophone {
                answer_scope: self.answer_scope.clone(),
                device_id: device_id.map(|s| s.to_string()),
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Publish camera video.
    pub async fn publish_camera(&self, device_path: Option<&str>) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::PublishCamera {
                answer_scope: self.answer_scope.clone(),
                device_path: device_path.map(|s| s.to_string()),
                config: None,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Stop microphone capture.
    pub fn stop_microphone(&self) {
        let _ = self.cmd_tx.try_send(CallCommand::StopMicrophone);
    }

    /// Stop camera capture.
    pub fn stop_camera(&self) {
        let _ = self.cmd_tx.try_send(CallCommand::StopCamera);
    }

    /// Set microphone muted.
    pub fn set_microphone_muted(&self, muted: bool) {
        let _ = self
            .cmd_tx
            .try_send(CallCommand::SetMicrophoneMuted { muted });
    }

    /// Check if microphone is muted.
    pub async fn is_microphone_muted(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::IsMicrophoneMuted { response: tx })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Locally mute/unmute a specific remote peer's audio. Fire-and-forget;
    /// drops silently if the worker has shut down.
    pub fn set_peer_audio_muted(&self, participant_id: String, muted: bool) {
        let _ = self.cmd_tx.try_send(CallCommand::SetPeerAudioMuted {
            participant_id,
            muted,
        });
    }

    #[cfg(not(target_os = "android"))]
    pub async fn audio_device_snapshot(&self, output: bool) -> Result<(Vec<crate::calls::AudioDevice>, String), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.try_send(CallCommand::AudioDeviceSnapshot { output, response: tx })
            .map_err(|_| "call worker is not running".to_owned())?;
        rx.await.map_err(|_| "call worker dropped request".to_owned())?
    }

    /// Route remote audio to the named output device.
    pub async fn set_audio_output(&self, device: String) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::SetAudioOutput {
                device,
                response: tx,
            })
            .map_err(|_| "call worker is not running".to_string())?;
        rx.await
            .map_err(|_| "call worker dropped the request".to_string())?
    }

    /// Locally pause/resume a specific remote peer's video.
    pub fn set_peer_video_paused(&self, participant_id: String, paused: bool) {
        let _ = self.cmd_tx.try_send(CallCommand::SetPeerVideoPaused {
            participant_id,
            paused,
        });
    }

    /// Check if camera is enabled.
    pub async fn is_camera_enabled(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::IsCameraEnabled { response: tx })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Publish screen share (desktop only).
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub async fn publish_screen_share(&self, source: ScreenSource) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::PublishScreenShare {
                source,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    #[cfg(target_os = "android")]
    pub async fn publish_screen_share(&self) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.try_send(CallCommand::PublishAndroidScreenShare { response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Stop screen share.
    #[cfg(not(target_os = "ios"))]
    pub fn stop_screen_share(&self) {
        let _ = self.cmd_tx.try_send(CallCommand::StopScreenShare);
    }

    /// Check if screen sharing.
    pub async fn is_screen_sharing(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::IsScreenSharing { response: tx })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Ratchet the encryption key for forward secrecy.
    ///
    /// This advances the key to a new epoch, providing forward secrecy
    /// so that compromise of a later key doesn't reveal earlier messages.
    /// Returns the new key epoch, or None if not connected.
    pub async fn ratchet_key(&self) -> Option<u32> {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::RatchetKey { response: tx })
            .is_err()
        {
            return None;
        }
        rx.await.ok().flatten()
    }

    /// Rotate the encryption key after a member leaves.
    ///
    /// Replaces the current key with a new random key (not derived from the old one),
    /// so the departed member cannot decrypt future frames. Returns the new epoch.
    pub async fn rotate_key(&self, new_key: [u8; 32]) -> Result<u32, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::RotateKey {
                new_key,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Install a group-call `(key, epoch)` received from a participant of
    /// `call_id`. Returns `Ok(false)` for a stale epoch, `Err(NotInCall)` when
    /// `call_id` is not the active call.
    pub async fn install_group_key(
        &self,
        call_id: CallId,
        key: [u8; 32],
        epoch: u32,
    ) -> Result<bool, CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::InstallGroupKey {
                call_id,
                key,
                epoch,
                response: tx,
            })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Set encryption key externally (e.g., from CallKeyState ratchet).
    ///
    /// Use this when managing keys via the Signal-based CallKeyState.
    pub async fn set_encryption_key(&self, key: Vec<u8>) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::SetEncryptionKey {
                key,
                response: tx,
                answer_scope: self.answer_scope.clone(),
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Get the current key epoch.
    pub async fn key_epoch(&self) -> u32 {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::GetKeyEpoch { response: tx })
            .is_err()
        {
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    /// Test-only: `(epoch, ring)` where `ring[i]` is the shared key LiveKit's
    /// `KeyProvider` holds at key index `i` — the material frames are actually
    /// encrypted with, not our own bookkeeping. `None` when not connected.
    #[cfg(feature = "testing")]
    pub async fn installed_key_ring(&self) -> Option<(u32, Vec<Option<Vec<u8>>>)> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::GetInstalledKeyRing { response: tx })
            .ok()?;
        rx.await.ok().flatten()
    }

    /// Test-only: how many tracks we publish to the room (0 when not in one).
    #[cfg(feature = "testing")]
    pub async fn local_track_count(&self) -> usize {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .try_send(CallCommand::GetLocalTrackCount { response: tx })
            .is_err()
        {
            return 0;
        }
        rx.await.unwrap_or(0)
    }

    /// Attempt to reconnect to the current call.
    ///
    /// This requests a new token from the server and reconnects to LiveKit.
    /// Useful when the connection drops unexpectedly.
    pub async fn reconnect(&self) -> Result<(), CallError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .try_send(CallCommand::Reconnect { response: tx })
            .map_err(|_| CallError::WorkerShutdown)?;
        rx.await.map_err(|_| CallError::WorkerShutdown)?
    }

    /// Shutdown the worker.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.try_send(CallCommand::Shutdown);
    }
}

