use super::*;

/// Key ratchet interval in seconds.
/// Ratcheting every 30 seconds provides good forward secrecy without excessive overhead.
pub(super) const KEY_RATCHET_INTERVAL_SECS: u64 = 30;

/// Given the seconds elapsed since the shared call-start anchor, return the
/// epoch that should be current now and the delay (seconds) until the next
/// ratchet boundary at `call_start + (epoch+1) * interval`.
///
/// Pure so it can be unit-tested without a running call. Both peers feed the
/// same `call_start`, so they compute the same boundaries and ratchet in
/// lockstep regardless of when each connected.
pub(super) fn aligned_ratchet_schedule(elapsed_secs: f64, interval_secs: u64) -> (u32, f64) {
    let interval = interval_secs as f64;
    let elapsed = elapsed_secs.max(0.0);
    let epoch_now = (elapsed / interval).floor() as u32;
    let next_boundary = (f64::from(epoch_now) + 1.0) * interval;
    (epoch_now, (next_boundary - elapsed).max(0.0))
}

/// A timer tick can fire a little before the wall-clock boundary it was
/// scheduled for (monotonic and wall clocks drift); it still counts as that
/// boundary.
pub(super) const RATCHET_TICK_TOLERANCE_SECS: f64 = 0.5;

/// Most epochs one fast-forward may ratchet through: 24 h of 30 s epochs. A
/// server-asserted `started_at` further back would otherwise have the call
/// worker hash up to `u32::MAX` times.
pub(super) const MAX_RATCHET_CATCH_UP: u32 = 24 * 60 * 2;

/// Ratchet `key_state` up to chain position `target`, returning what to hand
/// LiveKit when it moved. A chain already at or past `target` stays put: a
/// rotation is issued one position ahead (`CallKeyState::rotate`) and the
/// clock catches up to it rather than the chain running further ahead — so
/// every participant's position stays the wall-clock epoch (or the newest
/// rotation's, until the clock passes it), however many rotations the call
/// has had — so a joiner's first key stays within `MAX_JOIN_LEAD`.
pub(super) fn ratchet_to(key_state: &mut CallKeyState, target: u32) -> Option<(u32, Vec<u8>, Vec<u8>)> {
    let behind = target.checked_sub(key_state.epoch()).filter(|n| *n > 0)?;
    if behind > MAX_RATCHET_CATCH_UP {
        tracing::error!(
            epoch = key_state.epoch(),
            target,
            "Call key ratchet target out of range; not advancing"
        );
        return None;
    }
    while key_state.epoch() < target {
        if let Err(e) = key_state.ratchet() {
            tracing::error!("Key ratchet failed: {}", e);
            return None;
        }
    }
    Some(chain_install(key_state))
}

/// Reconnect uses the exact elapsed epoch, without introducing a new anchor
/// or advancing early using the timer's boundary tolerance.
pub(crate) fn reconnect_wall_epoch(start: chrono::DateTime<chrono::Utc>) -> u32 {
    let elapsed = (chrono::Utc::now() - start).num_milliseconds() as f64 / 1000.0;
    aligned_ratchet_schedule(elapsed, KEY_RATCHET_INTERVAL_SECS).0
}

/// Install each missed chain step, rather than skipping receive material in a
/// multi-epoch outage. This fills the native ring with previous/current/next
/// inputs in order; the final call selects the aligned sending epoch. The
/// authority stays unchanged on any rejection and the same chain can retry.
pub(crate) fn install_reconnect_catch_up(
    chain: &mut CallKeyState,
    target: u32,
    mut install: impl FnMut(u32, Vec<u8>, Vec<u8>) -> bool,
) -> Result<(), CallError> {
    let target = target.max(chain.epoch());
    if target - chain.epoch() > MAX_RATCHET_CATCH_UP {
        return Err(CallError::CryptoError(
            "Reconnect catch-up exceeds limit".into(),
        ));
    }
    let mut candidate = chain.clone();
    // Preflight exhaustion and derivation before mutating the native provider.
    let mut final_state = candidate.clone();
    while final_state.epoch() < target {
        final_state
            .ratchet()
            .map_err(|e| CallError::CryptoError(e.to_string()))?;
    }
    final_state
        .peek_next_key()
        .map_err(|e| CallError::CryptoError(e.to_string()))?;
    loop {
        let next = candidate
            .peek_next_key()
            .map_err(|e| CallError::CryptoError(e.to_string()))?;
        if !install(
            candidate.epoch(),
            candidate.current_key_vec(),
            next.to_vec(),
        ) {
            return Err(CallError::CryptoError(
                "Reconnect catch-up install failed".into(),
            ));
        }
        if candidate.epoch() == target {
            break;
        }
        candidate
            .ratchet()
            .map_err(|e| CallError::CryptoError(e.to_string()))?;
    }
    *chain = candidate;
    Ok(())
}

/// Start the coordinated key ratchet timer.
///
/// Both peers ratchet at wall-clock boundaries anchored to the server-assigned
/// call start time (`call.started_at`, identical for every participant). A peer
/// that connects 4s later than another still ratchets epoch N at exactly
/// `call_start + N*interval`, so the two never drift out of sync — the earlier
/// free-running `interval()` (counting from each peer's own connect moment) let
/// them ratchet up to `interval` seconds apart, which froze video whenever the
/// skew window coincided with a keyframe/loss (LiveKit's own recovery ratchet is
/// disabled). A peer joining after boundaries have already passed fast-forwards
/// its key chain to the current epoch on start.
///
/// The ratchet uses `CallKeyState::ratchet()` (HKDF-SHA256 with call_id + epoch)
/// which is deterministic, then pushes the new key to LiveKit.
///
/// # Forward secrecy
/// Each ratchet derives a new key from the current one. Compromising epoch N
/// doesn't reveal keys for epochs 0..N-1 (pre-compromise security).
///
/// # Post-compromise security
/// After a key compromise, the next ratchet produces a key the attacker can derive.
/// However, once the call ends, the keys are zeroized and can't be recovered.
pub(super) fn start_key_ratchet_timer(
    room_manager: &Arc<RoomManager>,
    ratchet_cancel: &Rc<RefCell<Option<tokio::sync::oneshot::Sender<()>>>>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
) {
    let manager = room_manager.clone();
    start_key_ratchet_scheduler(
        ratchet_cancel,
        active_call,
        Rc::new(move |epoch, current, next| manager.set_ratcheted_shared_key(epoch, current, next)),
        Rc::new(chrono::Utc::now),
    );
}

// Clock and installation boundaries are injectable; production and boundary
// regressions execute this same local scheduler and cancellation lifecycle.
pub(super) fn start_key_ratchet_scheduler(
    ratchet_cancel: &Rc<RefCell<Option<tokio::sync::oneshot::Sender<()>>>>,
    active_call: &Rc<RefCell<Option<ActiveCallState>>>,
    install: Rc<dyn Fn(u32, Vec<u8>, Vec<u8>) -> bool>,
    now: Rc<dyn Fn() -> chrono::DateTime<chrono::Utc>>,
) {
    // Cancel any existing timer
    stop_key_ratchet_timer(ratchet_cancel);

    let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
    *ratchet_cancel.borrow_mut() = Some(cancel_tx);

    // Shared wall-clock anchor. Read once — it's fixed for the call's lifetime.
    let call_start = active_call
        .borrow()
        .as_ref()
        .and_then(|s| s.call.started_at);

    let active_call = active_call.clone();

    tokio::task::spawn_local(async move {
        // A reconnect may cancel us before this local task is first polled.
        // Never perform initial catch-up against a room already being removed.
        if !matches!(
            cancel_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ) {
            return;
        }
        // Advance our deterministic CallKeyState to the wall-clock epoch (one
        // step past ours when there is no anchor) and push that epoch's key
        // plus the next (forward window) to LiveKit. Returns false if the call
        // ended (caller should stop the timer).
        let ratchet_once = |active_call: &Rc<RefCell<Option<ActiveCallState>>>,
                            install: &dyn Fn(u32, Vec<u8>, Vec<u8>) -> bool|
         -> bool {
            let ratcheted = {
                let mut call_guard = active_call.borrow_mut();
                let Some(state) = call_guard.as_mut() else {
                    tracing::debug!("Key ratchet timer stopping: no active call");
                    return false;
                };
                let target = match call_start {
                    Some(start) => {
                        aligned_ratchet_schedule(
                            (now() - start).num_milliseconds() as f64 / 1000.0
                                + RATCHET_TICK_TOLERANCE_SECS,
                            KEY_RATCHET_INTERVAL_SECS,
                        )
                        .0
                    }
                    None => state.key_state.epoch().saturating_add(1),
                };
                let mut candidate = state.key_state.clone();
                ratchet_to(&mut candidate, target).map(|install| (candidate, install))
            };
            if let Some((candidate, (epoch, key_cur, key_next))) = ratcheted {
                if !install(epoch, key_cur, key_next) {
                    tracing::warn!("Failed to push ratcheted key to LiveKit; timer stopping");
                    return false;
                }
                if let Some(state) = active_call.borrow_mut().as_mut() {
                    state.key_state = candidate;
                }
                tracing::info!(epoch, "Coordinated key ratchet completed");
            }
            true
        };

        // Align the first tick to the next boundary past `call_start`, and
        // catch up any epochs that elapsed before we connected.
        let first_delay_secs = match call_start {
            Some(start) => {
                let elapsed = (now() - start).num_milliseconds() as f64 / 1000.0;
                let (epoch_now, delay) =
                    aligned_ratchet_schedule(elapsed, KEY_RATCHET_INTERVAL_SECS);

                // Fast-forward to the wall-clock epoch (a peer that joined after
                // one or more boundaries already passed).
                let ratcheted = active_call.borrow_mut().as_mut().and_then(|state| {
                    let mut candidate = state.key_state.clone();
                    ratchet_to(&mut candidate, epoch_now).map(|install| (candidate, install))
                });
                if let Some((candidate, (epoch, key_cur, key_next))) = ratcheted {
                    if !install(epoch, key_cur, key_next) {
                        tracing::warn!("Failed to install join catch-up media key; timer stopping");
                        return;
                    }
                    if let Some(state) = active_call.borrow_mut().as_mut() {
                        state.key_state = candidate;
                    }
                    tracing::info!(epoch, "Fast-forwarded key to wall-clock epoch on join");
                }
                delay
            }
            // No anchor (shouldn't happen): behave like the old free-running timer.
            None => KEY_RATCHET_INTERVAL_SECS as f64,
        };

        // Wait to the aligned boundary, then ratchet every interval from there.
        let sleep = tokio::time::sleep(tokio::time::Duration::from_secs_f64(first_delay_secs));
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {}
            _ = &mut cancel_rx => {
                tracing::debug!("Key ratchet timer cancelled");
                return;
            }
        }

        // interval_at with start=now fires its first tick immediately (at the
        // boundary we just reached), then every interval after.
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now(),
            tokio::time::Duration::from_secs(KEY_RATCHET_INTERVAL_SECS),
        );

        loop {
            tokio::select! {
                biased;
                _ = &mut cancel_rx => {
                    tracing::debug!("Key ratchet timer cancelled");
                    break;
                }
                _ = interval.tick() => {
                    if !ratchet_once(&active_call, install.as_ref()) {
                        break;
                    }
                }
            }
        }
    });

    tracing::info!(
        interval_secs = KEY_RATCHET_INTERVAL_SECS,
        "Key ratchet timer started (wall-clock aligned)"
    );
}

/// Auto-publish microphone (and camera, for video calls) on the worker
/// thread immediately after a successful connect.
///
/// Previously the UI spawned a `publish_media` task from every accept /
/// key-exchange path. Centralising it in the actor:
///   - removes business logic from the UI layer,
///   - guarantees all media publication runs on the worker thread (LiveKit
///     tracks are `!Send`), and
///   - ensures we publish exactly once per connect regardless of who
///     initiated (caller, callee, DM, group).
///
/// Failures are logged but do not fail the connect — the call stays up
/// even if mic/camera permissions are denied.
pub(super) async fn auto_publish_media(room_manager: &RoomManager, is_video: bool) -> bool {
    if let Err(e) = room_manager.publish_microphone(None).await {
        tracing::warn!(error = %e, "auto-publish microphone failed");
    }
    if is_video {
        // None lets the room manager pick a sensible default capture config.
        if let Err(e) = room_manager.publish_camera(None, None).await {
            tracing::warn!(error = %e, "auto-publish camera failed");
            return true;
        }
    }
    false
}

/// Stop the periodic key ratchet timer.
pub(super) fn stop_key_ratchet_timer(ratchet_cancel: &Rc<RefCell<Option<tokio::sync::oneshot::Sender<()>>>>) {
    if let Some(cancel_tx) = ratchet_cancel.borrow_mut().take() {
        let _ = cancel_tx.send(());
    }
}

