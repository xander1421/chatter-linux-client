# Call Lifecycle Cleanup — Fix Successive Call Failures

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix stale orchestrator state and event handler races that cause successive calls to fail or hang.

**Architecture:** Three surgical fixes — (1) RoomManager tracks and awaits its spawned event handler task so disconnect is deterministic, (2) the call worker's LeaveCall/EndCall/DisconnectLocal handlers clear the orchestrator active slot atomically alongside the LiveKit disconnect, and (3) the workaround `orch_mark_ended` calls in CallSignalingService are removed since cleanup now lives in the right place.

**Tech Stack:** Rust, tokio (spawn_local, JoinHandle, timeout), parking_lot::RwLock, LiveKit SDK

---

## File Map

| File | Change | Purpose |
|------|--------|---------|
| `crates/chatter-core/src/calls/room_manager.rs` | Modify | Add event_task JoinHandle, connect timeout, disconnect await |
| `crates/chatter-core/src/services/call_worker.rs` | Modify | Atomic orchestrator cleanup in leave/end/disconnect handlers |
| `crates/chatter-core/src/services/call_signaling.rs` | Modify | Remove orch_mark_ended workarounds |

---

### Task 1: RoomManager — Track Event Handler Task

**Files:**
- Modify: `crates/chatter-core/src/calls/room_manager.rs:89-109` (struct def)
- Modify: `crates/chatter-core/src/calls/room_manager.rs:136-148` (constructor)
- Modify: `crates/chatter-core/src/calls/room_manager.rs:576-603` (connect spawn_local)
- Modify: `crates/chatter-core/src/calls/room_manager.rs:608-661` (disconnect)
- Modify: `crates/chatter-core/src/calls/room_manager.rs:492-495` (Room::connect timeout)

- [ ] **Step 1: Add `event_task` field to RoomManager struct**

In the struct definition (line 89), add a field to hold the event handler's JoinHandle:

```rust
pub struct RoomManager {
    /// Current room (if connected).
    room: Arc<RwLock<Option<Room>>>,
    /// Current state.
    state: Arc<RwLock<RoomState>>,
    /// Event sender for UI updates (bounded for backpressure).
    event_tx: mpsc::Sender<RoomEvent>,
    /// Audio capture manager.
    audio_capture: Arc<RwLock<AudioCapture>>,
    /// Audio playback manager for remote participants.
    audio_playback: Arc<RwLock<Option<AudioPlayback>>>,
    /// Video capture manager.
    video_capture: Arc<RwLock<VideoCapture>>,
    /// Video playback manager for remote participants.
    video_playback: Arc<RwLock<VideoPlayback>>,
    /// Screen capture manager.
    #[cfg(not(target_os = "android"))]
    screen_capture: Arc<RwLock<ScreenCapture>>,
    /// E2EE key state (provider + epoch).
    e2ee: Arc<RwLock<E2eeState>>,
    /// Handle to the spawned event handler task so disconnect() can await it.
    event_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}
```

- [ ] **Step 2: Initialize `event_task` in constructor**

In `with_shared_frames` (line 136), add the field:

```rust
        let manager = Self {
            room: Arc::new(RwLock::new(None)),
            state: Arc::new(RwLock::new(RoomState::Disconnected)),
            event_tx,
            audio_capture: Arc::new(RwLock::new(AudioCapture::new())),
            audio_playback: Arc::new(RwLock::new(audio_playback)),
            video_capture: Arc::new(RwLock::new(VideoCapture::new())),
            video_playback: Arc::new(RwLock::new(video_playback)),
            #[cfg(not(target_os = "android"))]
            screen_capture: Arc::new(RwLock::new(ScreenCapture::new())),
            e2ee: Arc::new(RwLock::new(E2eeState { provider: None, epoch: 0 })),
            event_task: Arc::new(RwLock::new(None)),
        };
```

- [ ] **Step 3: Add 10s timeout to Room::connect()**

Replace the bare `Room::connect` (line 492-495) with a timeout wrapper:

```rust
        // Connect to room (timeout prevents indefinite hang on slow/dead servers)
        let (room, mut events) = tokio::time::timeout(
            tokio::time::Duration::from_secs(10),
            Room::connect(url, token, room_options),
        )
        .await
        .map_err(|_| CallError::ConnectionFailed("LiveKit connection timed out (10s)".to_string()))?
        .map_err(|e| CallError::ConnectionFailed(e.to_string()))?;
```

- [ ] **Step 4: Store spawn_local JoinHandle in connect()**

Replace the fire-and-forget `tokio::task::spawn_local(...)` block (lines 586-603) with one that stores the handle:

```rust
        let event_task_slot = self.event_task.clone();
        let handle = tokio::task::spawn_local(async move {
            tracing::info!("Room event handler started");
            while let Some(event) = events.recv().await {
                tracing::info!("LiveKit room event received: {:?}", event);
                Self::handle_room_event(event, &event_tx, &state, &room_arc, &audio_playback, &video_playback);
                // Yield to allow other tasks (like forwarder) to run
                tokio::task::yield_now().await;
            }

            // Room disconnected - stop audio/video playback
            tracing::info!("Room event stream ended - marking disconnected");
            if let Some(playback) = audio_playback.read().as_ref() {
                playback.stop();
            }
            video_playback.read().stop();
            *state.write() = RoomState::Disconnected;
            let _ = event_tx.try_send(RoomEvent::StateChanged(RoomState::Disconnected));
        });
        *event_task_slot.write() = Some(handle);
```

- [ ] **Step 5: Await event handler drain in disconnect()**

After the `room.close()` timeout block in `disconnect()` (after line 658), add:

```rust
        // Wait for event handler task to finish draining
        let event_handle = self.event_task.write().take();
        if let Some(handle) = event_handle {
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(2),
                handle,
            ).await {
                Ok(Ok(())) => tracing::debug!("Event handler task joined cleanly"),
                Ok(Err(e)) => tracing::warn!("Event handler task panicked: {}", e),
                Err(_) => tracing::warn!("Event handler join timed out (2s), proceeding"),
            }
        }
```

- [ ] **Step 6: Verify it compiles**

Run: `cargo run -p chatter-ui 2>&1 | head -50`

Expected: no new compiler errors from room_manager.rs changes. App should launch.

- [ ] **Step 7: Commit**

```bash
git add crates/chatter-core/src/calls/room_manager.rs
git commit -m "fix(calls): track event handler task + add Room::connect timeout

RoomManager now stores the spawn_local JoinHandle and awaits it
in disconnect() so the old event handler fully drains before
a new call can connect. Also adds a 10s timeout to Room::connect()
to prevent indefinite hangs on unresponsive LiveKit servers."
```

---

### Task 2: CallWorker — Atomic Orchestrator Cleanup in Leave/End/Disconnect

**Files:**
- Modify: `crates/chatter-core/src/services/call_worker.rs:1174-1200` (LeaveCall/EndCall/DisconnectLocal handlers)

The root bug: `leave_call_impl` / `end_call_impl` clear `active_call` and disconnect LiveKit, but never touch `orchestrator.active`. The orchestrator thinks a call is still active, so the next incoming call shows `replaces_active = true`.

Fix: after each leave/end/disconnect, derive the `CallHandle` from the active state and clear the orchestrator.

- [ ] **Step 1: Add orchestrator cleanup helper**

Above the `run_call_worker` function (around line 1020), add a helper that extracts the CallHandle from an ActiveCallState:

```rust
/// Derive the orchestrator `CallHandle` from the active call state.
/// Anonymous calls key by room_name; standard calls by call_id.
fn derive_call_handle(state: &ActiveCallState) -> CallHandle {
    if state.room_name.starts_with("anon-") {
        CallHandle::from(state.room_name.clone())
    } else {
        CallHandle::from(state.call.id.to_string())
    }
}
```

- [ ] **Step 2: Update LeaveCall handler**

Replace the `CallCommand::LeaveCall` handler (lines 1174-1180):

```rust
                CallCommand::LeaveCall { response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    // Capture handle before leave clears active_call
                    let handle = active_call.read().as_ref().map(derive_call_handle);

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
                            reason: CallEndReason::LocalHangup,
                        });
                    }

                    let _ = response.send(result);
                }
```

- [ ] **Step 3: Update EndCall handler**

Replace the `CallCommand::EndCall` handler (lines 1182-1190):

```rust
                CallCommand::EndCall { reason, response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    let handle = active_call.read().as_ref().map(derive_call_handle);

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
```

- [ ] **Step 4: Update DisconnectLocal handler**

Replace the `CallCommand::DisconnectLocal` handler (lines 1192-1200):

```rust
                CallCommand::DisconnectLocal { response } => {
                    // Cancel key ratchet timer
                    stop_key_ratchet_timer(&ratchet_cancel);

                    let handle = active_call.read().as_ref().map(derive_call_handle);

                    room_manager.disconnect().await;
                    *active_call.write() = None;
                    tracing::info!("Disconnected locally from call");

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

                    let _ = response.send(());
                }
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo run -p chatter-ui 2>&1 | head -50`

Expected: no new compiler errors. The `derive_call_handle` function and updated handlers compile cleanly.

- [ ] **Step 6: Commit**

```bash
git add crates/chatter-core/src/services/call_worker.rs
git commit -m "fix(calls): atomic orchestrator cleanup in leave/end/disconnect

LeaveCall, EndCall, and DisconnectLocal handlers now clear the
orchestrator active slot and emit ActiveEnded inline, fixing the
stale-slot bug where successive calls saw replaces_active=true
because leave_call only disconnected LiveKit without touching
the orchestrator."
```

---

### Task 3: CallSignaling — Remove Workaround orch_mark_ended Calls

**Files:**
- Modify: `crates/chatter-core/src/services/call_signaling.rs:1001-1014` (end_dm_call)
- Modify: `crates/chatter-core/src/services/call_signaling.rs:1146-1163` (end_anon_call)

Now that `leave_call()` handles orchestrator cleanup internally (Task 2), the external workaround calls are redundant. They'd double-fire `ActiveEnded`, which is harmless (the second hit is a no-op) but noisy. Clean them out.

- [ ] **Step 1: Remove orch_mark_ended from end_dm_call**

In `end_dm_call` (lines 1001-1014), replace:

```rust
        // Leave the call
        if let Err(e) = call_worker.leave_call().await {
            tracing::error!(error = %e, "Failed to leave call");
        }

        // Same orchestrator-cleanup bug as `end_anon_call`: `leave_call`
        // only disconnects LiveKit, it doesn't touch the orchestrator
        // state. Without `orch_mark_ended` the active slot stays set
        // and the next incoming call is flagged "replaces active".
        use crate::services::call_worker::{CallEndReason, CallHandle};
        call_worker.orch_mark_ended(
            CallHandle::from(call.id.to_string()),
            CallEndReason::LocalHangup,
        );
```

with:

```rust
        // Leave the call (orchestrator cleanup is now atomic inside the worker)
        if let Err(e) = call_worker.leave_call().await {
            tracing::error!(error = %e, "Failed to leave call");
        }
```

- [ ] **Step 2: Remove orch_mark_ended from end_anon_call**

In `end_anon_call` (lines 1146-1163), replace:

```rust
        // Always leave/disconnect from the LiveKit room
        if let Err(e) = call_worker.leave_call().await {
            tracing::debug!(error = %e, "leave_call after anon hangup");
        }

        // Clear the orchestrator's active slot. Without this, the
        // `orch_mark_ended` only ever fires from the remote-hangup path
        // (call_worker.rs `AnonCallHangupReceived`); a local press of
        // the hangup button leaves `orchestrator.active` set, and the
        // next incoming call is presented with `replaces_active = true`
        // — UI shows "Accepting will end your current call" even
        // though there's nothing to end. The orchestrator keys active
        // calls by the anon room name, which we already have above.
        use crate::services::call_worker::{CallEndReason, CallHandle};
        call_worker.orch_mark_ended(
            CallHandle::from(room_name),
            CallEndReason::LocalHangup,
        );
```

with:

```rust
        // Leave the call (orchestrator cleanup is now atomic inside the worker)
        if let Err(e) = call_worker.leave_call().await {
            tracing::debug!(error = %e, "leave_call after anon hangup");
        }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo run -p chatter-ui 2>&1 | head -50`

Expected: no new compiler errors. Possibly dead-code warnings for `CallEndReason`/`CallHandle` imports — check and remove if no other use in the file.

- [ ] **Step 4: Check for dead imports**

Run: `grep -n 'use crate::services::call_worker' crates/chatter-core/src/services/call_signaling.rs`

If the two removed blocks were the only users of `CallEndReason`/`CallHandle` in this file, remove the now-dead `use` statements. The top-of-file imports already pull in `CallWorkerHandle` — those stay.

- [ ] **Step 5: Commit**

```bash
git add crates/chatter-core/src/services/call_signaling.rs
git commit -m "fix(calls): remove orch_mark_ended workarounds from signaling

leave_call() now handles orchestrator cleanup atomically inside
the worker (previous commit), making the external orch_mark_ended
calls in end_dm_call and end_anon_call redundant. Remove them and
the bug-documenting comments they carried."
```

---

### Task 4: Verify — Manual Successive Call Test

- [ ] **Step 1: Run the app**

Run: `cargo run -p chatter-ui`

- [ ] **Step 2: Test successive calls**

1. Start a DM call → hang up → immediately start another DM call
2. Verify second call connects without "replaces active" modal
3. Start an anon call → hang up → start another anon call  
4. Verify no stale state from first call leaks into second

- [ ] **Step 3: Test incoming during/after call**

1. End an active call
2. Have peer send a new incoming call
3. Verify incoming call shows fresh (no "replaces active" flag)

- [ ] **Step 4: Test rapid re-dial**

1. Start call → hang up → start call within 1-2 seconds
2. Verify no "already in call" error
3. Verify event handler from first call doesn't race with second
