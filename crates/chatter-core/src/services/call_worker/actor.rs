use super::*;

/// Derive the orchestrator `CallHandle` from the active call state.
pub(super) fn derive_call_handle(state: &ActiveCallState) -> CallHandle {
    CallHandle::from(state.call.id.to_string())
}

/// Run the call worker on the current thread.
///
/// This creates a single-threaded tokio runtime with LocalSet for non-Send futures.
pub(super) fn run_call_worker(
    mut cmd_rx: mpsc::Receiver<CallCommand>,
    internal_tx: mpsc::Sender<CallCommand>,
    events_tx: mpsc::Sender<LiveKitRoomEvent>,
    call_client: CallClient,
    shared_video_frames: SharedVideoFrames,
    call_events_tx: CallEventsTx,
    room_events_broadcast: broadcast::Sender<LiveKitRoomEvent>,
    incoming_presentation: IncomingCallPresentationState,
) {
    // Create single-threaded runtime for non-Send types
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create call worker runtime");

    let local = tokio::task::LocalSet::new();

    local.block_on(&rt, async move {
        tracing::info!("Call worker started");

        // Create the actual call service (LiveKit types stay on this thread)
        // Pass shared_video_frames so UI can access video frames directly
        let (room_manager, mut room_events_rx) = RoomManager::with_shared_frames(Some(shared_video_frames));
        let room_manager = Arc::new(room_manager);
        let call_client = Arc::new(TokioMutex::new(call_client));
        // Single-threaded: this state lives entirely on the call-worker thread
        // (created here, accessed synchronously or from spawn_local tasks, never
        // sent across threads — the handle reaches it only via the command
        // channel). Rc<RefCell> is the honest type; Rc-not-Send compile-guards
        // the invariant.
        let active_call: Rc<RefCell<Option<ActiveCallState>>> =
            Rc::new(RefCell::new(None));

        // Key ratchet timer control
        let ratchet_cancel: Rc<RefCell<Option<tokio::sync::oneshot::Sender<()>>>> =
            Rc::new(RefCell::new(None));

        // Orchestrator state (single source of truth for incoming/outgoing/active
        // call presentation policy).
        let mut orchestrator = OrchestratorState::default();
        // Controlled SDK boundary: production guards and actor queue remain real.
        #[cfg(test)]
        let mut answer_backend: Option<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>> = None;
        #[cfg(test)]
        let mut answer_effects = (0usize, 0usize);


        // Forward room events to the handle (UI via mpsc) and fan out to any
        // core-side policy subscribers (e.g. key rotation) via broadcast.
        let events_tx_clone = events_tx.clone();
        let room_bus = room_events_broadcast.clone();
        let lost_call = active_call.clone();
        let command_sender = internal_tx.clone();
        tokio::task::spawn_local(async move {
            tracing::info!("Room events forwarder started");
            while let Some(event) = room_events_rx.recv().await {
                tracing::info!("Forwarding room event: {:?}", event);
                if let LiveKitRoomEvent::MembershipChanged { generation } = event {
                    // Policy control bypasses the lossy broadcast/UI queues.
                    let _ = internal_tx.send(CallCommand::BeginPeerLeftCheck { generation }).await;
                    continue;
                }
                if let LiveKitRoomEvent::ConnectionLost { generation, ref reason, retryable } = event {
                    // Queue behind whatever the user already asked for: a
                    // hangup ahead of this clears the call and the handler
                    // finds nothing to recover.
                    let call = lost_call.borrow().as_ref().map(derive_call_handle);
                    if let Some(call) = call {
                        let _ = internal_tx.send(CallCommand::RoomLost {
                            call,
                            generation,
                            retryable,
                            reason: reason.clone(),
                        }).await;
                    }
                }
                // Fire-and-forget broadcast — no subscribers is fine.
                let _ = room_bus.send(event.clone());
                match events_tx_clone.try_send(event) {
                    Ok(()) => {},
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!("Room event UI queue full; keeping room-loss forwarding alive");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        // Core recovery and broadcast subscribers still need losses
                        // even when the optional UI receiver has been dropped.
                    }
                }
            }
            tracing::info!("Room events forwarder stopped");
        });

        // Recovery parks ordinary commands, but gives an admitted owner hangup
        // priority so it can use the normal teardown/response arm immediately.
        let mut deferred_commands = VecDeque::new();
        let mut camera_health = tokio::time::interval(std::time::Duration::from_millis(500));
        let mut audio_device_health = tokio::time::interval(std::time::Duration::from_secs(2));
        // Process commands
        loop {
            let deadline = orchestrator.incoming.values().map(|p| p.deadline).min();
            let cmd = if let Some(cmd) = deferred_commands.pop_front() {
                cmd
            } else {
                tokio::select! {
                    biased;
                    _ = audio_device_health.tick() => {
                        #[cfg(not(target_os = "android"))]
                        room_manager.refresh_audio_devices();
                        continue;
                    }
                    _ = camera_health.tick() => {
                        room_manager.recover_microphone_stream();
                        if room_manager.camera_capture_failed() {
                            room_manager.stop_camera().await;
                            if let Some(state) = active_call.borrow().as_ref() {
                                let _ = call_events_tx.send(CallOrchestratorEvent::CameraUnavailable {
                                    call: CallHandle::new(state.call.id.to_string()),
                                });
                            }
                        }
                        #[cfg(target_os = "android")]
                        if room_manager.screen_share_capture_failed() {
                            room_manager.stop_screen_share().await;
                        }
                        continue;
                    }
                    _ = async {
                        match deadline {
                            Some(deadline) => tokio::time::sleep_until(deadline).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        orchestrator.withdraw_expired(tokio::time::Instant::now(), &incoming_presentation, &call_events_tx);
                        continue;
                    }
                    cmd = cmd_rx.recv() => match cmd { Some(cmd) => cmd, None => break },
                }
            };

            // Membership decisions and owner checks happen together on the actor.
            // Recovery may defer these commands; generation prevents a stale timer
            // from ending a replacement room or call.
            let cmd = match cmd {
                CallCommand::BeginPeerLeftCheck { generation } => {
                    if orchestrator.active.as_ref().is_some_and(|a| incoming_presentation.accepts_session(a.session_revision))
                        && room_manager.connection_generation() == generation
                        && room_manager.membership_is_current()
                        && peer_left_ends_call(orchestrator.active.as_ref().map(|a| a.kind),
                            &room_manager.remote_participant_identities(), "")
                    {
                        if let Some(call) = active_call.borrow().as_ref().map(derive_call_handle) {
                            let generation = room_manager.connection_generation();
                            let session_revision = incoming_presentation.session_revision();
                            let membership_revision = room_manager.membership_revision();
                            let tx = command_sender.clone();
                            tokio::task::spawn_local(async move {
                                tokio::time::sleep(PEER_LEFT_GRACE).await;
                                let _ = tx.send(CallCommand::FinishPeerLeftCheck { call, generation, membership_revision, session_revision }).await;
                            });
                        }
                    }
                    continue;
                }
                CallCommand::FinishPeerLeftCheck { call, generation, membership_revision, session_revision } => {
                    if active_call.borrow().as_ref().map(derive_call_handle).as_ref() != Some(&call)
                        || incoming_presentation.session_revision() != session_revision
                        || !orchestrator.active.as_ref().is_some_and(|a| incoming_presentation.accepts_session(a.session_revision))
                        || room_manager.connection_generation() != generation
                        || room_manager.membership_revision() != membership_revision
                        || !room_manager.membership_is_current()
                        || !peer_left_ends_call(orchestrator.active.as_ref().map(|a| a.kind),
                            &room_manager.remote_participant_identities(), "")
                    { continue; }
                    let (response, _) = oneshot::channel();
                    CallCommand::DisconnectLocal { notify_server: false, session_revision,
                        call: Some(call), reason: CallEndReason::RemoteHangup, response }
                }
                cmd => cmd,
            };
            orchestrator.generation = orchestrator.generation.checked_add(1)
                .expect("call generation exhausted");
            match cmd {
                CallCommand::BeginPeerLeftCheck { .. } | CallCommand::FinishPeerLeftCheck { .. } => unreachable!("normalized above"),
                CallCommand::InitiateCall {
                    attempt,
                    channel_id,
                    title,
                    call_type,
                    encryption_root_key,
                    initiator_id,
                    response,
                } => {
                    let mut result = initiate_call_impl(
                        &call_client,
                        &room_manager,
                        &active_call,
                        attempt.as_ref(),
                        channel_id,
                        &title,
                        call_type,
                        &encryption_root_key,
                        initiator_id,
                    )
                    .await;

                    // Start key ratchet timer + auto-publish media on successful
                    // call initiation. Media publish was previously driven by UI
                    // glue — folding it into the worker keeps all call
                    // lifecycle side-effects in one place.
                    if result.is_ok() && !attempt.as_ref().is_some_and(|a| a.is_cancelled()) {
                        start_key_ratchet_timer(&room_manager, &ratchet_cancel, &active_call);
                        // the DM caller connects under an all-zero
                        // placeholder key — its real ECDH key only lands later
                        // in complete_dm_call. Publishing here would leak live
                        // mic/cam to the SFU under a fully server-derivable key.
                        // So defer the DM-caller publish until the real key is
                        // installed (complete_dm_call publishes right after
                        // set_encryption_key). Group/join connect with a real
                        // random key, so they still publish immediately.
                        //
                        // A group call the server says someone else started is
                        // one we joined (`CreateCall` returns a channel's live
                        // call): we hold no key of the call's yet, so publishing
                        // waits behind the gate like any joiner's.
                        let gated = active_call.borrow().as_ref().is_some_and(|s| s.publish_gate.is_closed());
                        if encryption_root_key != [0u8; 32] && !gated {
                            if auto_publish_media(&room_manager, matches!(call_type, CallType::Video)).await {
                                if let Some(state) = active_call.borrow().as_ref() {
                                    let _ = call_events_tx.send(CallOrchestratorEvent::CameraUnavailable {
                                        call: CallHandle::new(state.call.id.to_string()),
                                    });
                                }
                            }
                        }
                    }

                    if result.is_ok() && attempt.as_ref().is_some_and(|a| a.is_cancelled()) {
                        stop_key_ratchet_timer(&ratchet_cancel);
                        let _ = leave_call_impl(&call_client, &room_manager, &active_call).await;
                        result = Err(CallError::CallEnded);
                    }
                    let _ = response.send(result);
                }

                CallCommand::JoinCall {
                    session_revision,
                    call_id,
                    encryption_root_key,
                    joiner_id,
                    response,
                } => {
                    if session_revision.is_some() && !incoming_presentation.accepts_session(session_revision) {
                        let _ = response.send(Err(CallError::WorkerShutdown));
                        continue;
                    }
                    tracing::info!(stage = "join_call_handler_start", %call_id, elapsed_us = 0u64, "Call stage");
                    let result = join_call_impl(
                        &call_client,
                        &room_manager,
                        &active_call,
                        call_id,
                        encryption_root_key,
                        joiner_id,
                    )
                    .await;

                    if session_revision.is_some() && !incoming_presentation.accepts_session(session_revision) {
                        room_manager.disconnect().await;
                        *active_call.borrow_mut() = None;
                        let _ = response.send(Err(CallError::WorkerShutdown));
                        continue;
                    }
                    if result.is_ok() {
                        start_key_ratchet_timer(&room_manager, &ratchet_cancel, &active_call);
                        let is_video = result
                            .as_ref()
                            .ok()
                            .map(|c| matches!(c.call_type, CallType::Video))
                            .unwrap_or(false);
                        // A group joiner publishes once it holds the call's key.
                        if encryption_root_key.is_some() {
                            if auto_publish_media(&room_manager, is_video).await {
                                if let Some(state) = active_call.borrow().as_ref() {
                                    let _ = call_events_tx.send(CallOrchestratorEvent::CameraUnavailable {
                                        call: CallHandle::new(state.call.id.to_string()),
                                    });
                                }
                            }
                        }
                    }

                    let _ = response.send(result);
                }

                CallCommand::LeaveCall { reason, response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    // Capture handle before leave clears active_call
                    let handle = active_call.borrow().as_ref().map(derive_call_handle);

                    let result = leave_call_impl(&call_client, &room_manager, &active_call).await;

                    // Atomic orchestrator cleanup (previously required
                    // separate orch_mark_ended from callers)
                    if let Some(h) = handle {
                        if orchestrator.active.as_ref().map(|a| a.call == h).unwrap_or(false) {
                            orchestrator.active = None;
                        }
                        orchestrator.outgoing.remove(&h);
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call: h,
                            reason,
                        });
                    }

                    let _ = response.send(result);
                }

                CallCommand::EndCall { reason, response, session_revision } => {
                    if session_revision.is_some() && !incoming_presentation.accepts_session(session_revision) { let _ = response.send(Err(CallError::WorkerShutdown)); continue; }
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    let handle = active_call.borrow().as_ref().map(derive_call_handle);

                    let result =
                        end_call_impl(&call_client, &room_manager, &active_call, reason.as_deref())
                            .await;

                    if let Some(h) = handle {
                        if orchestrator.active.as_ref().map(|a| a.call == h).unwrap_or(false) {
                            orchestrator.active = None;
                        }
                        orchestrator.outgoing.remove(&h);
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call: h,
                            reason: CallEndReason::LocalHangup,
                        });
                    }

                    let _ = response.send(result);
                }

                CallCommand::DisconnectLocal { call, reason, response, session_revision, notify_server } => {
                    if session_revision.is_some() && !incoming_presentation.accepts_session(session_revision) { let _ = response.send(()); continue; }
                    let handle = active_call.borrow().as_ref().map(derive_call_handle);
                    if call.is_some() && call != handle {
                        tracing::info!(
                            call = ?call.as_ref().map(CallHandle::as_str),
                            active = ?handle.as_ref().map(CallHandle::as_str),
                            "Hangup for a call that is not the active one; ignored"
                        );
                        let _ = response.send(());
                        continue;
                    }

                    if notify_server {
                        let id = active_call.borrow().as_ref().map(|s| s.call.id.to_string());
                        if let Some(id) = id {
                            let mut client = call_client.lock().await;
                            let _ = tokio::time::timeout(std::time::Duration::from_secs(3), client.leave_call(&id)).await;
                        }
                    }

                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    room_manager.disconnect().await;
                    *active_call.borrow_mut() = None;
                    tracing::info!("Disconnected locally from call");

                    if let Some(h) = handle {
                        if orchestrator.active.as_ref().map(|a| a.call == h).unwrap_or(false) {
                            orchestrator.active = None;
                        }
                        orchestrator.outgoing.remove(&h);
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call: h,
                            reason,
                        });
                    }

                    let _ = response.send(());
                }

                CallCommand::SwapCallClient { client } => {
                    // Safe by construction: this arm only runs between complete
                    // call operations (the loop awaits each fully), so the
                    // signaling client's mutex is free and no RPC is in flight.
                    // Any active LiveKit media session lives in `room_manager`,
                    // independent of this signaling client, so an ongoing call
                    // is not disturbed — only the NEXT signaling RPC (e.g. the
                    // eventual leave/end) egresses over the new transport.
                    *call_client.lock().await = client;
                    tracing::info!("call worker: swapped gRPC call client for new transport");
                }

                CallCommand::AuthenticateDmHangup { session_revision, call, peer, own_elsewhere, response } => {
                    let session_lease = incoming_presentation.snapshot.read();
                    if session_revision.is_none() || session_lease.session_revision != session_revision {
                        let _ = response.send(SignalHangupDisposition::Reject);
                        continue;
                    }
                    let active = orchestrator.active.as_ref().filter(|a| a.call == call);
                    let incoming = orchestrator.incoming.get(&call);
                    let outgoing = orchestrator.outgoing.get(&call);
                    if !own_elsewhere && active.is_none() && incoming.is_some_and(|p| {
                        p.kind == OrchestratedCallKind::GroupChannel && p.peer_id == peer
                            && p.session_revision == session_revision
                    }) {
                        // Consume the pending ring on this actor turn. Emitting a
                        // generic hangup later could race acceptance and disconnect
                        // media for an already accepted group call.
                        if orchestrator.incoming.remove(&call).is_some() {
                            let _ = call_events_tx.send(CallOrchestratorEvent::IncomingWithdrawn {
                                call, reason: CallEndReason::RemoteHangup,
                            });
                        }
                        drop(session_lease);
                        incoming_presentation.publish(&orchestrator);
                        let _ = response.send(SignalHangupDisposition::GroupRingWithdrawn(session_revision.unwrap()));
                        continue;
                    }
                    let valid = if own_elsewhere {
                        // Another of our devices may only withdraw a ring,
                        // never tear down media this device has accepted.
                        active.is_none() && incoming.is_some_and(|p| p.kind == OrchestratedCallKind::DmStandard)
                    } else {
                        active.is_some_and(|a| a.kind == OrchestratedCallKind::DmStandard && a.peer_id == Some(peer))
                            || incoming.is_some_and(|p| p.kind == OrchestratedCallKind::DmStandard && p.peer_id == peer)
                            || outgoing.is_some_and(|p| p.kind == OrchestratedCallKind::DmStandard && p.peer_id == peer)
                    };
                    let _ = response.send(if valid { SignalHangupDisposition::Forward } else { SignalHangupDisposition::Reject });
                }

                CallCommand::GetActiveCall { response } => {
                    let call = active_call.borrow().as_ref().map(|a| a.call.clone());
                    let _ = response.send(call);
                }

                CallCommand::GetVerificationSnapshot { response } => {
                    let snapshot = call_verification_snapshot(
                        active_call.borrow().as_ref(),
                        &orchestrator,
                        room_manager.is_connected(),
                        room_manager.key_epoch(),
                    );
                    let _ = response.send(snapshot);
                }

                CallCommand::IsInCall { response } => {
                    let _ = response.send(active_call.borrow().is_some());
                }

                CallCommand::GetRoomConnection { response } => {
                    let _ = response.send((room_manager.connection_generation(), room_manager.state()));
                }

                CallCommand::GetRoomState { response } => {
                    let _ = response.send(room_manager.state());
                }

                CallCommand::GetRemoteParticipants { response } => {
                    let _ = response.send(room_manager.remote_participant_identities());
                }

                CallCommand::GetRoomSeniority { response } => {
                    let seniority = room_manager.room_seniority().map(|(local_joined_at_ms, remote)| {
                        RoomSeniority { local_joined_at_ms, remote }
                    });
                    let _ = response.send(seniority);
                }

                #[cfg(feature = "testing")]
                CallCommand::GetLocalTrackCount { response } => {
                    let _ = response.send(room_manager.local_track_count());
                }

                CallCommand::GetGroupKeyStatus { response } => {
                    let status = active_call.borrow().as_ref().map(|s| GroupKeyStatus {
                        call_id: s.call.id,
                        anchor: s.key_state.anchor(),
                        awaiting_key: s.publish_gate.is_closed(),
                    });
                    let _ = response.send(status);
                }

                CallCommand::PublishMicrophone { device_id, response, answer_scope } => {
                    if answer_scope.as_ref().is_some_and(|scope| !scope.accepts(&incoming_presentation, &active_call)) {
                        let _ = response.send(Err(CallError::CallEnded));
                        continue;
                    }
                    #[cfg(test)]
                    if answer_scope.is_some() && answer_backend.is_some() {
                        answer_effects.1 += 1;
                        if let Some((entered, resume)) = answer_backend.as_mut().unwrap().take() {
                            let _ = entered.send(());
                            let _ = resume.await;
                        }
                        let _ = response.send(Ok(()));
                        continue;
                    }
                    let result = if gate_defers(&active_call, |g| g.defer_mic(device_id.clone())) {
                        tracing::info!("Microphone held back until the call's key arrives");
                        Ok(())
                    } else {
                        room_manager.publish_microphone(device_id.as_deref()).await
                    };
                    let _ = response.send(result);
                }

                CallCommand::PublishCamera {
                    answer_scope,
                    device_path,
                    config,
                    response,
                } => {
                    if answer_scope.as_ref().is_some_and(|scope| !scope.accepts(&incoming_presentation, &active_call)) {
                        let _ = response.send(Err(CallError::CallEnded));
                        continue;
                    }
                    if answer_scope.is_some() {
                        // Drain named-answer publication before processing End/replacement.
                        // Ownership is rechecked by completion before any next mutation.
                        let result = room_manager.publish_camera(device_path.as_deref(), config).await;
                        let _ = response.send(result);
                        continue;
                    }
                    if gate_defers(&active_call, PublishGate::defer_camera) {
                        tracing::info!("Camera held back until the call's key arrives");
                        let _ = response.send(Ok(()));
                        continue;
                    }
                    // Spawn — never await a capture publish inside the command
                    // loop (see PublishScreenShare below for the full rationale).
                    let rm = room_manager.clone();
                    tokio::task::spawn_local(async move {
                        let result = rm.publish_camera(device_path.as_deref(), config).await;
                        let _ = response.send(result);
                    });
                }

                CallCommand::StopMicrophone => {
                    gate_defers(&active_call, |g| { g.drop_mic(); false });
                    room_manager.stop_microphone().await;
                }

                CallCommand::StopCamera => {
                    gate_defers(&active_call, |g| { g.drop_camera(); false });
                    room_manager.stop_camera().await;
                }

                CallCommand::SetMicrophoneMuted { muted } => {
                    room_manager.set_microphone_muted(muted);
                }

                CallCommand::IsMicrophoneMuted { response } => {
                    let _ = response.send(room_manager.is_microphone_muted());
                }

                CallCommand::IsCameraEnabled { response } => {
                    let _ = response.send(room_manager.is_camera_enabled());
                }

                CallCommand::SetPeerAudioMuted { participant_id, muted } => {
                    room_manager.set_peer_audio_muted(&participant_id, muted);
                }

                #[cfg(not(target_os = "android"))]
                CallCommand::AudioDeviceSnapshot { output, response } => {
                    let _ = response.send(room_manager.audio_device_snapshot(output).map_err(|e| e.to_string()));
                }
                CallCommand::SetAudioOutput { device, response } => {
                    let _ = response.send(room_manager.set_audio_output(&device).map_err(|e| e.to_string()));
                }

                CallCommand::SetPeerVideoPaused { participant_id, paused } => {
                    room_manager.set_peer_video_paused(&participant_id, paused);
                }

                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                CallCommand::PublishScreenShare { source, response } => {
                    if gate_defers(&active_call, |g| g.is_closed()) {
                        let _ = response.send(Err(CallError::AwaitingKey));
                        continue;
                    }
                    // CRITICAL: spawn, don't await here. publish_screen_share runs
                    // the desktop-capture handshake, which on Wayland waits on the
                    // XDG portal and can stall for tens of seconds (flaky portals)
                    // or until the user picks a source. Awaiting it inside the
                    // sequential command loop wedges the ENTIRE worker: GetRoomState
                    // and every other command block, so call_is_connected() hangs
                    // and all call UI buttons (share, video, …) go dead with no
                    // feedback. Spawning keeps the loop live; the oneshot still
                    // carries the result back when the publish finishes or fails.
                    let rm = room_manager.clone();
                    tokio::task::spawn_local(async move {
                        let result = rm.publish_screen_share(source, None).await;
                        let _ = response.send(result);
                    });
                }

                #[cfg(target_os = "android")]
                CallCommand::PublishAndroidScreenShare { response } => {
                    if gate_defers(&active_call, |g| g.is_closed()) {
                        let _ = response.send(Err(CallError::AwaitingKey));
                        continue;
                    }
                    let rm = room_manager.clone();
                    tokio::task::spawn_local(async move {
                        let _ = response.send(rm.publish_screen_share().await);
                    });
                }

                #[cfg(not(target_os = "ios"))]
                CallCommand::StopScreenShare => {
                    room_manager.stop_screen_share().await;
                }

                CallCommand::IsScreenSharing { response } => {
                    let _ = response.send(room_manager.is_screen_sharing());
                }

                CallCommand::RotateKey { new_key, response } => {
                    let result = rotate_group_media_key(&active_call, &room_manager, Some(&call_events_tx), new_key);
                    let _ = response.send(result);
                }

                CallCommand::InstallGroupKey { call_id, key, epoch, response } => {
                    let result = install_group_media_key(&active_call, &room_manager, Some(&call_events_tx), call_id, key, epoch);
                    let _ = response.send(result);
                }

                CallCommand::RatchetKey { response } => {
                    let epoch = room_manager.ratchet_key();
                    let _ = response.send(epoch);
                }

                CallCommand::SetEncryptionKey { key, response, answer_scope } => {
                    if answer_scope.as_ref().is_some_and(|scope| !scope.accepts(&incoming_presentation, &active_call)) {
                        let _ = response.send(false);
                        continue;
                    }
                    #[cfg(test)]
                    if answer_scope.is_some() && answer_backend.is_some() {
                        let mut state = active_call.borrow_mut();
                        let state = state.as_mut().unwrap();
                        state.key_state = CallKeyState::new(&<[u8; 32]>::try_from(key.as_slice()).unwrap(), state.key_state.call_id()).unwrap();
                        answer_effects.0 += 1;
                        let _ = response.send(true);
                        continue;
                    }
                    let success = reset_dm_media_key(&active_call, &room_manager, key);
                    let _ = response.send(success);
                }

                CallCommand::GetKeyEpoch { response } => {
                    let epoch = room_manager.key_epoch();
                    let _ = response.send(epoch);
                }

                #[cfg(feature = "testing")]
                CallCommand::GetInstalledKeyRing { response } => {
                    let _ = response.send(room_manager.installed_key_ring());
                }

                CallCommand::Reconnect { response } => {
                    // A live or SDK-recovering session is never joined twice.
                    if room_manager.is_connected()
                        || matches!(room_manager.state(), RoomState::Connecting)
                    {
                        let _ = response.send(Err(CallError::AlreadyInCall));
                        continue;
                    }
                    let call = active_call.borrow().as_ref().map(derive_call_handle);
                    let Some(call) = call else {
                        let _ = response.send(Err(CallError::NotInCall));
                        continue;
                    };
                    let _ = events_tx.try_send(LiveKitRoomEvent::RecoveryPhase { generation: room_manager.connection_generation(), recovered: false });
                    let result = recover_session(
                        &call_client, &room_manager, &active_call, &ratchet_cancel,
                        &mut cmd_rx, &mut deferred_commands, &incoming_presentation, &call,
                        orchestrator.active.as_ref().filter(|active| active.call == call)
                            .map(|active| active.session_revision),
                    ).await;
                    if result.is_ok() {
                        // Fresh connect emits no departure for an already-empty
                        // roster. Reconcile on the actor using its normal scoped grace.
                        deferred_commands.push_back(CallCommand::BeginPeerLeftCheck {
                            generation: room_manager.connection_generation(),
                        });
                        let _ = events_tx.try_send(LiveKitRoomEvent::RecoveryPhase { generation: room_manager.connection_generation(), recovered: true });
                    }
                    if matches!(&result, Err(RecoveryFailure::Failed(_))) {
                        finish_failed_recovery(
                            &call_client, &room_manager, &active_call, &ratchet_cancel,
                            &mut orchestrator, &call_events_tx, call,
                        ).await;
                    }
                    let result = result.map_err(|failure| match failure {
                        RecoveryFailure::Cancelled => CallError::CallEnded,
                        RecoveryFailure::Failed(error) => error,
                    });
                    let _ = response.send(result);
                }

                CallCommand::RoomLost { call, generation, retryable, reason } => {
                    let same_call = || active_call.borrow().as_ref().map(derive_call_handle).as_ref() == Some(&call);
                    // Stale notice: the user ended the call, a newer call
                    // owns the room manager, or a newer connect is under way.
                    if !same_call()
                        || room_manager.connection_generation() != generation
                        || !matches!(room_manager.state(), RoomState::Failed { .. })
                    {
                        tracing::info!(call = %call.as_str(), generation, "Stale room-loss notice ignored");
                        continue;
                    }
                    // A session that outlived its presentation session (account
                    // switch) is torn down, never rejoined.
                    let session_ok = orchestrator
                        .active
                        .as_ref()
                        .filter(|a| a.call == call)
                        .is_none_or(|a| incoming_presentation.accepts_session(a.session_revision));
                    tracing::warn!(call = %call.as_str(), %reason, retryable, "Room session lost");
                    if retryable && session_ok {
                        let _ = events_tx.try_send(LiveKitRoomEvent::RecoveryPhase { generation, recovered: false });
                    }
                    let recovery = if retryable && session_ok {
                        recover_session(
                            &call_client, &room_manager, &active_call, &ratchet_cancel,
                            &mut cmd_rx, &mut deferred_commands, &incoming_presentation, &call,
                            orchestrator.active.as_ref().filter(|active| active.call == call)
                                .map(|active| active.session_revision),
                        ).await
                    } else {
                        Err(RecoveryFailure::Failed(CallError::CallEnded))
                    };
                    match recovery {
                        Ok(()) => {
                            tracing::info!(call = %call.as_str(), "Room session recovered");
                            deferred_commands.push_back(CallCommand::BeginPeerLeftCheck {
                                generation: room_manager.connection_generation(),
                            });
                            let _ = events_tx.try_send(LiveKitRoomEvent::RecoveryPhase { generation: room_manager.connection_generation(), recovered: true });
                        },
                        // The admitted hangup is next, preserving its reason, server
                        // notification policy and reply through the existing arm.
                        Err(RecoveryFailure::Cancelled) => {},
                        Err(RecoveryFailure::Failed(error)) => {
                            tracing::warn!(call = %call.as_str(), %error, "Fresh-token recovery ended");
                            if same_call() {
                                finish_failed_recovery(
                                    &call_client, &room_manager, &active_call, &ratchet_cancel,
                                    &mut orchestrator, &call_events_tx, call,
                                ).await;
                            }
                        }
                    }
                }

                // ============================================================
                // Orchestrator commands
                // ============================================================

                #[cfg(test)]
                CallCommand::TestAdvanceIncomingClock { elapsed, response } => {
                    tokio::time::pause();
                    tokio::time::advance(elapsed).await;
                    let _ = response.send(());
                }
                #[cfg(test)]
                CallCommand::TestExpireIncoming { elapsed, response } => {
                    orchestrator.withdraw_expired(tokio::time::Instant::now() + elapsed, &incoming_presentation, &call_events_tx);
                    let _ = response.send(());
                }
                CallCommand::OrchPresentIncoming { incoming } => {
                    if !incoming_presentation.accepts_session(incoming.session_revision) {
                        continue;
                    }
                    // A group ring whose lookup raced our own join of that
                    // call: we are in it, so there is nothing to ring for, and
                    // a slot left behind would be withdrawn mid-call.
                    if incoming.kind == OrchestratedCallKind::GroupChannel
                        && orchestrator.active.as_ref().is_some_and(|a| a.call == incoming.call)
                    {
                        continue;
                    }
                    if orchestrator.incoming.get(&incoming.call).is_some_and(|old| old.session_revision != incoming.session_revision) {
                        orchestrator.incoming.remove(&incoming.call);
                    }
                    if orchestrator.incoming.contains_key(&incoming.call) {
                        continue; // never renew or re-announce an existing ring
                    }
                    let replaces_active = orchestrator.active.is_some();
                    let snapshot = incoming.clone();
                    orchestrator.incoming_order.insert(
                        incoming.call.clone(), orchestrator.generation,
                    );
                    orchestrator.incoming.insert(incoming.call.clone(), incoming);
                    let _ = call_events_tx.send(CallOrchestratorEvent::IncomingPresented {
                        call: snapshot.call,
                        kind: snapshot.kind,
                        peer_id: snapshot.peer_id,
                        peer_name: snapshot.peer_name,
                        is_video: snapshot.is_video,
                        is_identity_verified: snapshot.is_identity_verified,
                        identity_warning: snapshot.identity_warning,
                        ephemeral_public_key: snapshot.ephemeral_public_key,
                        channel_context: snapshot.channel_context,
                        conversation_id: snapshot.conversation_id,
                        replaces_active,
                    });
                }

                CallCommand::OrchWithdrawIncoming { session_revision, call, reason, response } => {
                    // Queue backpressure may span logout/login. Fence the mutation
                    // under the session lease, including a reused call handle.
                    let session_lease = incoming_presentation.snapshot.read();
                    if session_revision.is_some()
                        && (session_lease.session_revision != session_revision
                            || orchestrator.incoming.get(&call).is_some_and(|pending| {
                                pending.session_revision != session_revision
                            }))
                    {
                        if let Some(response) = response {
                            let _ = response.send(());
                        }
                        continue;
                    }
                    if orchestrator.incoming.remove(&call).is_some() {
                        let _ = call_events_tx.send(CallOrchestratorEvent::IncomingWithdrawn {
                            call,
                            reason,
                        });
                    } else {
                        tracing::debug!(
                            call = %call.as_str(),
                            "OrchWithdrawIncoming: no matching pending incoming"
                        );
                    }
                    drop(session_lease);
                    incoming_presentation.publish(&orchestrator);
                    if let Some(response) = response {
                        let _ = response.send(());
                    }
                }

                CallCommand::OrchRegisterOutgoing { outgoing, session_revision } => {
                    let session_lease = incoming_presentation.snapshot.read();
                    if session_revision.is_some() && session_lease.session_revision != session_revision { continue; }
                    if outgoing.session_revision.is_some() && outgoing.session_revision != session_lease.session_revision { continue; }
                    let snapshot = outgoing.clone();
                    orchestrator.outgoing.insert(outgoing.call.clone(), outgoing);
                    let _ = call_events_tx.send(CallOrchestratorEvent::OutgoingStarted {
                        call: snapshot.call,
                        kind: snapshot.kind,
                        peer_id: snapshot.peer_id,
                        peer_name: snapshot.peer_name,
                        is_video: snapshot.is_video,
                    });
                }

                CallCommand::OrchMarkConnected { call, kind, peer_id, is_video, session_revision, response, require_media_match, answer_scope } => {
                    if answer_scope.as_ref().is_some_and(|scope| !scope.accepts(&incoming_presentation, &active_call)) {
                        if let Some(response) = response { let _ = response.send(Err(CallError::CallEnded)); }
                        continue;
                    }
                    let session_lease = incoming_presentation.snapshot.read();
                    if session_revision.is_some() && session_lease.session_revision != session_revision { if let Some(response) = response { let _ = response.send(Err(CallError::WorkerShutdown)); } continue; }
                    if require_media_match && active_call.borrow().as_ref().map(derive_call_handle).as_ref() != Some(&call) {
                        if let Some(response) = response { let _ = response.send(Err(CallError::CallEnded)); }
                        continue;
                    }
                    if let Some(state) = active_call.borrow_mut().as_mut() {
                        if derive_call_handle(state) == call && state.call.connected_at.is_none() {
                            state.call.start();
                        }
                    }
                    // Clear from pending maps regardless of whether it was
                    // incoming or outgoing — either way it's now connected.
                    orchestrator.outgoing.remove(&call);
                    orchestrator.incoming.remove(&call);
                    orchestrator.active = Some(OrchestratedActive {
                        peer_id: Some(peer_id),
                        session_revision: session_revision.or(session_lease.session_revision),
                        call: call.clone(),
                        kind,
                    });
                    let _ = call_events_tx.send(CallOrchestratorEvent::ActiveConnected {
                        call,
                        kind,
                        peer_id,
                        is_video,
                    });
                    drop(session_lease);
                    incoming_presentation.publish(&orchestrator);
                    if let Some(response) = response { let _ = response.send(Ok(())); }
                }

                CallCommand::OrchMarkEnded { call, reason, session_revision } => {
                    let session_lease = incoming_presentation.snapshot.read();
                    if session_revision.is_some() && session_lease.session_revision != session_revision { continue; }
                    let matched_active = orchestrator
                        .active
                        .as_ref()
                        .map(|a| a.call == call)
                        .unwrap_or(false);
                    if matched_active {
                        orchestrator.active = None;
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call,
                            reason,
                        });
                    } else if orchestrator.outgoing.remove(&call).is_some() {
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call,
                            reason,
                        });
                    } else if orchestrator.incoming.remove(&call).is_some() {
                        let _ = call_events_tx.send(CallOrchestratorEvent::IncomingWithdrawn {
                            call,
                            reason,
                        });
                    } else {
                        // Not in any slot — an outgoing call, which nothing
                        // registers. The caller is still telling us the call
                        // is over, and swallowing that left the "Calling…"
                        // view up after a peer declined and left call history
                        // with a row that never closed. Announce it anyway;
                        // subscribers key off the handle, not the slot.
                        tracing::debug!(
                            call = %call.as_str(),
                            "OrchMarkEnded: untracked call — forwarding terminal event"
                        );
                        let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                            call,
                            reason,
                        });
                    }
                }

                #[cfg(test)]
                CallCommand::TestDmAnswerBackend { publish, response } => {
                    answer_backend = Some(publish);
                    let _ = response.send(());
                }
                #[cfg(test)]
                CallCommand::TestDmAnswerEffects { response } => { let _ = response.send(answer_effects); }
                #[cfg(test)]
                CallCommand::TestPrimeCall { call, response } => {
                    let handle = CallHandle::from(call.id.to_string().as_str());
                    let key_state = CallKeyState::new(&[8; 32], handle.as_str()).unwrap();
                    *active_call.borrow_mut() = Some(ActiveCallState {
                        reconnect_media: None,
                        fresh_rejoin_used: false,
                        call, key_state,
                        livekit_url: String::new(),
                        publish_gate: PublishGate::default(),
                    });
                    orchestrator.active = Some(OrchestratedActive {
                        peer_id: None,
                        session_revision: None, call: handle, kind: OrchestratedCallKind::DmStandard,
                    });
                    let _ = response.send(());
                }
                #[cfg(test)]
                CallCommand::TestPause { entered, resume } => {
                    let _ = entered.send(());
                    let _ = resume.await;
                }
                CallCommand::OrchAcceptPresented { call, replaces_active, invites, response } => {
                    let mut invites = invites.lock();
                    let is_group = orchestrator.incoming.get(&call)
                        .is_some_and(|p| p.kind == OrchestratedCallKind::GroupChannel);
                    let result = if is_group && !invites.get(call.as_str()).is_some_and(|i| !i.accepted) {
                        Err(CallError::NotInCall)
                    } else if !replaces_active && orchestrator.active.is_some() {
                        Err(CallError::AlreadyInCall)
                    } else if let Some(p) = orchestrator.incoming.remove(&call) {
                        // No await or reply can expose worker success before key ownership.
                        if is_group {
                            invites.get_mut(call.as_str()).unwrap().accepted = true;
                        }
                        if replaces_active {
                            if let Some(prev) = orchestrator.active.take() {
                                let _ = call_events_tx.send(CallOrchestratorEvent::ActiveEnded {
                                    call: prev.call, reason: CallEndReason::ReplacedByIncoming,
                                });
                            }
                        }
                        Ok(p)
                    } else {
                        Err(CallError::NotInCall)
                    };
                    incoming_presentation.publish(&orchestrator);
                    let _ = response.send(result);
                }

                CallCommand::OrchAcceptIncoming { call, response } => {
                    // Fail if an active call is holding the slot — UI must
                    // explicitly send the replacing variant so the user has
                    // chosen to end their current call.
                    let result = if orchestrator.active.is_some() {
                        Err(CallError::AlreadyInCall)
                    } else if let Some(p) = orchestrator.incoming.remove(&call) {
                        Ok(p)
                    } else {
                        Err(CallError::NotInCall)
                    };
                    incoming_presentation.publish(&orchestrator);
                    let _ = response.send(result);
                }

                CallCommand::OrchAcceptIncomingReplacingActive { call, response } => {
                    let result = match orchestrator.incoming.remove(&call) {
                        Some(p) => {
                            if let Some(prev) = orchestrator.active.take() {
                                let _ = call_events_tx.send(
                                    CallOrchestratorEvent::ActiveEnded {
                                        call: prev.call,
                                        reason: CallEndReason::ReplacedByIncoming,
                                    },
                                );
                            }
                            Ok(p)
                        }
                        None => Err(CallError::NotInCall),
                    };
                    incoming_presentation.publish(&orchestrator);
                    let _ = response.send(result);
                }

                CallCommand::OrchDeclineIncoming { call, response } => {
                    let result = match orchestrator.incoming.remove(&call) {
                        Some(p) => {
                            let _ = call_events_tx.send(
                                CallOrchestratorEvent::IncomingWithdrawn {
                                    call: p.call.clone(),
                                    reason: CallEndReason::Declined,
                                },
                            );
                            Ok(p)
                        }
                        None => Err(CallError::NotInCall),
                    };
                    incoming_presentation.publish(&orchestrator);
                    let _ = response.send(result);
                }

                CallCommand::OrchActiveKind { response } => {
                    let _ = response.send(orchestrator.call_kind());
                }

                CallCommand::ResetPresentationSession { revision, response } => {
                    orchestrator.outgoing.retain(|_, pending| pending.session_revision != Some(revision));
                    let retired: Vec<_> = orchestrator.incoming.iter()
                        .filter(|(_, p)| p.session_revision == Some(revision))
                        .map(|(id, _)| id.clone()).collect();
                    let retired_ids = retired.iter().map(|call| call.as_str().to_owned()).collect();
                    for call in retired {
                        orchestrator.incoming.remove(&call);
                        let _ = call_events_tx.send(CallOrchestratorEvent::IncomingWithdrawn {
                            call, reason: CallEndReason::LocalHangup,
                        });
                    }
                    incoming_presentation.publish(&orchestrator);
                    let _ = response.send(retired_ids);
                }
                CallCommand::OrchIncomingSnapshot { response } => {
                    // Prune withdrawn identities; ordering metadata contains no keys.
                    orchestrator.incoming_order.retain(
                        |call, _| orchestrator.incoming.contains_key(call),
                    );
                    let _ = response.send(orchestrator.incoming_snapshot_for(incoming_presentation.session_revision()));
                }

                CallCommand::Shutdown => {
                    tracing::info!("Call worker shutting down");
                    break;
                }
            }
            orchestrator.incoming_order.retain(|call, _| orchestrator.incoming.contains_key(call));
            incoming_presentation.publish(&orchestrator);
        }

        tracing::info!("Call worker stopped");
    });
}

