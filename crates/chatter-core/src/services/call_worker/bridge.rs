use super::*;

// ============================================================================
// EventBus bridge
// ============================================================================
//
// Subscribes to DomainEvents on the main tokio runtime and forwards call
// offers / answers / hangups into the worker's orchestrator state. Replaces
// the per-event UI-side dispatch that used to live in
// `domain_handler/calls.rs` and which scattered modal-policy
// decisions across the UI layer.

/// Spawn the EventBus → CallWorker orchestrator bridge. Safe to call once at
/// app startup, after both the EventBus and the call worker exist.
///
/// `signaling` lets the bridge tell our own devices' hangups from a peer's.
pub fn spawn_call_orchestrator_bridge(
    bus: &EventBus,
    worker: CallWorkerHandle,
    signaling: crate::services::call_signaling::CallSignalingService,
) -> tokio::task::JoinHandle<()> {
    use tokio::sync::broadcast::error::RecvError;
    let mut rx = bus.subscribe_domain();
    let mut room_rx = worker.subscribe_room_events();
    let bus = bus.clone();
    let mut sessions = worker.incoming_presentation.subscribe_changes();
    let mut owner = worker.incoming_presentation.session_revision();
    tokio::spawn(async move {
        tracing::info!("Call orchestrator EventBus bridge started");
        loop {
            tokio::select! {
                biased;
                changed = sessions.changed() => {
                    if changed.is_err() { break; }
                    let now = worker.incoming_presentation.session_revision();
                    if now != owner { owner = now; rx = bus.subscribe_domain(); }
                }
                event = rx.recv() => match event {
                    Ok(event) => {
                        let now = worker.incoming_presentation.session_revision();
                        if now != owner { owner = now; rx = bus.subscribe_domain(); continue; }
                        let scoped = match owner { Some(rev) => worker.for_presentation_session(rev), None => worker.clone() };
                        if scoped.presentation_session_is_current() { handle_domain_event(&scoped, &signaling, event).await; }
                    },
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(
                            skipped = n,
                            "Call orchestrator bridge lagged on DomainEvent channel"
                        );
                    }
                    Err(RecvError::Closed) => {
                        tracing::info!("Call orchestrator bridge stopping — domain bus closed");
                        break;
                    }
                },
                event = room_rx.recv() => match event {
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => {
                        tracing::info!("Call orchestrator bridge stopping — room bus closed");
                        break;
                    }
                },
            }
        }
    })
}

/// How long a 1:1 peer may be absent from the room before the call is ended
/// for them. Covers the peer's own hangup signal landing first (it is sent
/// before they leave the room, and it ends the call with history) and a brief
/// LiveKit reconnect, which reappears under the same identity.
pub(super) const PEER_LEFT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether `departed` leaving ends the call: nobody else is left in the room
/// (`remaining` may still list `departed` — the SDK's eviction order is not
/// something to depend on), and it is a 1:1 call.
pub(super) fn peer_left_ends_call(
    kind: Option<OrchestratedCallKind>,
    remaining: &[String],
    departed: &str,
) -> bool {
    kind == Some(OrchestratedCallKind::DmStandard) && remaining.iter().all(|id| id == departed)
}

/// How long an incoming offer may ring. Signal's ring timeout: an offer older
/// than this — one that sat in the queue while the app was offline, or whose
/// call was answered on another of our devices long ago — never rings.
pub const RING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether an offer received at `sent_ns` (ns since epoch) is past the ring
/// timeout at `now_ns`. For any offer that is the server's receive stamp
/// (Signal's rule; the inbox falls back to the caller's clock only when the
/// server gave none). `0` means nothing was stamped: ring rather than silently
/// drop a live call. A clock ahead of ours saturates to age 0 and rings.
pub(crate) fn incoming_ring_budget(sent_ns: u64, now_ns: u64) -> std::time::Duration {
    let age = if sent_ns == 0 { 0 } else { now_ns.saturating_sub(sent_ns) };
    RING_TIMEOUT.saturating_sub(std::time::Duration::from_nanos(age))
}

pub(crate) fn offer_is_stale(sent_ns: u64, now_ns: u64) -> bool {
    sent_ns != 0 && now_ns.saturating_sub(sent_ns) >= RING_TIMEOUT.as_nanos() as u64
}

pub(crate) fn offer_is_stale_now(sent_ns: u64) -> bool {
    offer_is_stale(sent_ns, crate::util::time::TimestampNanos::now().as_u64())
}

/// The withdrawal reason for a hangup saying another of our devices answered
/// or declined.
pub(super) fn elsewhere_reason(reason: crate::services::gateway::CallHangupReason) -> Option<CallEndReason> {
    use crate::services::gateway::CallHangupReason as R;
    match reason {
        R::AcceptedElsewhere => Some(CallEndReason::AnsweredElsewhere),
        R::DeclinedElsewhere => Some(CallEndReason::Declined),
        _ => None,
    }
}

pub(super) async fn handle_domain_event(
    worker: &CallWorkerHandle,
    signaling: &crate::services::call_signaling::CallSignalingService,
    event: DomainEvent,
) {
    match event {
        DomainEvent::CallKeyOfferReceived {
            call_id,
            peer_id,
            ephemeral_public_key,
            call_type,
            is_identity_verified,
            identity_warning,
            caller_name,
            timestamp,
            ..
        } => {
            let is_video = call_type.eq_ignore_ascii_case("video")
                || call_type.eq_ignore_ascii_case("Video Call");
            if offer_is_stale_now(timestamp) {
                worker.orch_missed_incoming(
                    CallHandle::from(call_id),
                    OrchestratedCallKind::DmStandard,
                    caller_name,
                    is_video,
                    peer_id.to_string(),
                );
                return;
            }
            worker.orch_present_incoming_offer(
                CallHandle::from(call_id),
                OrchestratedCallKind::DmStandard,
                peer_id,
                caller_name,
                is_video,
                is_identity_verified,
                identity_warning,
                Some(ephemeral_public_key),
                // DM has no channel name — notification reads "X is calling".
                None,
                // DMs are channel-free: the conversation key IS the peer.
                peer_id.to_string(),
                timestamp,
            );
        }

        DomainEvent::CallHangupReceived {
            call_id,
            peer_id,
            reason,
        } => {
            // Signal's Hangup ACCEPTED/DECLINED: another of OUR devices took
            // this call. Only our own user may say so, and it only stops the
            // ringing — it must never tear down a call this device is in.
            if let Some(reason) = elsewhere_reason(reason) {
                if signaling.is_current_user(&peer_id) {
                    withdraw_domain_call(worker, CallHandle::from(call_id), reason, false).await;
                    return;
                }
                tracing::warn!(peer_id = %peer_id, "Answered/declined-elsewhere hangup from another user; treating as a plain hangup");
            }
            // The callee declined our outgoing call: say so, and file it as
            // declined rather than unanswered.
            let end = if reason == crate::services::gateway::CallHangupReason::Declined {
                CallEndReason::Declined
            } else {
                CallEndReason::RemoteHangup
            };
            // Still ringing? The caller gave up — stop ringing. A no-op when
            // this call was never presented or was already accepted.
            let call = CallHandle::from(call_id);
            withdraw_domain_call(worker, call, end, true).await;
        }

        // Key answers are completed by the UI/glue layer against the
        // ChatterClient API; group-call signals (ring, rotation, key request)
        // by `ChatterClient::handle_group_call_signal`, which needs channel
        // membership and the server's view of the call.
        DomainEvent::CallKeyAnswerReceived { .. } | DomainEvent::GroupCallSignalReceived { .. } => {
        }

        _ => {}
    }
}

/// Run on the separate EventBus bridge, never on the call actor itself.
/// Apply the withdrawal before any exact-call disconnect, using the existing
/// bounded actor queue for both so saturation cannot drop either command.
pub(super) async fn withdraw_domain_call(
    worker: &CallWorkerHandle,
    call: CallHandle,
    reason: CallEndReason,
    disconnect: bool,
) {
    if let Err(error) = worker
        .orch_withdraw_incoming_reliably(call.clone(), reason.clone())
        .await
    {
        tracing::debug!(%error, "Domain call withdrawal worker unavailable");
        return;
    }
    if disconnect {
        let (tx, rx) = oneshot::channel();
        if worker
            .cmd_tx
            .send(CallCommand::DisconnectLocal {
                notify_server: false,
                session_revision: worker.presentation_session,
                call: Some(call),
                reason,
                response: tx,
            })
            .await
            .is_err()
            || rx.await.is_err()
        {
            tracing::debug!("Domain call disconnect worker unavailable");
        }
    }
}

