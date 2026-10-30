use super::*;

impl ChatterClient {
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
        struct AbortWait(tokio::task::AbortHandle);
        impl Drop for AbortWait {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let Some(worker) = self.call_service().get().cloned() else {
            return;
        };
        let Some(me) = self.sessions().current_user_id() else {
            return;
        };
        let signaling = self.call_signaling().clone();
        let inbox = self.device_inbox_provider().get().cloned();
        let handle = tokio::spawn(async move {
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
            if let Some(handle) = spawn_group_key_wait(
                inbox,
                worker,
                signaling,
                me,
                call.id,
                call.channel_id,
                status.anchor,
                std::time::Duration::ZERO,
            ) {
                let _abort = AbortWait(handle.abort_handle());
                let _ = handle.await;
            }
        });
        self.tasks.lock().push(handle);
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
    pub(super) async fn install_participant_group_key(
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
            CallEndReason, CallHandle, OrchestratedCallKind, incoming_ring_budget,
            offer_is_stale_now,
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
            self.call_signaling()
                .drop_ringing_group_call_invite(&call_id);
            return;
        }
        let deadline = tokio::time::Instant::now()
            + incoming_ring_budget(
                info.created_at,
                crate::util::time::TimestampNanos::now().as_u64(),
            );
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
            use crate::client::group_ring_lifecycle::{event_ends_ring, snapshot_ends_ring};
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
    pub(super) async fn join_group_call(
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
        if let Some(handle) = spawn_group_key_wait(
            self.device_inbox_provider().get().cloned(),
            worker.clone(),
            self.call_signaling().clone(),
            user_id,
            call,
            joined.channel_id,
            0,
            KEY_REQUEST_AFTER,
        ) {
            self.tasks.lock().push(handle);
        }

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
pub(super) fn spawn_group_key_wait(
    inbox: Option<std::sync::Arc<crate::inbox::DeviceInboxProvider>>,
    worker: CallWorkerHandle,
    signaling: crate::services::call_signaling::CallSignalingService,
    me: crate::domain::ids::UserId,
    call: crate::domain::ids::CallId,
    channel: crate::domain::ids::ChannelId,
    min_anchor: u32,
    first_delay: std::time::Duration,
) -> Option<tokio::task::JoinHandle<()>> {
    let call_id = call.to_string();
    let Some(guard) = signaling.begin_group_key_wait(&call_id) else {
        return None;
    };
    Some(tokio::spawn(async move {
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
    }))
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
