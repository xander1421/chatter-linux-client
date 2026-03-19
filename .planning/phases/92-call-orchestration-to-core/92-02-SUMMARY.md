---
phase: 92-call-orchestration-to-core
plan: 02
status: complete
start: 2026-03-19T08:14:56Z
end: 2026-03-19T08:45:00Z
---

# 92-02 Summary: Anonymous Call Orchestration to Core

## Tasks Completed

### Task 1: Add anon call orchestration methods to core
**Commit:** `de57769`

- Added `AnonCallSetupResult` type to `domain/call.rs` (room_name, channel_id, encryption_key, verification_emojis, is_video, peer_display_name)
- Extended `CallSignalingService` with `Arc<AnonDmService>` and `Arc<AnonGroupService>` fields
- Added error variants: `AnonCall(String)`, `NoTokens`
- New orchestration methods:
  - `acquire_call_room_token()` — private helper for token acquisition with auto-request
  - `anon_verification_emojis()` — private helper for emoji derivation
  - `start_anon_dm_call()` — token acquisition, send offer, store pending state
  - `start_anon_group_call()` — same flow via AnonGroupService
  - `accept_anon_call()` — token, send accept, join room, connect LiveKit
  - `handle_anon_call_answer()` — caller-side: token, room join, LiveKit connect (or declined history)
  - `decline_anon_call()` — send decline, store missed call history
  - `store_anon_call_history_failed()` — private helper for failed/missed/declined call history
- Updated `call_signaling()` OnceLock in ChatterClient to pass anon services
- Added 5 facade methods on ChatterClient: `start_anon_dm_call`, `start_anon_group_call`, `accept_anon_call`, `handle_anon_call_answer`, `decline_anon_call`

### Task 2: Simplify UI anon call paths to use core methods
**Commit:** `47c274b`

- `outgoing.rs`:
  - Anon DM call section (was ~190 lines of token/offer/pending/history) → single `start_anon_dm_call()` call + UI update
  - `start_private_group_call()` (was ~150 lines) → single `start_anon_group_call()` call + UI update
  - Removed `anon_dm_service` and `anon_group_service` from `start_call_impl()` parameters
  - Removed 2 ctx clones from `wire_start_call()`
- `incoming.rs`:
  - Anonymous accept section (was ~300 lines of token/answer/join/connect) → single `accept_anon_call()` call + UI update
  - Anonymous decline section (was ~50 lines of routing) → single `decline_anon_call()` call
  - Removed `anon_dm_service` and `anon_group_service` from wire function
- `anon_calls.rs`:
  - `handle_anon_call_answer()` (was ~260 lines of token/join/connect/history) → single `handle_anon_call_answer()` call + UI update
  - `handle_anon_group_call_answer()` (was ~140 lines) → delegates to `handle_anon_call_answer()` with ID conversion
- Net reduction: **722 lines removed** from UI layer

## Files Modified

### Core (chatter-core)
- `crates/chatter-core/src/domain/call.rs` — `AnonCallSetupResult` type
- `crates/chatter-core/src/services/call_signaling.rs` — 7 new methods, anon service fields, error variants
- `crates/chatter-core/src/client.rs` — 5 facade methods, updated OnceLock constructor

### UI (chatter-ui)
- `crates/chatter-ui/src/bin_ui/features/calls/outgoing.rs` — delegates anon DM + group calls to core
- `crates/chatter-ui/src/bin_ui/features/calls/incoming.rs` — delegates accept + decline to core
- `crates/chatter-ui/src/bin_ui/features/auth/inbox/domain_handler/anon_calls.rs` — delegates answer handling to core

## Verification

- [x] `cargo build` succeeds (both crates)
- [x] `cargo test -p chatter-core --lib` passes (1373 tests, 0 failures)
- [x] No `get_available_call_room_token()` calls in chatter-ui
- [x] No `store_call_history()` calls in chatter-ui for anonymous calls (only regular DM/channel calls remain in active.rs)
- [x] No `join_anon_call_room()` calls in chatter-ui
- [x] No `connect_with_token()` calls in chatter-ui
- [x] No `request_call_room_tokens` in chatter-ui

## Deviations

- **`handle_anon_group_call_answer` rewritten as delegation**: Instead of duplicating the core delegation pattern, the group variant now directly calls `handle_anon_call_answer` with an ID conversion. This is simpler and correct because both use the same pending call infrastructure keyed by room_name.
- **Call history on failure stored in core**: The plan mentioned storing call history on failure in core. The `store_anon_call_history_failed()` helper in `CallSignalingService` writes directly to `MessageService.store_message_local()` rather than going through the existing `ChatterClient.store_call_history()` method, because the signaling service doesn't have access to `ChatterClient`. The behavior is identical.

## Phase 92 Status

With Plans 01 and 02 complete, all call orchestration (DM, anonymous DM, anonymous group) now lives in `chatter-core`:
- **DM calls**: `start_dm_call`, `complete_dm_call`, `accept_dm_call` (Plan 01)
- **Anonymous calls**: `start_anon_dm_call`, `start_anon_group_call`, `accept_anon_call`, `handle_anon_call_answer`, `decline_anon_call` (Plan 02)

The UI layer contains only Slint property updates and media publishing (microphone/camera). Phase 92 is complete.
