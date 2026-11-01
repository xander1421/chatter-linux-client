use super::*;


/// The room's seniority: our LiveKit `joined_at_ms` and each remote
/// participant's `(identity, joined_at_ms)`. Server-assigned, so every
/// participant sees the same order — and server-asserted, like the roster.
#[derive(Debug, Clone)]
pub struct RoomSeniority {
    pub local_joined_at_ms: i64,
    pub remote: Vec<(String, i64)>,
}

/// Internal state for active call.
pub(super) struct ActiveCallState {
    pub(super) call: Call,
    /// Key state for Signal-based key derivation and ratcheting.
    pub(super) key_state: CallKeyState,
    /// LiveKit server URL for reconnection.
    pub(super) livekit_url: String,
    /// Closed while we hold none of the call's keys (a group joiner).
    pub(super) publish_gate: PublishGate,
    /// Retained across failed reconnect attempts, since teardown clears capture.
    pub(super) reconnect_media: Option<ReconnectMedia>,
    /// One fresh-token recovery for the lifetime of this call, including explicit reconnects.
    pub(super) fresh_rejoin_used: bool,
}

/// The same helper serves production snapshots and state-transition regressions.
pub(super) fn call_verification_snapshot(
    active: Option<&ActiveCallState>,
    orchestrator: &OrchestratorState,
    connected: bool,
    installed_epoch: u32,
) -> Option<CallVerificationSnapshot> {
    let state = active?;
    let handle = derive_call_handle(state);
    let kind = orchestrator
        .active
        .as_ref()
        .filter(|a| a.call == handle)
        .map(|a| a.kind)
        .or_else(|| orchestrator.outgoing.get(&handle).map(|a| a.kind));
    let verification = match kind {
        Some(OrchestratedCallKind::DmStandard) => CallEncryptionVerification::ExistingHandshake,
        Some(OrchestratedCallKind::GroupChannel)
            if connected
                && !state.publish_gate.is_closed()
                && installed_epoch == state.key_state.epoch()
                && state.key_state.call_id() == state.call.id.to_string() =>
        {
            let data =
                crate::services::call_verification::CallVerification::derive_verification_emojis(
                    state.key_state.current_key(),
                    &state.call.id.to_string(),
                );
            CallEncryptionVerification::MediaKey {
                epoch: installed_epoch,
                emojis: data
                    .emojis
                    .try_into()
                    .expect("verification always derives four emojis"),
            }
        }
        _ => CallEncryptionVerification::WaitingForKey,
    };
    Some(CallVerificationSnapshot {
        call: state.call.clone(),
        verification,
    })
}

/// The active call's group-key standing ([`CallWorkerHandle::group_key_status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupKeyStatus {
    pub call_id: CallId,
    /// Generation of the newest key from outside our chain
    /// ([`CallKeyState::anchor`]); 0 = only our starting key.
    pub anchor: u32,
    /// Our media is held back: we hold no key of the call's yet.
    pub awaiting_key: bool,
}

#[derive(Clone)]
pub(super) struct ReconnectMedia {
    /// Outer option records publication intent; inner option is the requested device.
    pub(super) microphone: Option<Option<String>>,
    pub(super) camera: Option<(String, VideoCaptureConfig)>,
    pub(super) muted: bool,
}

// Failed attempts must not replace intent with the disconnected manager's state.
pub(super) fn remember_reconnect_media(
    saved: &mut Option<ReconnectMedia>,
    snapshot: impl FnOnce() -> ReconnectMedia,
) {
    saved.get_or_insert_with(snapshot);
}

pub(super) async fn restore_camera_with<F, Fut>(
    camera: Option<(String, VideoCaptureConfig)>,
    publish: F,
) -> Result<(), CallError>
where
    F: FnOnce(String, VideoCaptureConfig) -> Fut,
    Fut: std::future::Future<Output = Result<(), CallError>>,
{
    if let Some((device, config)) = camera {
        publish(device, config).await?;
    }
    Ok(())
}

/// Media a channel group-call participant may not publish yet (§124 1k).
///
/// A joiner connects under a random key of its own that nobody else holds —
/// the ring no longer carries one — so frames it sent before the key
/// authority's key arrived would be undecryptable to everyone, and before
/// this gate they went out under the ring's root key, which every channel
/// member held. Closed, it publishes nothing and remembers what was asked
/// for; it opens when the authority's key is installed or we rotate
/// ourselves, and the remembered mic/camera are published then — after the
/// key is in LiveKit, so the first frame already uses it.
#[derive(Debug, Default)]
pub(super) struct PublishGate {
    pub(super) closed: bool,
    /// `Some(device)`: publish the microphone (on `device`) once open.
    pub(super) mic: Option<Option<String>>,
    pub(super) camera: bool,
}

/// What a [`PublishGate`] had held back when it opened.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct DeferredMedia {
    pub(super) mic: Option<Option<String>>,
    pub(super) camera: bool,
}

impl PublishGate {
    /// A joiner's gate: closed, with the call's auto-publish waiting in it.
    pub(super) fn awaiting_key(is_video: bool) -> Self {
        Self {
            closed: true,
            mic: Some(None),
            camera: is_video,
        }
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed
    }

    /// Hold back a microphone publish; false (nothing held) when open.
    pub(super) fn defer_mic(&mut self, device: Option<String>) -> bool {
        if self.closed {
            self.mic = Some(device);
        }
        self.closed
    }

    /// Hold back a camera publish; false (nothing held) when open.
    pub(super) fn defer_camera(&mut self) -> bool {
        if self.closed {
            self.camera = true;
        }
        self.closed
    }

    pub(super) fn drop_mic(&mut self) {
        self.mic = None;
    }

    pub(super) fn drop_camera(&mut self) {
        self.camera = false;
    }

    /// Open the gate: what it held back, the first time; `None` when it
    /// was already open.
    pub(super) fn open(&mut self) -> Option<DeferredMedia> {
        if !self.closed {
            return None;
        }
        self.closed = false;
        Some(DeferredMedia {
            mic: self.mic.take(),
            camera: std::mem::take(&mut self.camera),
        })
    }
}

/// Run `f` on the active call's publish gate; false when not in a call.
pub(super) fn gate_defers(
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    f: impl FnOnce(&mut PublishGate) -> bool,
) -> bool {
    active_call
        .borrow_mut()
        .as_mut()
        .is_some_and(|s| f(&mut s.publish_gate))
}

/// Open the active call's publish gate and publish what it held back.
/// Spawned: capture start-up must not hold the command loop.
pub(super) fn open_publish_gate(
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    room_manager: &Arc<RoomManager>,
    call_events_tx: Option<&CallEventsTx>,
) {
    let Some(media) = active_call
        .borrow_mut()
        .as_mut()
        .and_then(|s| s.publish_gate.open())
    else {
        return;
    };
    room_manager.timing_mark("call_key_accepted_publish_gate_open");
    tracing::info!(
        mic = media.mic.is_some(),
        camera = media.camera,
        "Call key held: publishing"
    );
    let rm = room_manager.clone();
    let call = active_call.borrow().as_ref().map(|s| CallHandle::new(s.call.id.to_string()));
    let events = call_events_tx.cloned();
    tokio::task::spawn_local(async move {
        if let Some(device) = media.mic {
            if let Err(e) = rm.publish_microphone(device.as_deref()).await {
                tracing::warn!(error = %e, "deferred microphone publish failed");
            }
        }
        if media.camera {
            if let Err(e) = rm.publish_camera(None, None).await {
                tracing::warn!(error = %e, "deferred camera publish failed");
                if let (Some(call), Some(events)) = (call, events) {
                    let _ = events.send(CallOrchestratorEvent::CameraUnavailable { call });
                }
            }
        }
    });
}

// These are the command actor's production installation paths. A zeroizing
// candidate is committed only after native media key selection succeeds, so a
// rejected install neither opens publication nor makes the authority epoch stale.
pub(super) fn reset_dm_media_key(
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    room_manager: &Arc<RoomManager>,
    key: Vec<u8>,
) -> bool {
    let key = zeroize::Zeroizing::new(key);
    let Ok(root) = <[u8; 32]>::try_from(key.as_slice()) else {
        return false;
    };
    let root = zeroize::Zeroizing::new(root);
    let mut guard = active_call.borrow_mut();
    let Some(state) = guard.as_mut() else {
        return false;
    };
    let Ok(candidate) = CallKeyState::new(&root, state.key_state.call_id()) else {
        return false;
    };
    // The callee installs this derived media key, never the raw ECDH root.
    if !room_manager.reset_shared_key(candidate.current_key_vec()) {
        return false;
    }
    state.key_state = candidate;
    true
}

pub(super) fn rotate_group_media_key(
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    room_manager: &Arc<RoomManager>,
    call_events_tx: Option<&CallEventsTx>,
    new_key: [u8; 32],
) -> Result<u32, CallError> {
    let mut guard = active_call.borrow_mut();
    let state = guard.as_mut().ok_or(CallError::NotInCall)?;
    let mut candidate = state.key_state.clone();
    candidate
        .rotate(new_key)
        .map_err(|e| CallError::CryptoError(e.to_string()))?;
    let (epoch, current, next) = chain_install(&candidate);
    if !room_manager.set_ratcheted_shared_key(epoch, current, next) {
        return Err(CallError::CryptoError(
            "Media key installation failed".into(),
        ));
    }
    state.key_state = candidate;
    drop(guard);
    tracing::info!(epoch, "Call key rotated after a membership change");
    open_publish_gate(active_call, room_manager, call_events_tx);
    Ok(epoch)
}

pub(super) fn install_group_media_key(
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    room_manager: &Arc<RoomManager>,
    call_events_tx: Option<&CallEventsTx>,
    call_id: CallId,
    key: [u8; 32],
    epoch: u32,
) -> Result<bool, CallError> {
    let mut guard = active_call.borrow_mut();
    let state = guard
        .as_mut()
        .filter(|s| s.call.id == call_id)
        .ok_or(CallError::NotInCall)?;
    let mut candidate = state.key_state.clone();
    let Some((selected_epoch, current, next)) = adopt_group_key(&mut candidate, key, epoch) else {
        tracing::debug!(
            epoch,
            "Group call key is not newer, or too far ahead; ignored"
        );
        return Ok(false);
    };
    if !room_manager.set_ratcheted_shared_key(selected_epoch, current, next) {
        return Err(CallError::CryptoError(
            "Media key installation failed".into(),
        ));
    }
    state.key_state = candidate;
    drop(guard);
    tracing::info!(
        epoch = selected_epoch,
        "Installed group call key from participant"
    );
    open_publish_gate(active_call, room_manager, call_events_tx);
    Ok(true)
}

/// [`adopt_group_key`] into the active call, which must be `call_id`.
#[cfg(test)]
pub(super) fn adopt_into_call(
    active: Option<&mut ActiveCallState>,
    call_id: CallId,
    key: [u8; 32],
    epoch: u32,
) -> Result<Option<(u32, Vec<u8>, Vec<u8>)>, CallError> {
    match active {
        Some(state) if state.call.id == call_id => {
            Ok(adopt_group_key(&mut state.key_state, key, epoch))
        }
        _ => Err(CallError::NotInCall),
    }
}

/// Adopt a group-call `key` issued at `epoch` into `key_state`
/// ([`CallKeyState::adopt`]), returning what to hand LiveKit. `None` when the
/// key is refused (not newer than the last one adopted, or too far ahead).
pub(super) fn adopt_group_key(
    key_state: &mut CallKeyState,
    key: [u8; 32],
    epoch: u32,
) -> Option<(u32, Vec<u8>, Vec<u8>)> {
    match key_state.adopt(key, epoch) {
        Ok(true) => Some(chain_install(key_state)),
        Ok(false) => None,
        Err(e) => {
            tracing::error!(error = %e, "Group call key derivation failed");
            None
        }
    }
}

/// What to hand LiveKit for the chain's current position (its epoch = the
/// key index): that key, and the next epoch's pre-installed — the same shape
/// the periodic ratchet installs, so a rotator and every receiver of its key
/// hold byte-identical chains and LiveKit indices.
pub(super) fn chain_install(key_state: &CallKeyState) -> (u32, Vec<u8>, Vec<u8>) {
    let key_cur = key_state.current_key_vec();
    let key_next = key_state
        .peek_next_key()
        .map(|k| k.to_vec())
        .unwrap_or_else(|_| key_cur.clone());
    (key_state.epoch(), key_cur, key_next)
}

