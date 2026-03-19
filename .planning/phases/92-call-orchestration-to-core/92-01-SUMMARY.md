---
phase: 92-call-orchestration-to-core
plan: 01
status: complete
start: 2026-03-19T07:54:03Z
end: 2026-03-19T08:15:00Z
---

# 92-01 Summary: DM Call Orchestration to Core

## Tasks Completed

### Task 1: Add DM call orchestration types and methods to core
**Commit:** `2e597ef`

- Added `CallSetupResult` and `PendingDmCall` types to `domain/call.rs`
- Extended `CallSignalingService` with:
  - `pending_dm_call: Arc<Mutex<Option<PendingDmCall>>>` field
  - `set_pending_dm_call()`, `take_pending_dm_call()`, `clear_pending_dm_call()` methods
  - `start_dm_call()` — generates ephemeral key, creates call, sends offer, stores pending
  - `complete_dm_call()` — takes pending, validates, derives key via Signal actor, updates LiveKit
  - `accept_dm_call()` — generates ephemeral key, derives key, sends answer, joins call
- Added error variants: `NoPendingCall`, `CallIdMismatch`, `KeyDerivationFailed`, `CallWorker`

### Task 2: Expose orchestration methods through ChatterClient
**Commit:** `5f0a5d5`

- Added `start_dm_call()`, `complete_dm_call()`, `accept_dm_call()` facade methods
- Changed `call_signaling()` from creating a new instance per call to returning a persistent `&CallSignalingService` via `OnceLock` (required for pending state persistence)
- Fixed 2 UI call sites (`active.rs`, `outgoing.rs`) that needed `.clone()` for the new ref return type

### Task 3: Wire UI to use new core methods for DM calls
**Commit:** `a4c8548`

- `outgoing.rs`: Replaced EphemeralCallKey::generate + initiate_call + send_call_offer + set_pending_call with single `start_dm_call()` call
- `key_exchange.rs`: Replaced PENDING_OUTGOING_CALL thread_local + signal actor call + set_encryption_key with `complete_dm_call()` delegation; `clear_pending_call()` is now a no-op (state lives in core)
- `incoming.rs`: Replaced EphemeralCallKey::generate + signal actor call + send_call_answer + join_call with single `accept_dm_call()` call
- `event_handlers.rs`: Passes `ui_ctx` instead of `signal_service` to `handle_call_key_answer`
- Net reduction: **193 lines removed** from UI layer

## Files Modified

### Core (chatter-core)
- `crates/chatter-core/src/domain/call.rs` — new types
- `crates/chatter-core/src/services/call_signaling.rs` — pending state + 3 orchestration methods
- `crates/chatter-core/src/client.rs` — 3 facade methods + persistent call_signaling

### UI (chatter-ui)
- `crates/chatter-ui/src/bin_ui/features/calls/outgoing.rs` — delegates to core
- `crates/chatter-ui/src/bin_ui/features/calls/incoming.rs` — delegates to core
- `crates/chatter-ui/src/bin_ui/features/calls/key_exchange.rs` — rewritten to delegate
- `crates/chatter-ui/src/bin_ui/features/calls/event_handlers.rs` — updated handler signature
- `crates/chatter-ui/src/bin_ui/features/calls/active.rs` — .clone() fix

## Verification

- [x] `cargo build` succeeds (both crates)
- [x] `cargo test -p chatter-core --lib` passes (1373 tests, 0 failures)
- [x] No `EphemeralCallKey` usage in chatter-ui
- [x] No `PENDING_OUTGOING_CALL` thread_local in UI
- [x] No `signal_service.actor` in calls/ UI files for DM path

## Deviations

- **call_signaling() return type change**: Changed from returning `CallSignalingService` by value to returning `&CallSignalingService` by reference (via OnceLock). This was necessary because the service now holds pending call state that must persist across the offer->answer lifecycle. Two existing UI call sites required `.clone()` to accommodate the new borrow semantics.
- **clear_pending_call() kept as no-op**: The function is still called by `cleanup_call_ui_state()` which runs in `invoke_from_event_loop` without ChatterClient access. Made it a no-op since core manages its own pending state lifecycle.
