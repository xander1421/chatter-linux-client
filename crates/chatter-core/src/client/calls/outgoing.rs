use super::*;

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
                let start = match self.dispatch_start_call(conversation, call_type).await {
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
            if let Err(error) = call_worker
                .orch_withdraw_incoming_reliably(
                    CallHandle::from(call_id.as_str()),
                    crate::services::call_worker::CallEndReason::AnsweredElsewhere,
                )
                .await
            {
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
