---
phase: 93-svr-encrypted-key-backup
plan: 04
subsystem: network, services
tags: [tonic, grpc, rust, svr, backup, client]

# Dependency graph
requires:
  - phase: 93-svr-encrypted-key-backup
    plan: 01
    provides: svr.proto RPCs and message definitions
  - phase: 93-svr-encrypted-key-backup
    plan: 03
    provides: encrypt_backup, decrypt_backup, derive_backup_id, KeyBackupPayload
provides:
  - SvrClient gRPC client (store, fetch, delete backup RPCs)
  - SvrBackupService orchestrating crypto + network + settings
  - ChatterClient facade accessor for SVR backup operations
  - Rust proto types for SVR messages and tonic service client
affects: [93-05]

# Tech tracking
tech-stack:
  added: []
  patterns: [OnceLock lazy service initialization on ChatterClient, RestoreResult struct for multi-field returns]

key-files:
  created:
    - crates/chatter-core/src/network/grpc/svr_client.rs
    - crates/chatter-core/src/services/svr_backup_service.rs
  modified:
    - crates/chatter-core/src/proto/chat.v1.rs
    - crates/chatter-core/src/proto/chat.v1.tonic.rs
    - crates/chatter-core/src/network/grpc/mod.rs
    - crates/chatter-core/src/services/mod.rs
    - crates/chatter-core/src/client.rs

key-decisions:
  - "Manual proto type addition (buf token expired) — hand-wrote prost/tonic types matching svr.proto"
  - "OnceLock lazy initialization on ChatterClient (matches call_signaling pattern, avoids ServiceContainer changes)"
  - "SvrBackupService takes Arc<GrpcClientPool> + Arc<SettingsRepository> (minimal deps)"
  - "RestoreResult struct returns identity_keypair + profile_key + remaining_attempts"
  - "Version tracked in settings_repo as crypto.svr_backup_version"

patterns-established:
  - "SVR service lazy init via OnceLock on ChatterClient"
  - "SvrBackupError with From traits for crypto/network/storage error composition"

issues-created: []

# Metrics
duration: 15min
completed: 2026-03-27
---

# Phase 93-04: Client SvrClient + SvrBackupService Summary

**SvrClient gRPC client + SvrBackupService orchestrating encrypt-upload and fetch-decrypt-restore, accessible via ChatterClient facade**

## Performance

- **Duration:** 15 min
- **Started:** 2026-03-27
- **Completed:** 2026-03-27
- **Tasks:** 2 (+1 pre-work for proto generation)
- **Files created:** 2
- **Files modified:** 5

## Accomplishments
- Hand-generated Rust proto types (5 messages + tonic SvrServiceClient) to unblock client work
- SvrClient gRPC wrapper with store/fetch/delete methods and status code mapping
- SvrBackupService with full backup lifecycle: create+upload, fetch+restore, delete
- Monotonic version tracking via settings_repo
- ChatterClient facade accessor via OnceLock lazy initialization

## Task Commits

1. **Pre-work: SVR proto types** - `5b65eb0` (chore)
2. **Task 1: SvrClient gRPC client** - `4bb4c93` (feat)
3. **Task 2: SvrBackupService** - `73054ed` (feat)

## Files Created/Modified
- `crates/chatter-core/src/proto/chat.v1.rs` - Added 5 SVR message types (prost-style)
- `crates/chatter-core/src/proto/chat.v1.tonic.rs` - Added svr_service_client tonic module
- `crates/chatter-core/src/network/grpc/svr_client.rs` - SvrClient + FetchBackupResult
- `crates/chatter-core/src/network/grpc/mod.rs` - Added svr_client module + re-exports + pool method
- `crates/chatter-core/src/services/svr_backup_service.rs` - SvrBackupService + SvrBackupError + RestoreResult
- `crates/chatter-core/src/services/mod.rs` - Added module + re-exports
- `crates/chatter-core/src/client.rs` - Added svr_backup() accessor via OnceLock

## Decisions Made
- Manually wrote proto types (prost message structs + tonic client) since buf token expired
- OnceLock lazy init on ChatterClient (avoids touching InfrastructureContainer/ServiceContainer)
- SvrBackupService uses minimal deps (GrpcClientPool + SettingsRepository only)
- RestoreResult struct for multi-field return (identity_keypair, profile_key, remaining_attempts)

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Rust SVR proto types missing**
- **Found during:** Pre-work check
- **Issue:** buf generate failed in 93-01 (expired token), no Rust proto types for SVR
- **Fix:** Hand-wrote prost message types and tonic service client matching svr.proto exactly
- **Files modified:** chat.v1.rs, chat.v1.tonic.rs
- **Verification:** `cargo check --package chatter-core` passes
- **Committed in:** `5b65eb0`

---

**Total deviations:** 1 auto-fixed (1 blocking), 0 deferred
**Impact on plan:** Proto generation was essential pre-work. Hand-written types match what buf would generate.

## Issues Encountered
None beyond the proto generation deviation.

## Next Phase Readiness
- Full SVR backup pipeline ready for auth integration (93-05)
- SvrBackupService accessible via `chatter_client.svr_backup()`
- create_and_upload_backup ready for post-registration hook
- fetch_and_restore_backup ready for new-device login flow

---
*Phase: 93-svr-encrypted-key-backup*
*Completed: 2026-03-27*
