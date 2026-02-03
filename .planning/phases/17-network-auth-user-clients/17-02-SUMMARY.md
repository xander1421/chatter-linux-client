---
phase: 17-network-auth-user-clients
plan: 02
subsystem: network
tags: [grpc, signal-protocol, prekeys, sealed-sender, tonic]

requires:
  - phase: 17-01
    provides: AuthClient core structure with gRPC client
provides:
  - Signal Protocol prekey upload/fetch/refresh RPCs
  - Sender certificate request/retrieval RPCs
  - Server trust root RPC
  - Complete AuthClient with 25 methods
affects: [phase-30-services-auth, phase-32-services-message]

tech-stack:
  added: []
  patterns:
    - Proto nested type access (signal_pre_key_bundle::PreKeyRecord)
    - Bundle-based RPC parameters (wraps fields in PreKeyBundle)

key-files:
  created: []
  modified:
    - src/network/grpc/auth_client.rs

key-decisions:
  - "Used proto bundle wrapper (PreKeyBundle) instead of individual fields"
  - "Added helper method get_pre_key_bundles_all_devices for convenience"

patterns-established:
  - "Signal Protocol RPCs follow same error handling pattern as core RPCs"
  - "Request IDs generated for all operations"

issues-created: []

duration: 4min
completed: 2026-02-03
---

# Phase 17 Plan 02: Signal Protocol RPCs Summary

**AuthClient Signal Protocol methods for prekey management and sealed sender certificates**

## Performance

- **Duration:** 4 min
- **Started:** 2026-02-03T05:03:47Z
- **Completed:** 2026-02-03T05:07:47Z
- **Tasks:** 6
- **Files modified:** 1

## Accomplishments

- Implemented prekey upload/fetch/refresh/stats RPCs
- Implemented sender certificate request/retrieval RPCs
- Implemented server trust root RPC
- AuthClient now has 25 total async methods (18 from 17-01 + 7 Signal RPCs)
- Added 3 new tests for Signal Protocol types

## Task Commits

Each task was committed atomically:

1. **Task 1: Implement prekey upload RPC** - `a22c691` (feat)
2. **Task 2: Implement prekey fetch RPC** - `795adad` (feat)
3. **Task 3: Implement prekey refresh and stats RPCs** - `53ca1f5` (feat)
4. **Task 4: Implement sender certificate RPCs** - `9b86ef6` (feat)
5. **Task 5: Implement server trust root RPC** - `211636b` (feat)
6. **Task 6: Add Signal Protocol helper tests** - `44aa8af` (feat)

**Plan metadata:** (this commit)

## Files Created/Modified

- `src/network/grpc/auth_client.rs` - Added ~185 lines with 7 new methods and 3 tests

## New Methods Added

| Method | Purpose |
|--------|---------|
| `upload_pre_key_bundle` | Upload identity key + signed prekey + OTPKs |
| `get_pre_key_bundle` | Fetch bundle for X3DH session establishment |
| `get_pre_key_bundles_all_devices` | Convenience wrapper for device_id="*" |
| `refresh_pre_keys` | Replenish one-time prekeys |
| `get_pre_key_stats` | Monitor remaining prekey count |
| `request_sender_certificate` | Request new sealed sender certificate |
| `get_sender_certificate` | Get cached certificate |
| `get_server_trust_root` | Get server signing key for verification |

## Decisions Made

1. **Proto bundle wrapper:** Used `UploadPreKeyBundleRequest.bundle` field (proto structure) rather than individual fields
2. **Convenience helper:** Added `get_pre_key_bundles_all_devices` for common use case
3. **Request IDs:** Added to all requests as per proto requirements

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Proto field names differ from plan**
- **Found during:** Task 1 (prekey upload)
- **Issue:** Plan showed individual fields but proto uses nested `PreKeyBundle` with `PreKeyRecord` and `SignedPreKeyRecord`
- **Fix:** Used correct proto types: `signal_pre_key_bundle::PreKeyRecord`, `signal_pre_key_bundle::SignedPreKeyRecord`
- **Files modified:** src/network/grpc/auth_client.rs
- **Verification:** cargo check passes

**2. [Rule 3 - Blocking] RequestSenderCertificateRequest has device_id not include_e164**
- **Found during:** Task 4 (sender certificate)
- **Issue:** Proto uses `device_id: i32` not `include_e164: bool`
- **Fix:** Updated method signature to accept `device_id`
- **Files modified:** src/network/grpc/auth_client.rs
- **Verification:** cargo check passes

---

**Total deviations:** 2 auto-fixed (both blocking - proto mismatch), 0 deferred
**Impact on plan:** Proto field differences required adaptation but functionality unchanged. No scope creep.

## Issues Encountered

None - plan executed successfully after adapting to proto definitions.

## Next Phase Readiness

- AuthClient complete with all 25 methods
- Signal Protocol RPCs ready for Services Auth layer (Phase 30)
- Ready for 17-03 (UserClient implementation)

---
*Phase: 17-network-auth-user-clients*
*Completed: 2026-02-03*
