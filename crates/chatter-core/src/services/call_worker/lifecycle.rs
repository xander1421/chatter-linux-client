use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn initiate_call_impl(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    attempt: Option<&crate::client::calls::CallStartAttempt>,
    channel_id: ChannelId,
    title: &str,
    call_type: CallType,
    encryption_root_key: &[u8; 32],
    initiator_id: UserId,
) -> Result<Call, CallError> {
    if attempt.is_some_and(|a| a.is_cancelled()) {
        return Err(CallError::CallEnded);
    }
    // Check if already in a call
    if active_call.borrow().is_some() {
        return Err(CallError::AlreadyInCall);
    }

    // Convert call type to proto
    let proto_type = match call_type {
        CallType::Voice => ProtoCallType::Voice,
        CallType::Video => ProtoCallType::Video,
        CallType::Group => ProtoCallType::Video,
    };

    // Create call on server
    let response = {
        let mut client = call_client.lock().await;
        client
            .create_call(&channel_id.to_string(), title, proto_type, None)
            .await
            .map_err(|e| CallError::ServerError(e.to_string()))?
    };

    // Once the server returns an identity, all subsequent errors/cancellation
    // have enough information to undo membership and partial media setup.
    let prepared = async {
        if attempt.is_some_and(|a| a.is_cancelled()) {
            return Err(CallError::CallEnded);
        }
        let call_id = CallId::from_str(&response.call_id).map_err(|_| CallError::InvalidCallId)?;
        let key_state = CallKeyState::new(encryption_root_key, &response.call_id)
            .map_err(|e| CallError::CryptoError(e.to_string()))?;
        room_manager
            .connect(
                &response.livekit_url,
                &response.livekit_token,
                key_state.current_key_vec(),
            )
            .await?;
        if attempt.is_some_and(|a| a.is_cancelled()) {
            return Err(CallError::CallEnded);
        }
        Ok((call_id, key_state))
    }
    .await;
    let (call_id, key_state) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            room_manager.disconnect().await;
            let mut client = call_client.lock().await;
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                client.leave_call(&response.call_id),
            )
            .await;
            return Err(error);
        }
    };

    // `CreateCall` hands back a group channel's live call when there is one:
    // then its creator is someone else and we have joined it, holding none of
    // its keys (the key we connected with is ours alone).
    let creator = response
        .call_info
        .as_ref()
        .and_then(|ci| UserId::from_str(&ci.created_by).ok())
        .unwrap_or(initiator_id);
    let joined_existing = creator != initiator_id && encryption_root_key != &[0u8; 32];
    let mut call = Call::new(channel_id, creator, call_type);
    call.id = call_id;
    // The DM caller joins with a placeholder before the peer answers. Keep
    // connected_at unset until OrchMarkConnected; group calls connect now.
    if encryption_root_key == &[0u8; 32] {
        call.ring();
    } else {
        call.start();
    }
    // Retain server creation time for signaling, separate from connected_at.
    if let Some(dt) = response
        .call_info
        .as_ref()
        .and_then(|ci| server_ns_to_utc(ci.created_at))
    {
        call.started_at = Some(dt);
    }
    call.add_participant(Participant::new(initiator_id));

    // Store active call
    *active_call.borrow_mut() = Some(ActiveCallState {
        reconnect_media: None,
        fresh_rejoin_used: false,
        call: call.clone(),
        key_state,
        livekit_url: response.livekit_url,
        publish_gate: if joined_existing {
            PublishGate::awaiting_key(matches!(call_type, CallType::Video))
        } else {
            PublishGate::default()
        },
    });

    Ok(call)
}

/// Convert a server Unix-nanoseconds timestamp to `DateTime<Utc>`, or `None`
/// when the value is 0/out of range (caller then keeps the local `started_at`).
pub(super) fn server_ns_to_utc(ns: u64) -> Option<chrono::DateTime<chrono::Utc>> {
    if ns == 0 {
        return None;
    }
    chrono::DateTime::from_timestamp((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32)
}

pub(super) async fn join_call_impl(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    call_id: CallId,
    encryption_root_key: Option<[u8; 32]>,
    joiner_id: UserId,
) -> Result<Call, CallError> {
    // Check if already in a call
    if active_call.borrow().is_some() {
        return Err(CallError::AlreadyInCall);
    }

    let timing = crate::calls::timing::CallTiming::new("join_requested_worker");

    // Join call on server
    let response = {
        let mut client = call_client.lock().await;
        let rpc_started = std::time::Instant::now();
        tracing::info!(stage = "join_call_rpc_begin", %call_id, elapsed_us = 0u64, "Call stage");
        timing.mark("join_rpc_begin");
        let joined = client.join_call(&call_id.to_string()).await;
        let stage = if joined.is_ok() {
            "join_call_rpc_succeeded"
        } else {
            "join_call_rpc_failed"
        };
        tracing::info!(stage, %call_id, elapsed_us = rpc_started.elapsed().as_micros() as u64, "Call stage");
        timing.mark(stage);
        joined.map_err(|e| CallError::ServerError(e.to_string()))?
    };

    // Derive E2EE key. A group joiner has none of the call's: it connects
    // under a random key of its own, never sent, and publishes nothing until
    // the key authority's key is installed (`PublishGate`).
    let root = zeroize::Zeroizing::new(
        encryption_root_key.unwrap_or_else(crate::crypto::generate_group_call_key),
    );
    let key_state = CallKeyState::new(&root, &call_id.to_string())
        .map_err(|e| CallError::CryptoError(e.to_string()))?;

    // Connect to LiveKit room
    room_manager
        .connect_timed(
            &response.livekit_url,
            &response.livekit_token,
            key_state.current_key_vec(),
            timing.clone(),
        )
        .await?;

    // Convert proto to domain
    let call_info = response.call_info.ok_or(CallError::NotFound)?;
    let server_channel_id =
        ChannelId::from_str(&call_info.channel_id).map_err(|_| CallError::InvalidChannelId)?;
    let initiator_id =
        UserId::from_str(&call_info.created_by).map_err(|_| CallError::InvalidUserId)?;
    let channel_id = joined_channel_id(server_channel_id, &initiator_id, &joiner_id);

    let call_type =
        match ProtoCallType::try_from(call_info.r#type).unwrap_or(ProtoCallType::Unspecified) {
            ProtoCallType::Voice => CallType::Voice,
            ProtoCallType::Video | ProtoCallType::ScreenShare => CallType::Video,
            _ => CallType::Voice,
        };

    let mut call = Call::new(channel_id, initiator_id, call_type);
    call.id = call_id;
    call.start();
    // Share the server creation time for key-ratchet boundaries; duration
    // separately uses the local connected_at timestamp.
    if let Some(dt) = server_ns_to_utc(call_info.created_at) {
        call.started_at = Some(dt);
    }
    call.add_participant(Participant::new(joiner_id));

    // Store active call
    timing.mark(if encryption_root_key.is_none() {
        "call_key_waiting"
    } else {
        "call_key_installed"
    });
    let publish_gate = if encryption_root_key.is_none() {
        PublishGate::awaiting_key(matches!(call.call_type, CallType::Video))
    } else {
        PublishGate::default()
    };
    *active_call.borrow_mut() = Some(ActiveCallState {
        reconnect_media: None,
        fresh_rejoin_used: false,
        call: call.clone(),
        key_state,
        livekit_url: response.livekit_url,
        publish_gate,
    });

    Ok(call)
}

/// The conversation a joined call belongs to, from the joiner's side.
///
/// A channel-free 1:1 call is created with the caller's scoping id, which is
/// `ChannelId::from(callee)` — so the callee reads its own id back from the
/// server. Filed verbatim, that is the callee's Saved Messages; from here the
/// conversation is the DM with the caller. Group and channel-backed DM calls
/// carry a real channel id and pass through.
pub(super) fn joined_channel_id(
    server_channel_id: ChannelId,
    initiator_id: &UserId,
    joiner_id: &UserId,
) -> ChannelId {
    if server_channel_id == ChannelId::from(*joiner_id) {
        ChannelId::from(*initiator_id)
    } else {
        server_channel_id
    }
}

pub(super) async fn leave_call_impl(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
) -> Result<(), CallError> {
    let call_id = {
        let active = active_call.borrow();
        let state = active.as_ref().ok_or(CallError::NotInCall)?;
        state.call.id.to_string()
    };

    {
        let mut client = call_client.lock().await;
        match tokio::time::timeout(
            tokio::time::Duration::from_secs(3),
            client.leave_call(&call_id),
        )
        .await
        {
            Ok(Ok(())) => {
                tracing::debug!("Server acknowledged call leave");
            }
            Ok(Err(e)) => {
                tracing::warn!("Server leave_call error (continuing): {}", e);
            }
            Err(_) => {
                tracing::warn!("Server leave_call timed out (continuing with local disconnect)");
            }
        }
    }

    // Disconnect from LiveKit
    room_manager.disconnect().await;

    // Clear active call
    *active_call.borrow_mut() = None;

    Ok(())
}

pub(super) async fn end_call_impl(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    reason: Option<&str>,
) -> Result<(), CallError> {
    let call_id = {
        let active = active_call.borrow();
        active
            .as_ref()
            .map(|a| a.call.id.to_string())
            .ok_or(CallError::NotInCall)?
    };

    // Final local cleanup runs even if signaling fails or times out.
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let mut client = call_client.lock().await;
        client.end_call(&call_id, reason.unwrap_or(""))
            .await.map_err(|e| CallError::ServerError(e.to_string()))
    }).await.unwrap_or_else(|_| Err(CallError::ServerError("End call timed out".into())));
    room_manager.disconnect().await;
    *active_call.borrow_mut() = None;
    result
}

/// Recovery consumes the actual admitted command, rather than a separate
/// intent counter that can race admission or cancel the wrong call.
pub(super) enum RecoveryFailure {
    Cancelled,
    Failed(CallError),
}

pub(super) fn cancels_recovery(
    command: &CallCommand,
    call: &CallHandle,
    presentation: &IncomingCallPresentationState,
) -> bool {
    match command {
        CallCommand::LeaveCall { .. } => true,
        CallCommand::EndCall { session_revision, .. } => {
            session_revision.is_none() || presentation.accepts_session(*session_revision)
        }
        CallCommand::DisconnectLocal { call: target, session_revision, .. } => {
            (target.is_none() || target.as_ref() == Some(call))
                && (session_revision.is_none() || presentation.accepts_session(*session_revision))
        }
        _ => false,
    }
}

pub(super) async fn recover_session(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    ratchet_cancel: &Rc<RefCell<Option<oneshot::Sender<()>>>>,
    commands: &mut mpsc::Receiver<CallCommand>,
    deferred: &mut VecDeque<CallCommand>,
    presentation: &IncomingCallPresentationState,
    call: &CallHandle,
    owner_session: Option<Option<u64>>,
) -> Result<(), RecoveryFailure> {
    stop_key_ratchet_timer(ratchet_cancel);
    let session_current = || owner_session.is_none_or(|revision| presentation.accepts_session(revision));
    let mut session_changes = presentation.changes.subscribe();
    if !session_current() {
        return Err(RecoveryFailure::Failed(CallError::CallEnded));
    }
    if let Some(index) = deferred.iter().position(|cmd| cancels_recovery(cmd, call, presentation)) {
        let command = deferred.remove(index).unwrap();
        deferred.push_front(command);
        return Err(RecoveryFailure::Cancelled);
    }
    {
        let mut active = active_call.borrow_mut();
        let state = active.as_mut().ok_or(RecoveryFailure::Failed(CallError::NotInCall))?;
        if derive_call_handle(state) != *call || state.fresh_rejoin_used {
            return Err(RecoveryFailure::Failed(CallError::CallEnded));
        }
        // Consume even on timeout/error: no rapid success/loss cycle can renew it.
        state.fresh_rejoin_used = true;
    }
    let recovery = async {
        suspend_for_reconnect(
            ratchet_cancel,
            reconnect_impl(call_client, room_manager, active_call),
            || start_key_ratchet_timer(room_manager, ratchet_cancel, active_call),
        ).await?;
        if !session_current()
            || active_call.borrow().as_ref().map(derive_call_handle).as_ref() != Some(call) {
            return Err(CallError::CallEnded);
        }
        restore_reconnected_media(room_manager, active_call).await;
        Ok(())
    };
    tokio::pin!(recovery);
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(30));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            _ = &mut deadline => return Err(RecoveryFailure::Failed(
                CallError::ServerError("Call recovery timed out".into()),
            )),
            _ = session_changes.changed() => {
                if !session_current() {
                    return Err(RecoveryFailure::Failed(CallError::CallEnded));
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    return Err(RecoveryFailure::Failed(CallError::WorkerShutdown));
                };
                if cancels_recovery(&command, call, presentation) {
                    // Priority is deliberate: run the original owner hangup next;
                    // ordinary commands retain FIFO order and their response senders.
                    deferred.push_front(command);
                    stop_key_ratchet_timer(ratchet_cancel);
                    return Err(RecoveryFailure::Cancelled);
                }
                deferred.push_back(command);
            }
            result = &mut recovery => return result.map_err(RecoveryFailure::Failed),
        }
    }
}

pub(super) async fn finish_failed_recovery(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    ratchet_cancel: &Rc<RefCell<Option<oneshot::Sender<()>>>>,
    orchestrator: &mut OrchestratorState,
    events: &CallEventsTx,
    call: CallHandle,
) {
    stop_key_ratchet_timer(ratchet_cancel);
    let _ = leave_call_impl(call_client, room_manager, active_call).await;
    if orchestrator.active.as_ref().is_some_and(|active| active.call == call) {
        orchestrator.active = None;
    }
    orchestrator.outgoing.remove(&call);
    let _ = events.send(CallOrchestratorEvent::ActiveEnded {
        call, reason: CallEndReason::Error(CONNECTION_LOST.to_string()),
    });
}

/// The actor suspends before polling any reconnect await, and replaces the
/// timer only after staged installation succeeds, before media restoration awaits.
pub(super) async fn suspend_for_reconnect(
    cancel: &Rc<RefCell<Option<tokio::sync::oneshot::Sender<()>>>>,
    attempt: impl std::future::Future<Output = Result<(), CallError>>,
    restart: impl FnOnce(),
) -> Result<(), CallError> {
    stop_key_ratchet_timer(cancel);
    let result = attempt.await;
    if result.is_ok() {
        restart();
    }
    result
}

/// Reconnect to the current call after a connection drop.
///
/// This gets a new token from the server and reconnects to LiveKit.
pub(super) async fn reconnect_impl(
    call_client: &Arc<TokioMutex<CallClient>>,
    room_manager: &Arc<RoomManager>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
) -> Result<(), CallError> {
    // Get current call info
    let (call_id, snapshot, livekit_url, mut candidate, started_at) = {
        let mut active = active_call.borrow_mut();
        let state = active.as_mut().ok_or(CallError::NotInCall)?;
        let snapshot = room_manager.snapshot_reconnect_keys(&state.key_state)?;
        remember_reconnect_media(&mut state.reconnect_media, || ReconnectMedia {
            microphone: room_manager.reconnect_microphone_device(),
            camera: room_manager.reconnect_camera_selection(),
            muted: room_manager.is_microphone_muted(),
        });
        (
            state.call.id.to_string(),
            snapshot,
            state.livekit_url.clone(),
            state.key_state.clone(),
            state.call.started_at,
        )
    };

    tracing::info!(call_id = %call_id, "Attempting reconnection");

    // Capture belongs to the call, not the replaceable room transport.
    room_manager.disconnect_for_reconnect().await;

    // Get a new token by rejoining the call
    let response = {
        let mut client = call_client.lock().await;
        client
            .join_call(&call_id)
            .await
            .map_err(|e| CallError::ServerError(format!("Reconnect failed: {}", e)))?
    };

    // Reconnect with the staged absolute epoch and retained native receive ring
    room_manager
        .connect_reusing_keys_aligned(
            &livekit_url,
            &response.livekit_token,
            snapshot,
            &mut candidate,
            started_at,
        )
        .await?;
    // No await between room initialization and committing its authoritative
    // chain. The actor restarts the timer before restoration can await.
    active_call
        .borrow_mut()
        .as_mut()
        .ok_or(CallError::NotInCall)?
        .key_state = candidate;
    tracing::info!(call_id = %call_id, "Reconnected successfully");
    Ok(())
}

pub(super) async fn restore_reconnected_media(
    room_manager: &RoomManager,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
) {
    let media = {
        let mut active = active_call.borrow_mut();
        let Some(state) = active.as_mut() else {
            return;
        };
        // A keyless group joiner retains its deferred requests and its gate.
        let media = state.reconnect_media.take();
        if state.publish_gate.is_closed() {
            return;
        }
        media
    };
    let Some(media) = media else {
        return;
    };
    // Restore requested tracks, including a muted microphone, after the
    // authenticated gate and catch-up installation. Preserve its device.
    if let Some(device) = media.microphone {
        if let Err(error) = room_manager
            .publish_microphone_restored(Some(&device), media.muted)
            .await
        {
            tracing::warn!(%error, "reconnect microphone restoration failed");
        }
    }
    if let Err(error) = restore_camera_with(media.camera, |device, config| async move {
        room_manager
            .publish_camera(Some(&device), Some(config))
            .await
    })
    .await
    {
        tracing::warn!(%error, "reconnect camera restoration failed");
    }
}

