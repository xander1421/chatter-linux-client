//! Call orchestration (DM / group) plus call history and lifecycle.
//!
//! Secret chats (anonymous DMs and groups) have no calls: LiveKit media would
//! leave Tor and expose the user's IP.

use super::{ChatterClient, ClientError};
#[cfg(feature = "media")]
use crate::services::call_worker::CallWorkerHandle;

/// One load of the Calls screen: the rows to render plus the unseen
/// missed-call count for the tab badge. Both come from the same blocking read so the
/// list and the badge can never disagree.
#[derive(Clone, Debug, Default)]
pub struct CallHistoryPage {
    pub rows: Vec<crate::ui::CallHistoryRowData>,
    pub missed_count: u32,
    /// Taken just before the read. Clearing "this list" passes it back so a
    /// call that arrived after the user saw the list is never deleted unseen.
    pub snapshot_at: chrono::DateTime<chrono::Utc>,
    /// Where the next page starts, when this one filled its `limit`: the
    /// `(started_at, id)` of the last stored entry read, which the
    /// repository treats as strictly-older-than. `None` = the list ends here.
    pub next_cursor: Option<CallHistoryCursor>,
}

/// Opaque to the UI: it hands the cursor back unchanged to page further.
pub type CallHistoryCursor = (chrono::DateTime<chrono::Utc>, String);

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
#[cfg(feature = "media")]
#[derive(Clone, Debug)]
pub struct CallRoomSnapshot {
    pub verification: crate::services::call_worker::CallEncryptionVerification,
    pub call: crate::domain::call::Call,
    /// None while ringing/disconnected: keep the pre-rendered peer presentation.
    pub remotes: Option<Vec<(String, crate::ui::chat_data::CallParticipantInfo)>>,
}

/// Screen presentations survive only while their owner is in the real roster.
#[cfg(feature = "media")]
pub fn room_roster_keeps_screen(
    tile_id: &str,
    tile_name: &str,
    remotes: &[(String, crate::ui::chat_data::CallParticipantInfo)],
    local: Option<&str>,
) -> bool {
    tile_name == crate::calls::tiles::LOCAL_SCREEN_TILE_NAME
        || remotes
            .iter()
            .map(|(id, _)| id.as_str())
            .chain(local)
            .any(|id| crate::calls::screen_share_frame_id(id, true) == tile_id)
}

/// Cancellation is cooperative: never drop a worker command's response future.
/// The owner drains dispatched setup and rolls it back before releasing its gate.
#[cfg(feature = "media")]
#[derive(Clone, Default)]
pub struct CallStartAttempt {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    notify: std::sync::Arc<tokio::sync::Notify>,
}
#[cfg(feature = "media")]
impl CallStartAttempt {
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

/// Shared production ownership boundary, also exercised with delayed resources
/// in tests. Cancellation never drops `setup`; it drains and rolls back it.
#[cfg(feature = "media")]
async fn owned_setup<T, F, R, RF>(
    gate: &tokio::sync::Mutex<()>,
    attempt: CallStartAttempt,
    setup: F,
    rollback: R,
) -> Result<T, ClientError>
where
    F: std::future::Future<Output = Result<T, ClientError>>,
    R: FnOnce(T) -> RF,
    RF: std::future::Future<Output = ()>,
{
    let _owner = gate.lock().await;
    if attempt.is_cancelled() {
        return Err(ClientError::Validation("call start cancelled".into()));
    }
    let result = START_ATTEMPT.scope(attempt.clone(), setup).await;
    match result {
        Ok(start) if attempt.is_cancelled() => {
            rollback(start).await;
            Err(ClientError::Validation("call start cancelled".into()))
        }
        other => other,
    }
}

impl ChatterClient {
    // ── DM Call Orchestration ──────────────────────────────────────────

    /// UI-facing summary of a freshly started outgoing call.
    ///
    /// Returned by [`ChatterClient::start_call`]. The UI only has to render
    /// this struct — no branching on conversation type, no peer-id lookups.
    // Intentionally lives next to the facade: tightly coupled and not used
    // elsewhere.
    // (Defined at module scope below; referenced here for doc linkage.)

    /// Start an outgoing call for an arbitrary conversation.
    ///
    /// Resolves the conversation kind (channel / server-DM) internally, looks
    /// up the peer when required,
    /// and dispatches to the correct signalling path. Returns an
    /// [`OutgoingCallStart`] with everything the UI needs to render the
    /// "Calling…" state.
    ///
    /// Also opens the call-history row for the outgoing side. Only DM calls
    /// are registered with the orchestrator while they ring
    /// (`orch_register_outgoing`), and that registration names no
    /// conversation, so this is the only place that sees an outgoing call of
    /// *every* conversation type at the moment it is placed;
    /// `call_history_recorder` closes the row on the terminal event.
    #[cfg(feature = "media")]
    pub async fn start_call(
        &self,
        conversation: crate::state::ActiveConversation,
        call_type: crate::domain::call::CallType,
    ) -> Result<OutgoingCallStart, ClientError> {
        self.start_call_attempt(conversation, call_type, CallStartAttempt::default())
        .await
    }

    /// Owned setup entry point. A cancelled request remains owned until any
    /// in-flight worker/server operation has returned and rollback has completed.
    #[cfg(feature = "media")]
    pub async fn start_call_attempt(
        &self,
        conversation: crate::state::ActiveConversation,
        call_type: crate::domain::call::CallType,
        attempt: CallStartAttempt,
    ) -> Result<OutgoingCallStart, ClientError> {
        let revision = self.auth_state().get().session_revision;
        owned_setup(
            &self.call_signaling().start_gate,
            attempt.clone(),
            async {
                if self.auth_state().get().session_revision != revision {
                    attempt.cancel();
                    return Err(ClientError::Validation("call session changed".into()));
                }
                let _attempt_owner = self.call_signaling().own_start(attempt.clone());
                if self.auth_state().get().session_revision != revision {
                    attempt.cancel();
                    return Err(ClientError::Validation("call session changed".into()));
                }
                let start = match self
                    .dispatch_start_call(conversation, call_type)
                    .await
                {
                    Ok(start) => start,
                    Err(error) => {
                        attempt.cancel();
                        if let Some(worker) = self.call_service().get() {
                            self.call_signaling()
                                .rollback_registered_attempt(&attempt, worker)
                                .await;
                        }
                        return Err(error);
                    }
                };
                if self.auth_state().get().session_revision != revision {
                    attempt.cancel();
                }
                if !attempt.is_cancelled() {
                    self.open_outgoing_history_row(conversation, &start).await;
                }
                if self.auth_state().get().session_revision != revision {
                    attempt.cancel();
                }
                Ok(start)
            },
            |start| async move {
                if let Some(worker) = self.call_service().get() {
                    self.call_signaling().rollback_start(&start, worker).await;
                }
            },
        )
        .await
    }

    #[cfg(feature = "media")]
    pub fn cancel_call_start(&self, call_id: &str) -> bool {
        self.call_signaling().cancel_start(call_id)
    }
    #[cfg(feature = "media")]
    pub async fn end_cancelled_call(&self, call_id: String, channel_id: String, is_video: bool) {
        let start = OutgoingCallStart {
            call_id,
            channel_id,
            is_video,
            verification_emojis: None,
            title: String::new(),
            peer: None,
        };
        self.rollback_call_start(&start).await;
    }

    #[cfg(feature = "media")]
    pub fn call_start_was_cancelled(&self, call_id: &str) -> bool {
        self.call_signaling().start_was_cancelled(call_id)
    }

    #[cfg(feature = "media")]
    pub async fn rollback_call_start(&self, start: &OutgoingCallStart) {
        let _ownership = self.call_signaling().start_gate.lock().await;
        if let Some(worker) = self.call_service().get() {
            self.call_signaling().rollback_start(start, worker).await;
        }
    }

    /// Record the "we placed a call" row.
    ///
    /// Both SQLite touches run on a blocking worker — this is reached from
    /// the UI's call-start task, and a synchronous repository call there
    /// parks a tokio async worker. Best-effort: a history write must never be
    /// what stops a call from being placed, so failures are logged and
    /// dropped. Awaited rather than detached so the row always exists before
    /// the recorder can try to close it.
    #[cfg(feature = "media")]
    async fn open_outgoing_history_row(
        &self,
        conversation: crate::state::ActiveConversation,
        start: &OutgoingCallStart,
    ) {
        use crate::domain::call::{CallDirection, CallHistoryEntry, CallResult};

        let Some((conversation_type, conversation_id)) = conversation.routing_key() else {
            return;
        };
        let conversations = self.services.conversation_repo.clone();
        let history = self.call_history_repo();
        let id = start.call_id.clone();
        let has_video = start.is_video;

        let written = tokio::task::spawn_blocking(move || {
            let peer_name = conversations
                .get_conversation(&conversation_id, &conversation_type)
                .ok()
                .flatten()
                .map(|c| c.name)
                .unwrap_or_default();

            history.insert_entry(&CallHistoryEntry {
                id,
                conversation_id,
                conversation_type,
                peer_name,
                started_at: chrono::Utc::now(),
                ended_at: None,
                direction: CallDirection::Outgoing,
                // Nobody has picked up yet. The recorder promotes this to
                // `Completed` on hangup only if the call actually connected.
                result: CallResult::Missed,
                duration_secs: None,
                has_video,
            })
        })
        .await;

        match written {
            Ok(Err(e)) => tracing::warn!(error = %e, "call history: outgoing insert failed"),
            Err(e) => tracing::warn!(error = %e, "call history: outgoing insert task failed"),
            Ok(Ok(())) => {}
        }
    }

    #[cfg(feature = "media")]
    async fn dispatch_start_call(
        &self,
        conversation: crate::state::ActiveConversation,
        call_type: crate::domain::call::CallType,
    ) -> Result<OutgoingCallStart, ClientError> {
        use crate::domain::call::CallType as CT;
        use crate::state::ActiveConversation as AC;

        let user_id = self
            .session_current_user_id()
            .ok_or_else(|| ClientError::Validation("not logged in".into()))?;
        let is_video = matches!(call_type, CT::Video);

        match conversation {
            AC::None => Err(ClientError::Validation(
                "no conversation selected for call".into(),
            )),

            // Secret chats have no calls: LiveKit media would leave Tor.
            AC::AnonDm(_) | AC::AnonGroup(_) => Err(ClientError::Validation(
                "calls are not available in secret chats".into(),
            )),

            AC::ServerDm { peer_id } => {
                // Channel-free DM: the call's local scoping id is derived
                // from the peer, same as the message conversation_id.
                let channel_id: crate::domain::ids::ChannelId = peer_id.into();
                let call_id = self
                    .start_dm_call(&peer_id, channel_id, call_type, user_id)
                    .await?;
                let peer = RingingPeer {
                    user_id: peer_id.to_string(),
                    info: self.resolve_call_participant(&peer_id.to_string()),
                };
                Ok(OutgoingCallStart {
                    call_id: call_id.to_string(),
                    channel_id: channel_id.to_string(),
                    is_video,
                    verification_emojis: None,
                    title: peer.info.display_name.clone(),
                    peer: Some(peer),
                })
            }

            AC::Channel(channel_id) => {
                let setup = self
                    .start_group_call(channel_id, call_type, user_id)
                    .await?;
                Ok(OutgoingCallStart {
                    call_id: setup.call_id.to_string(),
                    channel_id: channel_id.to_string(),
                    is_video,
                    // A group call has no SAS (as in Signal): no key is
                    // shared by everyone who could compare one.
                    verification_emojis: None,
                    title: String::new(), // UI substitutes channel name.
                    peer: None,
                })
            }
        }
    }

    /// Start a DM call: generate ephemeral key, create call on server, send offer.
    ///
    /// Returns the call ID. The UI should show "Calling..." and wait for
    /// `complete_dm_call()` when the peer answers.
    #[cfg(feature = "media")]
    pub async fn start_dm_call(
        &self,
        peer_id: &crate::domain::ids::UserId,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::ids::CallId, ClientError> {
        let call_worker = self
            .call_service()
            .get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();
        self.call_signaling()
            .start_dm_call(peer_id, channel_id, call_type, &call_worker, user_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))
    }

    /// Start a group call in a channel, or join the one it already has.
    ///
    /// We connect under a random key of our own that is never sent (§124 1k):
    /// the ring carries no key, and the call's key reaches a participant only
    /// from its key authority, after it is in the room ([`Self::handle_group_call_signal`]).
    /// The server hands back the channel's live call when there is one — then
    /// we joined it unrung, holding none of its keys: nothing is published
    /// until the authority's key arrives, and we ask for it if it does not
    /// ([`await_group_call_key`]). Group calls carry no verification emojis.
    #[cfg(feature = "media")]
    pub async fn start_group_call(
        &self,
        channel_id: crate::domain::ids::ChannelId,
        call_type: crate::domain::call::CallType,
        user_id: crate::domain::ids::UserId,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        let call_worker = self
            .call_service()
            .get()
            .ok_or_else(|| ClientError::Inbox("Call service not initialized".into()))?
            .clone();

        let own_key = zeroize::Zeroizing::new(crate::crypto::generate_group_call_key());

        let call_type_str = match call_type {
            crate::domain::call::CallType::Voice => "Voice Call",
            crate::domain::call::CallType::Video => "Video Call",
            crate::domain::call::CallType::Group => "Group Call",
        };

        let call = call_worker
            .initiate_call(channel_id, call_type_str, call_type, &own_key, user_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;
        drop(own_key);
        if start_cancelled() {
            call_worker
                .rollback_setup(crate::services::call_worker::CallHandle::from(
                    call.id.to_string().as_str(),
                ))
                .await;
            return Err(ClientError::Validation("call start cancelled".into()));
        }

        self.call_signaling()
            .register_group_start_attempt(&call.id.to_string());

        // Register the active call so the orchestrator can resolve its kind on
        // hangup. A group/channel call has no single peer, so we use a nil
        // UserId (the orchestrator only consults peer_id for the DM
        // replace-active policy, which group calls don't use). Without this,
        // `end_active_call` finds no active slot and never leaves the room.
        use crate::services::call_worker::{CallHandle, OrchestratedCallKind};
        // Await admission: a dropped best-effort notification leaves a live
        // room without an active slot or the recorder's connected timestamp.
        let handle = CallHandle::from(call.id.to_string().as_str());
        if let Err(error) = call_worker
            .orch_mark_connected_reliably(
                handle.clone(),
                OrchestratedCallKind::GroupChannel,
                crate::domain::ids::UserId::from_uuid(uuid::Uuid::nil()),
                matches!(call_type, crate::domain::call::CallType::Video),
            )
            .await
        {
            call_worker.rollback_setup(handle).await;
            return Err(ClientError::Inbox(error.to_string()));
        }

        let is_video = matches!(call_type, crate::domain::call::CallType::Video);
        if call.initiator_id != user_id {
            // The channel's call was already going: we joined it, unrung.
            tracing::info!(call_id = %call.id, "Joined the channel's live group call");
            // Rung for it too: the ring is answered now, and any key held for
            // the join belongs to this one (else it would sit in the invite).
            let call_id = call.id.to_string();
            if let Err(error) = call_worker.orch_withdraw_incoming_reliably(
                CallHandle::from(call_id.as_str()),
                crate::services::call_worker::CallEndReason::AnsweredElsewhere,
            ).await {
                tracing::debug!(%error, "Group join withdrawal worker unavailable");
            }
            let _ = self
                .services
                .notification_service
                .cancel_incoming_call(&call_id);
            for pending in self.call_signaling().take_group_call_invite(&call_id) {
                self.install_participant_group_key(
                    &call_worker,
                    call.id,
                    &channel_id,
                    pending.sender,
                    *pending.key,
                    pending.epoch,
                    false,
                )
                .await;
            }
            spawn_group_key_wait(
                self.device_inbox_provider().get().cloned(),
                call_worker.clone(),
                self.call_signaling().clone(),
                user_id,
                call.id,
                channel_id,
                0,
                KEY_REQUEST_AFTER,
            );
        } else {
            // InitiateCall already attempts initial media publication before
            // returning for the host. The worker owns that attempt and logs
            // failures; publishing again here would restart an active capture.

            // Keep ring/STARTED signaling inside the owned attempt: no detached
            // producer may advertise this call after cancellation rolled it back.
            let call_id_str = call.id.to_string();
            if !start_cancelled() {
                if let Err(e) = self
                    .call_signaling()
                    .ring_group_call(&channel_id, &call_id_str)
                    .await
                {
                    tracing::error!(call_id = %call_id_str, error = %e, "Failed to ring the group call");
                }
            }
            if !start_cancelled() {
                use crate::services::message::ChannelCallPhase;
                if let Err(e) = self
                    .messages()
                    .post_channel_call_event(
                        &channel_id,
                        &call_id_str,
                        ChannelCallPhase::Started,
                        0,
                        is_video,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Channel call STARTED not posted");
                }
            }
        }

        Ok(crate::domain::call::CallSetupResult {
            call_id: call.id,
            channel_id,
            verification_emojis: Default::default(),
            is_video,
            started_at_ns: call
                .started_at
                .and_then(|d| d.timestamp_nanos_opt())
                .filter(|n| *n > 0)
                .map(|n| n as u64)
                .unwrap_or(0),
        })
    }

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
        let _lifecycle = self.auth_lifecycle_lock.lock().await;
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
        let snapshot = worker
            .orch_accept_presented(
                handle.clone(),
                replaces_active,
                self.call_signaling().group_call_invites.clone(),
            )
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;
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

    // ── Media / encryption pass-throughs ─────────────────────────────
    //
    // UI callbacks use these instead of pulling a CallWorkerHandle out of
    // the OnceLock themselves. Keeps the "UI never touches the worker
    // handle directly" invariant the orchestrator refactor depends on.

    #[cfg(feature = "media")]
    pub async fn call_publish_microphone(
        &self,
        device_id: Option<&str>,
    ) -> Result<(), crate::error::CallError> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.publish_microphone(device_id).await,
            None => Err(crate::error::CallError::WorkerShutdown),
        }
    }

    #[cfg(feature = "media")]
    pub async fn call_publish_camera(
        &self,
        device_path: Option<&str>,
    ) -> Result<(), crate::error::CallError> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.publish_camera(device_path).await,
            None => Err(crate::error::CallError::WorkerShutdown),
        }
    }

    #[cfg(feature = "media")]
    pub fn call_stop_microphone(&self) {
        if let Some(worker) = self.call_service().get() {
            worker.stop_microphone();
        }
    }

    #[cfg(feature = "media")]
    pub fn call_stop_camera(&self) {
        if let Some(worker) = self.call_service().get() {
            worker.stop_camera();
        }
    }

    #[cfg(feature = "media")]
    pub fn call_set_microphone_muted(&self, muted: bool) {
        if let Some(worker) = self.call_service().get() {
            worker.set_microphone_muted(muted);
        }
    }

    #[cfg(feature = "media")]
    pub async fn call_is_microphone_muted(&self) -> bool {
        match self.call_service().get().cloned() {
            Some(worker) => worker.is_microphone_muted().await,
            None => false,
        }
    }

    #[cfg(feature = "media")]
    pub async fn call_is_camera_enabled(&self) -> bool {
        match self.call_service().get().cloned() {
            Some(worker) => worker.is_camera_enabled().await,
            None => false,
        }
    }

    #[cfg(feature = "media")]
    pub async fn call_is_screen_sharing(&self) -> bool {
        match self.call_service().get().cloned() {
            Some(worker) => worker.is_screen_sharing().await,
            None => false,
        }
    }

    /// True only when the LiveKit room is fully connected.
    ///
    /// Distinct from "there is an active call" — an outgoing call sits in the
    /// "Calling…" state (no room) until the peer answers. Media actions like
    /// screen share need the room, so the UI gates on this before opening the
    /// source picker; otherwise `publish_screen_share` fails `NotInCall` after
    /// the user has already interacted (and the OS portal dialog never opens).
    #[cfg(feature = "media")]
    pub async fn call_is_connected(&self) -> bool {
        match self.call_service().get().cloned() {
            Some(worker) => matches!(
                worker.room_state().await,
                crate::calls::RoomState::Connected { .. }
            ),
            None => false,
        }
    }

    /// Return the currently-active call, if any. Used by the remote-hangup
    /// handler to match incoming hangup events against the live call.
    #[cfg(feature = "media")]
    pub async fn call_active_call(&self) -> Option<crate::domain::call::Call> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.active_call().await,
            None => None,
        }
    }

    /// Read live membership bracketed by call identity checks. A leave/rejoin
    /// during the read must not attach the previous room's roster to a new call.
    #[cfg(feature = "media")]
    pub async fn call_room_snapshot(&self) -> Option<CallRoomSnapshot> {
        let worker = self.call_service().get()?.clone();
        let initial = worker.verification_snapshot().await?;
        let call = initial.call;
        let connected = matches!(
            worker.room_state().await,
            crate::calls::RoomState::Connected { .. }
        );
        let remotes = if connected && call.connected_at.is_some() {
            let mut identities = worker.remote_participant_identities().await;
            identities.sort();
            identities.dedup();
            Some(
                identities
                    .into_iter()
                    .map(|id| {
                        let info = self.resolve_call_participant(&id);
                        (id, info)
                    })
                    .collect(),
            )
        } else {
            None
        };
        let latest = worker.verification_snapshot().await?;
        let current = latest.call;
        if current.id != call.id
            || current.channel_id != call.channel_id
            || current.connected_at != call.connected_at
        {
            return None;
        }
        Some(CallRoomSnapshot {
            verification: latest.verification,
            call: current,
            remotes,
        })
    }

    /// Recovery UI needs connection identity as well as call identity. Bracket
    /// the existing call/roster snapshot so queued notices cannot acquire a
    /// replacement connection's owner.
    #[cfg(feature = "media")]
    pub async fn call_room_recovery_snapshot(
        &self,
    ) -> Option<(CallRoomSnapshot, u64, crate::calls::RoomState)> {
        let worker = self.call_service().get()?.clone();
        let (generation, state) = worker.room_connection().await?;
        let snapshot = self.call_room_snapshot().await?;
        let (latest_generation, latest_state) = worker.room_connection().await?;
        if generation != latest_generation || state != latest_state {
            return None;
        }
        Some((snapshot, generation, state))
    }

    /// Locally mute/unmute a remote participant's audio. The LiveKit
    /// stream stays subscribed; muted frames are dropped before mixing.
    #[cfg(feature = "media")]
    pub fn call_set_peer_audio_muted(&self, participant_id: String, muted: bool) {
        if let Some(worker) = self.call_service().get() {
            worker.set_peer_audio_muted(participant_id, muted);
        }
    }

    /// Route call audio to the output device `device` (an id from
    /// `AudioCapture::list_output_devices`). Applies mid-call, and to the
    /// next call if none is running.
    #[cfg(feature = "media")]
    pub async fn call_set_audio_output(&self, device: &str) -> Result<(), String> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.set_audio_output(device.to_string()).await,
            None => Err("calls are not available".to_string()),
        }
    }

    /// Locally pause/resume a remote participant's video.
    #[cfg(feature = "media")]
    pub fn call_set_peer_video_paused(&self, participant_id: String, paused: bool) {
        if let Some(worker) = self.call_service().get() {
            worker.set_peer_video_paused(participant_id, paused);
        }
    }

    #[cfg(all(feature = "media", not(any(target_os = "android", target_os = "ios"))))]
    pub async fn call_publish_screen_share(
        &self,
        source: crate::calls::ScreenSource,
    ) -> Result<(), crate::error::CallError> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.publish_screen_share(source).await,
            None => Err(crate::error::CallError::WorkerShutdown),
        }
    }

    #[cfg(all(feature = "media", target_os = "android"))]
    pub async fn call_publish_screen_share(&self) -> Result<(), crate::error::CallError> {
        match self.call_service().get().cloned() {
            Some(worker) => worker.publish_screen_share().await,
            None => Err(crate::error::CallError::WorkerShutdown),
        }
    }

    #[cfg(all(feature = "media", not(target_os = "ios")))]
    pub fn call_stop_screen_share(&self) {
        if let Some(worker) = self.call_service().get() {
            worker.stop_screen_share();
        }
    }

    pub fn call_session_is_current(&self, revision: u64) -> bool {
        self.auth_state().session_is_current(revision)
    }

    /// Authoritative incoming modal/notification state. Never contains signaling keys.
    #[cfg(feature = "media")]
    pub fn incoming_call_presentation_state(
        &self,
    ) -> Option<crate::services::call_worker::IncomingCallPresentationState> {
        self.call_service()
            .get()
            .map(|w| w.incoming_presentation_state())
    }

    /// Subscribe to the orchestrator's broadcast stream. Returns `None`
    /// until the call service has been initialized (post-login).
    #[cfg(feature = "media")]
    pub fn subscribe_call_orchestrator_events(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<crate::services::call_worker::CallOrchestratorEvent>>
    {
        self.call_service().get().map(|w| w.subscribe_call_events())
    }

    /// Clone the worker's shared video frames store. Cheap — the inner
    /// type is an `Arc` handle. Returns `None` until the call service
    /// has been initialized (post-login). The UI uses this to drive its
    /// video frame polling timer without poking the raw worker handle.
    #[cfg(feature = "media")]
    pub fn call_video_frames(&self) -> Option<crate::calls::SharedVideoFrames> {
        self.call_service().get().map(|w| w.video_frames().clone())
    }

    /// Take the LiveKit room events receiver from the worker. Can only
    /// be called once — the receiver is moved out of the worker. Returns
    /// `None` if the call service isn't initialized yet, or if the
    /// receiver has already been taken.
    #[cfg(feature = "media")]
    pub fn take_call_room_events(
        &self,
    ) -> Option<tokio::sync::mpsc::Receiver<crate::calls::LiveKitRoomEvent>> {
        self.call_service().get().and_then(|w| w.take_room_events())
    }

    /// Leave the currently-joined LiveKit room without clearing the
    /// orchestrator's active-call record. Bridge pairs this with
    /// `end_dm_call` for the full hang-up flow.
    #[cfg(feature = "media")]
    pub async fn call_leave_room(&self) -> Result<(), crate::error::CallError> {
        if let Some(worker) = self.call_service().get().cloned() {
            worker
                .leave_call(crate::services::call_worker::CallEndReason::LocalHangup)
                .await?;
        }
        Ok(())
    }

    /// Cloned handle to the currently-initialised call worker, or None
    /// when no call service has been wired up yet. Useful for bridge code
    /// that wants to call worker methods directly (mic publish, screen
    /// share, etc.) without re-fetching via `call_service()` each time.
    #[cfg(feature = "media")]
    pub fn call_service_handle_opt(
        &self,
    ) -> Option<crate::services::call_worker::CallWorkerHandle> {
        self.call_service().get().cloned()
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

    // ── Channel group call keys ──────────────────────────────────────
    //
    // One wire message, `GroupCallSignal`, in three kinds (§124 1k, after
    // Signal's group calls: RingRTC's ring carries a ring id and no key, and
    // media keys are exchanged only among devices in the call —
    // Signal-Desktop `ts/services/calling.preload.ts` `#handleGroupCallRingUpdate`,
    // `resendGroupCallMediaKeys`):
    //   * Ring — the initiator rang the channel. No key: a member who never
    //     joins learns nothing that decrypts media.
    //   * Key — the key authority's rotation, on every join and leave, sent to
    //     the participants in the room. A joiner holds no key of the call's
    //     before this (it connects under one of its own, never sent, and
    //     publishes nothing — `call_worker::PublishGate`), and is never sent
    //     one that was in use before it joined. Only the key authority's keys
    //     are installed (`is_key_authority`).
    //   * KeyRequest — a participant in the room holds no current key (the
    //     rotation was lost, or it joined unrung); the authority rotates
    //     again, for everyone in the room. Never a resend of a key in use.
    // Never in the UI.

    /// Route a received group-call signal: ring, install or hold a key, or
    /// take a key request. Senders must be members of the channel the signal
    /// names. True when a key request is due a rotation from us — the caller
    /// schedules it, coalesced with joins ([`JoinRotationGate`]).
    #[cfg(feature = "media")]
    pub(crate) async fn handle_group_call_signal(
        &self,
        call_id: String,
        sender_id: crate::domain::ids::UserId,
        channel_id: crate::domain::ids::ChannelId,
        signal: crate::domain::call::GroupCallSignal,
    ) -> bool {
        use crate::domain::call::GroupCallSignal;

        let revision = self.auth_state().get().session_revision;
        if !self.call_session_is_current(revision) {
            return false;
        }
        let Some(worker) = self
            .call_service()
            .get()
            .map(|w| w.for_presentation_session(revision))
        else {
            return false;
        };
        let Ok(call) = call_id.parse::<crate::domain::ids::CallId>() else {
            tracing::warn!(
                call_id,
                "Group call signal with a malformed call id; ignored"
            );
            return false;
        };
        let membership_started = std::time::Instant::now();
        let membership = self
            .call_signaling()
            .is_channel_member(&channel_id, &sender_id)
            .await;
        let stage = match &membership {
            Ok(true) => "group_signal_membership_validated",
            Ok(false) => "group_signal_membership_rejected",
            Err(_) => "group_signal_membership_failed",
        };
        tracing::info!(stage, call_id = %call, elapsed_us = membership_started.elapsed().as_micros() as u64, "Call stage");
        match membership {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(%sender_id, %channel_id, "Group call signal from a non-member; ignored");
                return false;
            }
            Err(e) => {
                tracing::warn!(error = %e, %channel_id, "Group call signal: membership check failed; ignored");
                return false;
            }
        }

        match signal {
            GroupCallSignal::Ring => {
                if worker.active_call().await.is_some_and(|c| c.id == call) {
                    return false;
                }
                self.present_group_call_ring(&worker, call_id, sender_id, channel_id)
                    .await;
                false
            }
            GroupCallSignal::Key { key, epoch } => {
                // Checked before the active call: a join takes the invite only
                // after the call is active, so a key that misses the invite
                // finds the call.
                if self
                    .call_signaling()
                    .buffer_group_call_key(&call_id, sender_id, key, epoch)
                {
                    return false;
                }
                if worker.active_call().await.is_some_and(|c| c.id == call) {
                    self.install_participant_group_key(
                        &worker,
                        call,
                        &channel_id,
                        sender_id,
                        key,
                        epoch,
                        true,
                    )
                    .await;
                } else {
                    tracing::debug!(
                        call_id,
                        epoch,
                        "Group call key for a call we are not in; ignored"
                    );
                }
                false
            }
            GroupCallSignal::KeyRequest => {
                let Some(me) = self.sessions().current_user_id() else {
                    return false;
                };
                let active = worker.active_call().await;
                let seniority = worker.room_seniority().await;
                let due = key_request_is_due(
                    &me,
                    call,
                    &channel_id,
                    &sender_id,
                    active.as_ref(),
                    seniority.as_ref(),
                );
                if !due {
                    tracing::debug!(%sender_id, call_id, "Group call key request not ours to answer; ignored");
                    return false;
                }
                if self
                    .call_signaling()
                    .recently_keyed(&call_id, &sender_id, KEY_IN_FLIGHT_WINDOW)
                {
                    tracing::debug!(%sender_id, call_id, "Group call key request crossed its key in flight; not re-rotating");
                    return false;
                }
                true
            }
        }
    }

    /// Ask for a fresh key after group-call signals may have been lost (the
    /// domain bus lagged, or key handling was saturated): in a channel group
    /// call where we are not the key authority, request a rotation and keep
    /// asking until a newer key is installed ([`await_group_call_key`]).
    #[cfg(feature = "media")]
    pub(crate) fn resync_group_call_key(&self) {
        let Some(worker) = self.call_service().get().cloned() else {
            return;
        };
        let Some(me) = self.sessions().current_user_id() else {
            return;
        };
        let signaling = self.call_signaling().clone();
        let inbox = self.device_inbox_provider().get().cloned();
        tokio::spawn(async move {
            use crate::services::call_worker::OrchestratedCallKind;
            // Channel group calls only: a 1:1 call has no key
            // authority to ask.
            if worker.orch_active_kind().await != Some(OrchestratedCallKind::GroupChannel) {
                return;
            }
            let Some(status) = worker.group_key_status().await else {
                return;
            };
            let Some(call) = worker
                .active_call()
                .await
                .filter(|c| c.id == status.call_id)
            else {
                return;
            };
            tracing::info!(call_id = %call.id, "Group call signals may have been lost; requesting a fresh key");
            spawn_group_key_wait(
                inbox,
                worker,
                signaling,
                me,
                call.id,
                call.channel_id,
                status.anchor,
                std::time::Duration::ZERO,
            );
        });
    }

    /// Install a participant's `(key, epoch)` into the active call `call`,
    /// if the call is in `channel_id` and they are its key authority.
    ///
    /// `settle`: a departure the rotator has already seen may not have reached
    /// our room view yet — the departed senior then still outranks the
    /// rotator. A key from a participant who is in the room but not (yet) the
    /// authority is held, and re-checked on every room event, until our view
    /// makes them the authority or [`KEY_AUTHORITY_SETTLE_MAX`] passes — not
    /// dropped after one fixed wait (§124 1k residual).
    #[cfg(feature = "media")]
    #[allow(clippy::too_many_arguments)]
    async fn install_participant_group_key(
        &self,
        worker: &CallWorkerHandle,
        call: crate::domain::ids::CallId,
        channel_id: &crate::domain::ids::ChannelId,
        sender_id: crate::domain::ids::UserId,
        key: [u8; 32],
        epoch: u32,
        settle: bool,
    ) {
        let key = zeroize::Zeroizing::new(key);
        let in_channel = worker
            .active_call()
            .await
            .is_some_and(|c| c.id == call && c.channel_id == *channel_id);
        let remote = |s: Option<crate::services::call_worker::RoomSeniority>| {
            s.map(|s| s.remote).unwrap_or_default()
        };
        let mut roster = remote(worker.room_seniority().await);
        if settle {
            let events = tokio::sync::Mutex::new(worker.subscribe_room_events());
            roster = settle_key_authority(
                &sender_id,
                roster,
                || async { remote(worker.room_seniority().await) },
                // Woken by a room event (a departure) or, at the latest, a
                // short re-read: an event lost to a lag must not stall it.
                || async {
                    let _ = tokio::time::timeout(KEY_AUTHORITY_RECHECK, events.lock().await.recv())
                        .await;
                },
                KEY_AUTHORITY_SETTLE_MAX,
            )
            .await;
        }
        if !in_channel || !is_key_authority(&sender_id, &roster) {
            tracing::warn!(%sender_id, %call, "Group call key from outside the call or not its key authority; ignored");
            return;
        }
        match worker.install_group_key(call, *key, epoch).await {
            Ok(true) => tracing::info!(%call, epoch, "Group call key installed"),
            Ok(false) => {}
            Err(e) => tracing::debug!(error = %e, %call, "Group call key not installed"),
        }
    }

    /// Ring for a channel group call — only one the server confirms is the
    /// channel's live call, started by the member who rang. A call that has
    /// been going longer than a ring lasts is filed as missed instead.
    #[cfg(feature = "media")]
    async fn present_group_call_ring(
        &self,
        worker: &CallWorkerHandle,
        call_id: String,
        sender_id: crate::domain::ids::UserId,
        channel_id: crate::domain::ids::ChannelId,
    ) {
        use crate::proto::chat_v1::CallType as ProtoCallType;
        use crate::services::call_worker::{
            CallEndReason, CallHandle, OrchestratedCallKind, incoming_ring_budget, offer_is_stale_now,
        };

        use crate::services::call_signaling::CALL_POLICY_RPC_TIMEOUT;

        let mut call_client = self.services.grpc_pool.call_client();
        let channel = channel_id.to_string();
        let lookup = tokio::time::timeout(
            CALL_POLICY_RPC_TIMEOUT,
            call_client.get_active_call_in_channel(&channel),
        );
        let info = match lookup.await {
            Err(_) => {
                tracing::warn!(
                    call_id,
                    "Group call ring: active-call lookup timed out; ignored"
                );
                return;
            }
            Ok(Ok(Some(info))) => info,
            Ok(Ok(None)) => {
                tracing::debug!(call_id, "Group call ring for a call that is over; ignored");
                return;
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, call_id, "Group call ring: active-call lookup failed; ignored");
                return;
            }
        };
        if info.id != call_id || info.created_by != sender_id.to_string() {
            tracing::warn!(call_id, %sender_id, "Group call ring does not match the channel's call; ignored");
            return;
        }
        let is_video = matches!(
            ProtoCallType::try_from(info.r#type),
            Ok(ProtoCallType::Video | ProtoCallType::ScreenShare)
        );
        let peer_name = self.display_name(&sender_id);
        let channel_name = self
            .channels()
            .get_channel_local(&channel_id)
            .ok()
            .flatten()
            .map(|c| c.name);
        let handle = CallHandle::from(call_id.as_str());

        if offer_is_stale_now(info.created_at) {
            worker.orch_missed_incoming(
                handle,
                OrchestratedCallKind::GroupChannel,
                peer_name,
                is_video,
                channel_id.to_string(),
            );
            return;
        }
        if !worker.presentation_session_is_current()
            || worker.incoming_presentation_state().session_revision()
                != Some(self.auth_state().get().session_revision)
            || !self.auth_state().is_logged_in()
        {
            return;
        }
        if !self.call_signaling().store_group_call_invite(&call_id) {
            return;
        }
        // Our own join may have completed during the lookup above. A join
        // makes the call active before it takes the invite, so re-checking
        // after the store catches every order: if the join's take already
        // ran, the call is active now and this ringing invite is stale (it
        // would swallow the live call's keys into a buffer nobody takes);
        // if not, that take removes it. The worker drops this ring too.
        let parsed = call_id.parse::<crate::domain::ids::CallId>().ok();
        if worker
            .active_call()
            .await
            .is_some_and(|c| parsed == Some(c.id))
        {
            self.call_signaling().drop_ringing_group_call_invite(&call_id);
            return;
        }
        let deadline = tokio::time::Instant::now() + incoming_ring_budget(
            info.created_at, crate::util::time::TimestampNanos::now().as_u64());
        worker.orch_present_incoming_offer(
            handle.clone(),
            OrchestratedCallKind::GroupChannel,
            sender_id,
            peer_name,
            is_video,
            false,
            String::new(),
            None,
            channel_name,
            channel_id.to_string(),
            info.created_at,
        );

        // Watch the authoritative call, not the initiator's departure: other
        // participants may keep the room alive. Reconcile after subscribing
        // and on every retry to cover a missed/reordered end or a rapid
        // start/end between the lookup above and presentation.
        let worker = worker.clone();
        let signaling = self.call_signaling().clone();
        let pool = self.services.grpc_pool.clone();
        tokio::spawn(async move {
            use super::group_ring_lifecycle::{event_ends_ring, snapshot_ends_ring};
            use crate::proto::chat_v1::{CallEventType, call_event};
            use futures::StreamExt;

            let mut events: Option<
                futures::stream::BoxStream<
                    'static,
                    Result<crate::proto::chat_v1::CallEvent, crate::error::NetworkError>,
                >,
            > = None;
            let mut reconcile = tokio::time::interval(std::time::Duration::from_secs(2));
            reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let reason = loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break CallEndReason::NoAnswer,
                    _ = reconcile.tick() => {
                        if !signaling.is_ringing_group_call_invite(handle.as_str()) {
                            return; // answered/declined: preserve keys and active media
                        }
                        if events.is_none() {
                            let mut client = pool.call_client();
                            if let Ok(Ok(stream)) = tokio::time::timeout_at(
                                deadline.min(tokio::time::Instant::now() + CALL_POLICY_RPC_TIMEOUT),
                                client.stream_call_events(vec![handle.as_str().to_owned()], vec![]),
                            ).await {
                                events = Some(Box::pin(stream));
                            }
                        }
                        // Redis PubSub has no replay. A fresh authoritative snapshot
                        // closes the subscribe gap and repairs a dropped end event.
                        let mut client = pool.call_client();
                        if let Ok(Ok(info)) = tokio::time::timeout_at(
                            deadline.min(tokio::time::Instant::now() + CALL_POLICY_RPC_TIMEOUT),
                            client.get_active_call_in_channel(&channel),
                        ).await {
                            if snapshot_ends_ring(handle.as_str(), info.as_ref().map(|i| i.id.as_str())) {
                                break CallEndReason::RemoteHangup;
                            }
                        }
                    }
                    event = async { events.as_mut().unwrap().next().await }, if events.is_some() => {
                        match event {
                            Some(Ok(event)) => {
                                if event_ends_ring(
                                    handle.as_str(), &channel, &event.call_id, &event.channel_id,
                                    event.r#type == CallEventType::Ended as i32
                                        && matches!(event.data, Some(call_event::Data::CallEnded(_))),
                                ) {
                                    break CallEndReason::RemoteHangup;
                                }
                            }
                            _ => events = None, // reconnect on next bounded reconciliation tick
                        }
                    }
                }
            };
            signaling.drop_ringing_group_call_invite(handle.as_str());
            // Only removes the incoming slot. A racing accept or an unrelated
            // active call must never be disconnected by ring cancellation.
            if let Err(error) = worker.orch_withdraw_incoming_reliably(handle, reason).await {
                tracing::debug!(%error, "Group ring withdrawal worker unavailable");
            }
        });
    }

    /// Join the group call we were rung for, holding none of its keys: our
    /// media waits behind the publish gate until the key authority's join
    /// rotation is installed — taken from what reached us while joining, or
    /// asked for if it never comes ([`await_group_call_key`]).
    #[cfg(feature = "media")]
    async fn join_group_call(
        &self,
        worker: &CallWorkerHandle,
        call_id: &str,
    ) -> Result<crate::domain::call::CallSetupResult, ClientError> {
        let call = call_id
            .parse::<crate::domain::ids::CallId>()
            .map_err(|_| ClientError::Inbox("malformed group call id".into()))?;
        tracing::info!(stage = "join_group_call_core_entry", call_id = %call, elapsed_us = 0u64, "Call stage");
        let user_id = self
            .session_current_user_id()
            .ok_or_else(|| ClientError::Inbox("Not logged in".into()))?;
        self.call_signaling().accept_group_call_invite(call_id);

        let joined = worker
            .join_group_call(call, user_id)
            .await
            .map_err(|e| ClientError::Inbox(e.to_string()))?;

        // Only the key authority's passes; the others are dropped there.
        for pending in self.call_signaling().take_group_call_invite(call_id) {
            self.install_participant_group_key(
                worker,
                call,
                &joined.channel_id,
                pending.sender,
                *pending.key,
                pending.epoch,
                false,
            )
            .await;
        }
        spawn_group_key_wait(
            self.device_inbox_provider().get().cloned(),
            worker.clone(),
            self.call_signaling().clone(),
            user_id,
            call,
            joined.channel_id,
            0,
            KEY_REQUEST_AFTER,
        );

        Ok(crate::domain::call::CallSetupResult {
            call_id: call,
            channel_id: joined.channel_id,
            verification_emojis: Default::default(),
            is_video: matches!(joined.call_type, crate::domain::call::CallType::Video),
            started_at_ns: joined
                .started_at
                .and_then(|d| d.timestamp_nanos_opt())
                .filter(|n| *n > 0)
                .map(|n| n as u64)
                .unwrap_or(0),
        })
    }

    // ── Call history ──────────────────────────────────────────────────

    // ── Local call history (v053) ──────────────────────────────────
    //
    // These methods talk to the `call_history` SQLite table via
    // `CallHistoryRepository`. Purely client-local state: the server exposes
    // no call-history RPC yet. Rows are opened by `start_call` (outgoing)
    // and `call_history_recorder` (incoming), and read by the Calls screen.

    /// Build the Calls screen's rows, newest first, plus the missed-call
    /// count the tab badge shows.
    ///
    /// Async because `CallHistoryRepository` is a synchronous SQLite handle:
    /// the read (and all the row shaping) runs on a blocking worker, never on
    /// the caller's thread. The Calls screen is painted from the Slint event
    /// loop, and a SQLite read there stalls every frame in the app.
    ///
    /// Returns display-ready data — the UI renders it without re-deriving
    /// wording, durations or routing.
    ///
    /// Only the Calls screen loads a page, so loading one is the user seeing
    /// their missed calls: it marks them seen and the badge clears.
    ///
    /// `cursor` is the previous page's [`CallHistoryPage::next_cursor`];
    /// `None` reads from the newest call.
    pub async fn call_history_page(
        &self,
        filter: crate::domain::call::CallHistoryFilter,
        limit: u32,
        cursor: Option<CallHistoryCursor>,
    ) -> Result<CallHistoryPage, ClientError> {
        let auth = self.auth_state().clone();
        let revision = auth.get().session_revision;
        let repo = self.call_history_repo();
        let mut page = tokio::task::spawn_blocking(move || {
            let snapshot_at = chrono::Utc::now();
            let entries = repo
                .list(filter, limit, cursor)
                .map_err(|e| ClientError::Inbox(format!("call history list: {}", e)))?;
            // From the stored rows, before the live-row filter below: a page
            // short by a hidden in-progress call still has older calls.
            let next_cursor = (entries.len() as u32 >= limit)
                .then(|| entries.last().map(|e| (e.started_at, e.id.clone())))
                .flatten();
            if auth.get().session_revision != revision {
                return Err(ClientError::Validation(
                    "call history session changed".into(),
                ));
            }
            let previous_missed = repo
                .missed_count()
                .map_err(|e| ClientError::Inbox(format!("call history count: {e}")))?
                as u32;
            let missed_count = match repo.mark_missed_seen() {
                Ok(()) => repo
                    .missed_count()
                    .map_err(|e| ClientError::Inbox(format!("call history count: {e}")))?
                    as u32,
                Err(e) => {
                    tracing::warn!(error = %e, "call history: mark missed seen failed");
                    previous_missed
                }
            };
            Ok::<_, ClientError>(CallHistoryPage {
                rows: entries
                    .iter()
                    .filter(|entry| {
                        // An open row created in this process may still be live;
                        // its provisional result is not a missed-call outcome.
                        filter != crate::domain::call::CallHistoryFilter::Missed
                            || entry.ended_at.is_some()
                            || entry.started_at < history_live_since()
                    })
                    .map(|entry| {
                        let mut row = crate::ui::CallHistoryRowData::from(entry);
                        if entry.ended_at.is_none() && entry.started_at >= history_live_since() {
                            row.detail = "In progress".to_string();
                            row.is_missed = false;
                            row.can_call_back = false;
                        }
                        row
                    })
                    .collect(),
                missed_count,
                snapshot_at,
                next_cursor,
            })
        })
        .await
        .map_err(|e| ClientError::Inbox(format!("call history join: {}", e)))??;
        for row in &mut page.rows {
            row.avatar_bytes = self.call_peer_avatar(&row.conversation_type, &row.conversation_id);
            if let Some(name) = self.call_peer_name(&row.conversation_type, &row.conversation_id) {
                row.initials = crate::ui::chat_data::initials_from_name(&name);
                row.name = name;
            }
        }
        Ok(page)
    }

    /// A 1:1 call row's peer, named as the chat list names them. The name
    /// stored with the entry is whatever was known when the call happened —
    /// empty for most outgoing calls, which then read "Unknown" forever.
    /// Group and channel calls keep the stored conversation name → `None`.
    fn call_peer_name(&self, conversation_type: &str, conversation_id: &str) -> Option<String> {
        use crate::domain::message::ConversationType as CT;
        match CT::from_str_value(conversation_type).ok()? {
            CT::ServerDm => Some(self.display_name(&conversation_id.parse().ok()?)),
            _ => None,
        }
    }

    /// Avatar for a call-history row: the peer's profile for a server DM or
    /// the channel icon for a channel call. Anonymous groups have no stored
    /// group icon and must not borrow an unrelated channel's icon.
    fn call_peer_avatar(&self, conversation_type: &str, conversation_id: &str) -> Option<Vec<u8>> {
        use crate::domain::message::ConversationType as CT;
        match CT::from_str_value(conversation_type).ok()? {
            CT::ServerDm => self.user_avatar(&conversation_id.parse().ok()?),
            CT::Channel => self
                .get_channel_local(&conversation_id.parse().ok()?)
                .filter(|channel| channel.conversation_type() == CT::Channel)
                .and_then(|channel| channel.icon),
            _ => None,
        }
    }

    /// Unseen missed incoming calls on their own, for painting the tab badge
    /// at startup without loading the list (which would mark them seen).
    pub async fn missed_call_count(&self) -> u32 {
        let repo = self.call_history_repo();
        tokio::task::spawn_blocking(move || repo.missed_count().unwrap_or(0) as u32)
            .await
            .unwrap_or(0)
    }

    /// Fallible badge read; UI callers retain their last successful badge on error.
    pub async fn try_missed_call_count(&self) -> Result<u32, ClientError> {
        let repo = self.call_history_repo();
        tokio::task::spawn_blocking(move || {
            repo.missed_count()
                .map(|count| count as u32)
                .map_err(|e| ClientError::Inbox(format!("call history count: {e}")))
        })
        .await
        .map_err(|e| ClientError::Inbox(format!("call history count task: {e}")))?
    }

    /// Resolve a Calls-screen row back to the conversation it happened in,
    /// so the UI can reopen it and redial. `None` when the row's key no
    /// longer names a reachable conversation.
    pub fn call_history_conversation(
        &self,
        conversation_type: &str,
        conversation_id: &str,
    ) -> Option<crate::state::ActiveConversation> {
        let ct =
            crate::domain::message::ConversationType::from_str_value(conversation_type).ok()?;
        crate::state::ActiveConversation::from_routing_key(&ct, conversation_id)
    }

    /// Delete every call `filter` lists, on this device only, that started
    /// no later than `snapshot_at` (the [`CallHistoryPage`] the user is
    /// looking at). Calls history has no server copy and no peer-side delete;
    /// chat Call bubbles are separate messages and are not touched. A call
    /// still in progress is kept. Returns the number deleted.
    pub async fn clear_call_history(
        &self,
        filter: crate::domain::call::CallHistoryFilter,
        snapshot_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, ClientError> {
        let repo = self.call_history_repo();
        tokio::task::spawn_blocking(move || {
            repo.delete_matching(filter, snapshot_at, history_live_since())
                .map_err(|e| ClientError::Inbox(format!("call history clear: {}", e)))
        })
        .await
        .map_err(|e| ClientError::Inbox(format!("call history join: {}", e)))?
    }

    /// Delete exactly the selected Calls-screen rows, on this device only;
    /// same scope and in-progress rule as
    /// [`clear_call_history`](Self::clear_call_history). Returns the number
    /// deleted, which is short of `ids.len()` when a row was kept.
    pub async fn delete_call_history_entries(&self, ids: Vec<String>) -> Result<u64, ClientError> {
        let repo = self.call_history_repo();
        tokio::task::spawn_blocking(move || {
            repo.delete_ids(&ids, history_live_since())
                .map_err(|e| ClientError::Inbox(format!("call history delete: {}", e)))
        })
        .await
        .map_err(|e| ClientError::Inbox(format!("call history join: {}", e)))?
    }

    /// Accessor used by the history helpers above and by the signaling
    /// service when it wants to record events from inside core.
    fn call_history_repo(&self) -> crate::storage::repositories::CallHistoryRepository {
        crate::storage::repositories::CallHistoryRepository::new(self.services.database.clone())
    }

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

    /// A participant left the call: rotate so `departed_identity` can't
    /// decrypt future frames ([`Self::rotate_group_call_key`]).
    #[cfg(feature = "media")]
    pub async fn handle_participant_departed(
        &self,
        departed_identity: &str,
    ) -> Result<(), ClientError> {
        self.rotate_group_call_key(Some(departed_identity)).await
    }

    /// Participants joined the call: rotate so they get a key that covered
    /// nothing from before they joined ([`Self::rotate_group_call_key`]).
    /// Runs at once on a join; joins during a run coalesce into one more run
    /// ([`JoinRotationGate`]).
    #[cfg(feature = "media")]
    pub(crate) async fn handle_participants_joined(&self) -> Result<(), ClientError> {
        self.rotate_group_call_key(None).await
    }

    /// Rotate a channel group call's key after a leave or a join, if we are
    /// its designated rotator: generate a new random key, rotate locally, and
    /// send it to the peers in the room now (`departed`, if any, excluded).
    ///
    /// A leave rotates so the departed can't decrypt what follows; a join
    /// rotates so the joiner is sent only a key that covered nothing before
    /// it joined — the key in use until now is never sent to a joiner (a
    /// member who left and rejoins would otherwise read what was said while
    /// they were away). As in Signal, membership changes rekey the call.
    ///
    /// One rotator, so concurrent rotations don't race: the participant in
    /// the room longest ([`is_designated_rotator`]). Every participant
    /// evaluates the same server-assigned join times, so they agree on it —
    /// and it is never a joiner, which holds no current key to rotate from.
    #[cfg(feature = "media")]
    async fn rotate_group_call_key(&self, departed: Option<&str>) -> Result<(), ClientError> {
        let call_worker = self
            .call_service()
            .get()
            .ok_or_else(|| ClientError::Inbox("call service not initialized".into()))?;

        let call = match call_worker.active_call().await {
            Some(c) => c,
            None => return Ok(()), // No active call
        };

        // Only rotate for channel-based group calls (not Saved Messages).
        let channel = match self.channels().get_channel_local(&call.channel_id) {
            Ok(Some(c)) => c,
            _ => return Ok(()),
        };

        use crate::domain::channel::ChannelType;
        if matches!(channel.channel_type, ChannelType::SavedMessages) {
            return Ok(());
        }

        let my_id = self
            .sessions()
            .current_user_id()
            .ok_or_else(|| ClientError::Inbox("not logged in".into()))?;

        // Sample the roster BEFORE rotating. Rotation is irreversible — it bumps
        // the epoch on the local worker — so anything fallible that the
        // distribution depends on has to happen while failing is still free. The
        // reverse order leaves the local key changed and nobody else holding it,
        // which breaks the call for every remaining participant rather than for
        // the one who left.
        //
        // The departed identity is subtracted explicitly rather than trusting the
        // SDK to have evicted it before firing ParticipantLeft — not an ordering
        // assumption this path should depend on.
        let Some(seniority) = call_worker.room_seniority().await else {
            return Ok(());
        };
        let others: Vec<(String, i64)> = seniority
            .remote
            .into_iter()
            .filter(|(id, _)| Some(id.as_str()) != departed)
            .collect();
        if !is_designated_rotator(&my_id, seniority.local_joined_at_ms, &others) {
            return Ok(());
        }
        let roster: Vec<String> = others.into_iter().map(|(id, _)| id).collect();

        // Generate new random key and rotate locally
        let new_key = crate::crypto::generate_group_call_key();
        let epoch = call_worker
            .rotate_key(new_key)
            .await
            .map_err(|e| ClientError::Inbox(format!("key rotation failed: {e}")))?;

        // No wait before sending: we already encrypt with the new key, so
        // every moment before the peers hold it is media they cannot decrypt
        // (and a joiner hears nothing until it arrives).

        // Redistribute to the peers actually still in the room, not to the
        // channel membership — a departed peer is still a member, so the old
        // membership-wide send handed the rotated key straight back to the
        // participant this rotation exists to lock out.
        if roster.is_empty() {
            // Still rotated: an empty room is not a reason to leave the departed
            // peer holding a working key.
            tracing::info!(
                epoch,
                "Rotated group call key; no other participants to redistribute to"
            );
            return Ok(());
        }

        let call_id = call.id.to_string();
        match self
            .call_signaling()
            .redistribute_group_call_key(&call.channel_id, &call_id, new_key, epoch, &roster)
            .await
        {
            Ok(count) => {
                tracing::info!(
                    epoch,
                    sent = count,
                    joined = departed.is_none(),
                    "Rotated group call key after a membership change"
                );
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to redistribute rotated group call key");
            }
        }

        Ok(())
    }
}

/// Summary returned by [`ChatterClient::start_call`] for the UI's
/// "Calling…" placeholder. Holds only view data — no service handles, no
/// crypto material. UI code should treat this as an opaque payload to
/// render, not as orchestration state.
#[derive(Debug, Clone)]
pub struct OutgoingCallStart {
    /// Stable identifier for the outgoing call: the server-assigned `CallId`
    /// stringified.
    pub call_id: String,
    /// Conversation / channel id to display in the call metadata. Stringified
    /// to avoid leaking domain ID types into the UI crate.
    pub channel_id: String,
    pub is_video: bool,
    /// Verification emojis, if already known at dial time (channel group
    /// calls). `None` for DM calls — those populate after the peer's
    /// answer arrives.
    pub verification_emojis: Option<[String; 4]>,
    /// Who is being called: the peer's display name for a DM. Empty for a
    /// channel group call,
    /// where the UI uses the channel name it already shows.
    pub title: String,
    /// The person being rung in a 1:1 call, so the ringing screen shows them
    /// rather than the caller. `None` for group calls.
    pub peer: Option<RingingPeer>,
}

/// The callee of a 1:1 outgoing call, resolved for display at dial time.
#[derive(Debug, Clone)]
pub struct RingingPeer {
    /// The peer's call identity.
    pub user_id: String,
    pub info: crate::ui::chat_data::CallParticipantInfo,
}

/// Open rows started at or after this may be live calls; see
/// `call_history_recorder::LIVE_SINCE`. Without media no call can be live.
fn history_live_since() -> chrono::DateTime<chrono::Utc> {
    #[cfg(feature = "media")]
    {
        crate::services::call_history_recorder::LIVE_SINCE
            .get()
            .copied()
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MIN_UTC)
    }
    #[cfg(not(feature = "media"))]
    {
        chrono::DateTime::<chrono::Utc>::MAX_UTC
    }
}

/// A participant's place in the order that picks the group-call key
/// authority: earliest LiveKit `joined_at_ms`, then lowest user id (LiveKit
/// identities are user ids — the server mints tokens with
/// `SetIdentity(userID)`). The join time is assigned by the LiveKit server,
/// so every participant computes the same order; like the roster itself it
/// is server-asserted (security review #9).
#[cfg(feature = "media")]
fn seniority(identity: &str, joined_at_ms: i64) -> (i64, &str) {
    (joined_at_ms, identity)
}

/// Whether `sender` is the participant whose group-call keys we install: the
/// most senior of the others in the room (`remote`: `(identity,
/// joined_at_ms)`). Every participant applies the same order, so this is the
/// designated rotator ([`is_designated_rotator`]) for all but the rotator
/// itself, which makes its rotations locally. A joiner is never the most
/// senior, so it never decides a key.
#[cfg(feature = "media")]
fn is_key_authority(sender: &crate::domain::ids::UserId, remote: &[(String, i64)]) -> bool {
    key_authority(remote).is_some_and(|id| id == sender.to_string())
}

/// Whether we rotate the call key on a leave or a join: we have been in the
/// room longer than everyone else in it (`others`, ourselves excluded).
/// Deterministic, so the participants agree on one rotator; when it leaves,
/// the next most senior takes over.
#[cfg(feature = "media")]
fn is_designated_rotator(
    me: &crate::domain::ids::UserId,
    my_joined_at_ms: i64,
    others: &[(String, i64)],
) -> bool {
    let me = me.to_string();
    others
        .iter()
        .all(|(id, at)| seniority(&me, my_joined_at_ms) < seniority(id, *at))
}

/// The participant whose keys are installed, as `remote` (the others in
/// the room) ranks it: the most senior. `None` in an empty room.
#[cfg(feature = "media")]
fn key_authority(remote: &[(String, i64)]) -> Option<&str> {
    remote
        .iter()
        .min_by(|a, b| seniority(&a.0, a.1).cmp(&seniority(&b.0, b.1)))
        .map(|(id, _)| id.as_str())
}

/// Whether a key request from `sender` for `call` in `channel` is ours to
/// answer with a rotation: it is our active call, in that channel, the
/// sender is in the room with us, and we are the key authority. A rotation —
/// never a resend: the key in use may predate the requester's join.
#[cfg(feature = "media")]
fn key_request_is_due(
    me: &crate::domain::ids::UserId,
    call: crate::domain::ids::CallId,
    channel: &crate::domain::ids::ChannelId,
    sender: &crate::domain::ids::UserId,
    active: Option<&crate::domain::call::Call>,
    room: Option<&crate::services::call_worker::RoomSeniority>,
) -> bool {
    let (Some(active), Some(room)) = (active, room) else {
        return false;
    };
    let sender = sender.to_string();
    active.id == call
        && active.channel_id == *channel
        && room.remote.iter().any(|(id, _)| *id == sender)
        && is_designated_rotator(me, room.local_joined_at_ms, &room.remote)
}

/// One step of waiting for a group-call key ([`await_group_call_key`]).
#[cfg(feature = "media")]
#[derive(Debug, PartialEq, Eq)]
enum KeyWait {
    /// Stop: the key came, the call changed, or the next rotation is ours.
    Done,
    /// Ask this participant — the key authority — for a rotation.
    Request(String),
}

/// What to do now while waiting, in `call`, for a key newer than generation
/// `min_anchor`.
#[cfg(feature = "media")]
fn key_wait_step(
    me: &crate::domain::ids::UserId,
    call: crate::domain::ids::CallId,
    min_anchor: u32,
    status: Option<crate::services::call_worker::GroupKeyStatus>,
    room: Option<&crate::services::call_worker::RoomSeniority>,
) -> KeyWait {
    let (Some(status), Some(room)) = (status, room) else {
        return KeyWait::Done;
    };
    if status.call_id != call || status.anchor > min_anchor {
        return KeyWait::Done;
    }
    // Alone, or the most senior: the next membership change rotates, and
    // the rotation is ours (`rotate_group_call_key`) — nobody to ask.
    if is_designated_rotator(me, room.local_joined_at_ms, &room.remote) {
        return KeyWait::Done;
    }
    match key_authority(&room.remote) {
        Some(authority) => KeyWait::Request(authority.to_string()),
        None => KeyWait::Done,
    }
}

/// Wait for a key newer than generation `min_anchor` in `call`, asking the
/// key authority for a rotation each time one has not come within
/// [`KEY_REQUEST_AFTER`] (the first ask after `first_delay`), at most
/// [`KEY_REQUEST_ATTEMPTS`] times. One wait per call runs at a time.
#[cfg(feature = "media")]
fn spawn_group_key_wait(
    inbox: Option<std::sync::Arc<crate::inbox::DeviceInboxProvider>>,
    worker: CallWorkerHandle,
    signaling: crate::services::call_signaling::CallSignalingService,
    me: crate::domain::ids::UserId,
    call: crate::domain::ids::CallId,
    channel: crate::domain::ids::ChannelId,
    min_anchor: u32,
    first_delay: std::time::Duration,
) {
    let call_id = call.to_string();
    let Some(guard) = signaling.begin_group_key_wait(&call_id) else {
        return;
    };
    tokio::spawn(async move {
        let _guard = guard;
        let _poll_guard = inbox.as_ref().map(|provider| provider.responsive_polling());
        let mut delay = first_delay;
        for _ in 0..KEY_REQUEST_ATTEMPTS {
            tokio::time::sleep(delay).await;
            delay = KEY_REQUEST_AFTER;
            let status = worker.group_key_status().await;
            let room = worker.room_seniority().await;
            let authority = match key_wait_step(&me, call, min_anchor, status, room.as_ref()) {
                KeyWait::Done => return,
                KeyWait::Request(authority) => authority,
            };
            let Ok(authority) = authority.parse::<crate::domain::ids::UserId>() else {
                return;
            };
            if let Err(e) = signaling
                .request_group_call_key(&channel, &call_id, &authority)
                .await
            {
                tracing::warn!(error = %e, call_id, "Group call key request not sent");
            }
        }
        tracing::warn!(
            call_id,
            "Still no group call key after {KEY_REQUEST_ATTEMPTS} requests"
        );
    });
}

/// Hold a key from `sender` — in the room, not (yet) the key authority in
/// `roster` — until a re-read roster (`read`, after each `wait`) makes it the
/// authority, shows it gone, or `max` passes. Returns the last roster read.
/// A departed senior our view still lists is waited out for as long as the
/// view takes to drop it, not one fixed interval.
#[cfg(feature = "media")]
async fn settle_key_authority<R, RF, W, WF>(
    sender: &crate::domain::ids::UserId,
    mut roster: Vec<(String, i64)>,
    mut read: R,
    mut wait: W,
    max: std::time::Duration,
) -> Vec<(String, i64)>
where
    R: FnMut() -> RF,
    RF: std::future::Future<Output = Vec<(String, i64)>>,
    W: FnMut() -> WF,
    WF: std::future::Future<Output = ()>,
{
    let sender_id = sender.to_string();
    let deadline = tokio::time::Instant::now() + max;
    while roster.iter().any(|(id, _)| *id == sender_id)
        && !is_key_authority(sender, &roster)
        && tokio::time::Instant::now() < deadline
    {
        wait().await;
        roster = read().await;
    }
    roster
}

/// Longest a key from an in-room participant who is not yet the key
/// authority in our view is held for that view to catch up. Above LiveKit's
/// own departure timeout (20 s by default), after which a participant that
/// dropped without leaving is gone from every view.
#[cfg(feature = "media")]
const KEY_AUTHORITY_SETTLE_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// While a key is held: how often the roster is re-read when no room event
/// wakes it first.
#[cfg(feature = "media")]
const KEY_AUTHORITY_RECHECK: std::time::Duration = std::time::Duration::from_millis(500);

/// A participant holding no current key asks for one when this long passes
/// without it. The authority rotates on the join at once, so the push lands
/// well inside this on a working link; the request is the fallback.
#[cfg(feature = "media")]
pub(crate) const KEY_REQUEST_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

// A request itself crosses the peer's inbox and membership check. Allow two
// maximum idle polling gaps for the already-sent key before rotating again.
// Actual join/leave rotations do not consult this request-only suppression.
#[cfg(feature = "media")]
const KEY_IN_FLIGHT_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// Key requests per wait before giving up (about a minute at [`KEY_REQUEST_AFTER`]).
#[cfg(feature = "media")]
const KEY_REQUEST_ATTEMPTS: u32 = 30;

/// Single-flight join rotation with one trailing run: a join starts a
/// rotation at once when none runs; a join during a run asks for one more
/// run after it (which reads the roster then). No fixed wait anywhere.
#[cfg(feature = "media")]
#[derive(Default)]
pub(crate) struct JoinRotationGate(parking_lot::Mutex<(bool, bool)>);

#[cfg(feature = "media")]
impl JoinRotationGate {
    /// A join (or key request) happened. True: the caller starts the
    /// rotation now. False: one is running and will run again for it.
    pub(crate) fn request(&self) -> bool {
        let mut g = self.0.lock();
        let (running, again) = &mut *g;
        if *running {
            *again = true;
            false
        } else {
            *running = true;
            true
        }
    }

    /// The running rotation finished. True: run once more (a join arrived
    /// meanwhile); false: idle.
    pub(crate) fn finish(&self) -> bool {
        let mut g = self.0.lock();
        let (running, again) = &mut *g;
        if std::mem::take(again) {
            true
        } else {
            *running = false;
            false
        }
    }
}

#[cfg(all(test, feature = "media"))]
mod group_call_key_tests {
    use super::*;
    use crate::domain::ids::UserId;

    fn user(n: u8) -> UserId {
        UserId(
            format!("00000000-0000-4000-8000-0000000000{n:02}")
                .parse()
                .unwrap(),
        )
    }

    #[test]
    fn most_senior_participant_rotates() {
        // a joined first, then b, then c; c has the lowest id.
        let (a, b, c) = (user(5), user(7), user(1));
        let view = |skip: &UserId| -> Vec<(String, i64)> {
            [(a, 100), (b, 200), (c, 300)]
                .iter()
                .filter(|(u, _)| u != skip)
                .map(|(u, at)| (u.to_string(), *at))
                .collect()
        };
        assert!(is_designated_rotator(&a, 100, &view(&a)));
        assert!(!is_designated_rotator(&b, 200, &view(&b)));
        assert!(
            !is_designated_rotator(&c, 300, &view(&c)),
            "a joiner never rotates, lowest id or not"
        );
        // a leaves: b, next most senior, takes over.
        assert!(is_designated_rotator(&b, 200, &[(c.to_string(), 300)]));
        assert!(!is_designated_rotator(&c, 300, &[(b.to_string(), 200)]));
        assert!(is_designated_rotator(&b, 200, &[]), "alone in the room");
        // Same join time: lowest id.
        assert!(is_designated_rotator(&c, 100, &[(a.to_string(), 100)]));
    }

    #[test]
    fn only_the_rotators_keys_are_installed() {
        let (rotator, mallory, joiner) = (user(9), user(1), user(3));
        // As the joiner sees the room: the rotator joined first.
        let room = vec![(mallory.to_string(), 200), (rotator.to_string(), 100)];
        assert!(is_key_authority(&rotator, &room));
        assert!(
            !is_key_authority(&mallory, &room),
            "an in-room non-rotator is ignored"
        );
        assert!(!is_key_authority(&joiner, &room), "not in the room");
        assert!(!is_key_authority(&rotator, &[]));
    }

    fn room(local: i64, remote: &[(UserId, i64)]) -> crate::services::call_worker::RoomSeniority {
        crate::services::call_worker::RoomSeniority {
            local_joined_at_ms: local,
            remote: remote.iter().map(|(u, at)| (u.to_string(), *at)).collect(),
        }
    }

    fn status(
        call: crate::domain::ids::CallId,
        anchor: u32,
    ) -> crate::services::call_worker::GroupKeyStatus {
        crate::services::call_worker::GroupKeyStatus {
            call_id: call,
            anchor,
            awaiting_key: anchor == 0,
        }
    }

    /// §124 1k item 5 / item 4: a participant holding no current key asks
    /// the most senior participant — and nobody when the rotation is its own.
    #[test]
    fn a_keyless_participant_asks_the_authority() {
        let call: crate::domain::ids::CallId =
            "11111111-2222-4333-8444-555555555555".parse().unwrap();
        let other: crate::domain::ids::CallId =
            "11111111-2222-4333-8444-666666666666".parse().unwrap();
        let (authority, middle, me) = (user(9), user(1), user(3));
        let r = room(300, &[(middle, 200), (authority, 100)]);

        assert_eq!(
            key_wait_step(&me, call, 0, Some(status(call, 0)), Some(&r)),
            KeyWait::Request(authority.to_string())
        );
        assert_eq!(
            key_wait_step(&me, call, 0, Some(status(call, 4)), Some(&r)),
            KeyWait::Done,
            "the key came"
        );
        // A resync waits for a key newer than the one it held.
        assert_eq!(
            key_wait_step(&me, call, 4, Some(status(call, 4)), Some(&r)),
            KeyWait::Request(authority.to_string())
        );
        assert_eq!(
            key_wait_step(&me, call, 4, Some(status(call, 5)), Some(&r)),
            KeyWait::Done
        );
        assert_eq!(
            key_wait_step(&me, call, 0, Some(status(other, 0)), Some(&r)),
            KeyWait::Done,
            "another call"
        );
        assert_eq!(
            key_wait_step(&me, call, 0, None, Some(&r)),
            KeyWait::Done,
            "left the call"
        );
        // The authority left: we are the most senior now, the rotation is ours.
        assert_eq!(
            key_wait_step(
                &me,
                call,
                0,
                Some(status(call, 0)),
                Some(&room(50, &[(middle, 200)]))
            ),
            KeyWait::Done
        );
        assert_eq!(
            key_wait_step(&me, call, 0, Some(status(call, 0)), Some(&room(300, &[]))),
            KeyWait::Done,
            "alone"
        );
    }

    /// A key request rotates only at the authority, only for a requester in
    /// the room with it, only in that call and channel.
    #[test]
    fn only_the_authority_answers_a_key_request_from_the_room() {
        use crate::domain::call::{Call, CallType};
        use crate::domain::ids::ChannelId;
        let (me, joiner, stranger, senior) = (user(9), user(3), user(4), user(1));
        let channel = ChannelId::new();
        let mut active = Call::new(channel, me, CallType::Voice);
        active.id = "11111111-2222-4333-8444-555555555555".parse().unwrap();
        let call = active.id;
        let r = room(100, &[(joiner, 300)]);

        assert!(key_request_is_due(
            &me,
            call,
            &channel,
            &joiner,
            Some(&active),
            Some(&r)
        ));
        assert!(
            !key_request_is_due(&me, call, &channel, &stranger, Some(&active), Some(&r)),
            "not in the room"
        );
        assert!(
            !key_request_is_due(
                &me,
                call,
                &ChannelId::new(),
                &joiner,
                Some(&active),
                Some(&r)
            ),
            "another channel"
        );
        let other_call = "11111111-2222-4333-8444-666666666666".parse().unwrap();
        assert!(
            !key_request_is_due(&me, other_call, &channel, &joiner, Some(&active), Some(&r)),
            "another call"
        );
        assert!(
            !key_request_is_due(&me, call, &channel, &joiner, None, Some(&r)),
            "not in a call"
        );
        let not_senior = room(100, &[(joiner, 300), (senior, 50)]);
        assert!(
            !key_request_is_due(
                &me,
                call,
                &channel,
                &joiner,
                Some(&active),
                Some(&not_senior)
            ),
            "not the authority"
        );
    }

    /// §124 1k item 3: a departed senior still listed in our view past the
    /// old one-shot 2 s settle no longer costs the rotator's key — it is
    /// held until the view drops the departed one.
    #[tokio::test(start_paused = true)]
    async fn a_key_waits_out_a_departed_senior_still_listed() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let (departed, rotator) = (user(1), user(9));
        let stale = vec![(departed.to_string(), 100), (rotator.to_string(), 200)];
        let reads = AtomicU32::new(0);
        let started = tokio::time::Instant::now();
        // The departure reaches our view 6 s in (12 re-reads of 500 ms).
        let roster = settle_key_authority(
            &rotator,
            stale.clone(),
            || {
                let n = reads.fetch_add(1, Ordering::SeqCst) + 1;
                let view = if n >= 12 {
                    vec![(rotator.to_string(), 200)]
                } else {
                    stale.clone()
                };
                async move { view }
            },
            || tokio::time::sleep(std::time::Duration::from_millis(500)),
            KEY_AUTHORITY_SETTLE_MAX,
        )
        .await;
        assert!(
            is_key_authority(&rotator, &roster),
            "the rotator's key was given up on"
        );
        assert!(
            started.elapsed() > std::time::Duration::from_secs(2),
            "the departure landed after the old fixed 2 s settle"
        );

        // A sender who never becomes the authority is refused, after the cap.
        let roster = settle_key_authority(
            &rotator,
            stale.clone(),
            || {
                let v = stale.clone();
                async move { v }
            },
            || tokio::time::sleep(std::time::Duration::from_millis(500)),
            KEY_AUTHORITY_SETTLE_MAX,
        )
        .await;
        assert!(!is_key_authority(&rotator, &roster));
    }

    /// A join rotates at once; joins during a running rotation cost one
    /// more run (none missed, no fixed wait); an idle gate starts again.
    #[test]
    fn joins_rotate_at_once_and_coalesce_while_running() {
        let gate = JoinRotationGate::default();
        assert!(
            gate.request(),
            "the first join starts the rotation immediately"
        );
        let piggyback = (0..5).filter(|_| gate.request()).count();
        assert_eq!(
            piggyback, 0,
            "joins during a run start nothing of their own"
        );
        assert!(gate.finish(), "…but the run goes once more for them");
        assert!(!gate.finish(), "then it idles");
        assert!(gate.request(), "a later join rotates again at once");
        assert!(!gate.finish());
    }
}

#[cfg(all(test, feature = "media"))]
mod start_ownership_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn delayed_setup_end_drains_rolls_back_before_replacement() {
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let resource = Arc::new(AtomicUsize::new(0));
        let attempt = CallStartAttempt::default();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let owner = {
            let gate = gate.clone();
            let resource = resource.clone();
            let attempt = attempt.clone();
            tokio::spawn(async move {
                let setup_resource = resource.clone();
                owned_setup(
                    &gate,
                    attempt,
                    async move {
                        // A server/media command has acquired a partial resource.
                        setup_resource.store(1, Ordering::SeqCst);
                        entered_tx.send(()).unwrap();
                        finish_rx.await.unwrap();
                        Ok(1usize)
                    },
                    |id| async move {
                        assert_eq!(resource.swap(0, Ordering::SeqCst), id);
                    },
                )
                .await
            })
        };
        entered_rx.await.unwrap();
        attempt.cancel();
        let replacement = {
            let gate = gate.clone();
            let resource = resource.clone();
            tokio::spawn(async move {
                owned_setup(
                    &gate,
                    CallStartAttempt::default(),
                    async move {
                        assert_eq!(
                            resource.swap(2, Ordering::SeqCst),
                            0,
                            "rollback must precede replacement"
                        );
                        Ok(2usize)
                    },
                    |_| async {},
                )
                .await
            })
        };
        tokio::task::yield_now().await;
        assert!(!owner.is_finished());
        assert_eq!(resource.load(Ordering::SeqCst), 1);
        finish_tx.send(()).unwrap();
        assert!(owner.await.unwrap().is_err());
        assert_eq!(replacement.await.unwrap().unwrap(), 2);
        assert_eq!(resource.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn setup_timeout_keeps_owner_until_cleanup() {
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let attempt = CallStartAttempt::default();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let (release, delayed) = tokio::sync::oneshot::channel();
        let mut task = {
            let gate = gate.clone();
            let attempt = attempt.clone();
            let cleaned = cleaned.clone();
            tokio::spawn(async move {
                owned_setup(
                    &gate,
                    attempt,
                    async {
                        delayed.await.unwrap();
                        Ok(7usize)
                    },
                    |_| async move {
                        cleaned.store(1, Ordering::SeqCst);
                    },
                )
                .await
            })
        };
        // The timeout borrows, rather than dropping, the production owner.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(5), &mut task)
                .await
                .is_err()
        );
        attempt.cancel();
        assert!(!task.is_finished());
        release.send(()).unwrap();
        assert!(task.await.unwrap().is_err());
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    }
}
