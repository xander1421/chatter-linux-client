---
phase: 17-network-auth-user-clients
plan: 01
subsystem: network
tags: [grpc, tonic, auth, session, device, streaming]

# Dependency graph
requires:
  - phase: 16-network-grpc-foundation
    provides: GrpcClientPool, AuthInterceptor, NetworkError
provides:
  - AuthClient wrapper with 16 authentication RPCs
  - Streaming register and delete_account methods
  - Session and device management APIs
affects: [services-auth, services-account, state-auth]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Wrapper client with InterceptedService<Channel, AuthInterceptor>"
    - "Streaming responses via impl Stream<Item = Result<T, NetworkError>>"
    - "register_and_wait convenience method pattern"

key-files:
  created: []
  modified:
    - src/network/grpc/auth_client.rs
    - src/network/grpc/mod.rs

key-decisions:
  - "Use device_id for session revocation (matches proto)"
  - "Optional parameters default to empty/zero"
  - "Streaming methods return impl Stream for flexibility"

patterns-established:
  - "RPC wrapper pattern: request → inner.method → map_err(NetworkError::from_status)"

issues-created: []

# Metrics
duration: 6min
completed: 2026-02-03
---

# Phase 17 Plan 01: AuthClient Core Summary

**Ergonomic AuthClient wrapper with 16 authentication RPCs including streaming register and delete_account**

## Performance

- **Duration:** 6 min
- **Started:** 2026-02-03T04:54:21Z
- **Completed:** 2026-02-03T05:00:39Z
- **Tasks:** 9 completed (1 skipped - dependency already present)
- **Files modified:** 2

## Accomplishments

- AuthClient wrapper around generated AuthServiceClient with InterceptedService
- Core auth: login, logout, check_username, verify_email, resend_verification_email
- Registration: streaming register with progress events, register_and_wait convenience
- Password: change_password, initiate_password_reset, complete_password_reset
- Sessions: list_sessions, revoke_session, revoke_all_sessions
- Devices: list_devices, rename_device, revoke_device
- Account: streaming delete_account

## Task Commits

Each task was committed atomically:

1. **Task 1: Add tokio-stream dependency** - SKIPPED (already in Cargo.toml)
2. **Task 2: AuthClient struct and constructor** - `8522078` (feat)
3. **Task 3: Core authentication RPCs** - `6a570e3` (feat)
4. **Task 4: Streaming register method** - `fa9e02a` (feat)
5. **Task 5: Password management RPCs** - `707ab1f` (feat)
6. **Task 6: Session management RPCs** - `3c02593` (feat)
7. **Task 7: Device management RPCs** - `8a6b6b7` (feat)
8. **Task 8: Streaming delete_account** - `e58db16` (feat)
9. **Task 9: Add tests for AuthClient** - `51f9b2e` (feat)
10. **Task 10: Update mod.rs exports** - `a74e387` (feat)

## Files Created/Modified

- `src/network/grpc/auth_client.rs` - Full implementation (454 lines)
- `src/network/grpc/mod.rs` - Added `pub use auth_client::AuthClient`

## Decisions Made

- **Use device_id for session revocation**: Proto uses u32 device_id not session_id string
- **Optional parameters default to empty**: e.g., `reason` in delete_account defaults to ""
- **Streaming methods return impl Stream**: Allows flexible consumption patterns

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Proto field name corrections**
- **Found during:** Tasks 3-8 (all RPC implementations)
- **Issue:** Plan guessed field names that differed from actual proto definitions
- **Fix:** Used correct field names from proto:
  - LogoutRequest: has `device_id: u32`
  - VerifyEmailRequest: uses `verification_token` not `code`
  - ResendVerificationEmailRequest: uses `email_or_username`
  - ChangePasswordRequest: has `request_id`
  - RequestPasswordResetRequest: uses `username_or_email` not `email`
  - CompletePasswordResetRequest: uses `reset_token` not `token`
  - InvalidateSessionRequest: uses `device_id: u32` not `session_id: String`
  - RenameDeviceRequest: uses `device_name` not `name`
  - DeleteAccountRequest: no `reason` field
- **Files modified:** src/network/grpc/auth_client.rs
- **Verification:** cargo check passes

**2. [Rule 1 - Bug] Task 1 skipped - dependency exists**
- **Found during:** Task 1
- **Issue:** tokio-stream already in Cargo.toml
- **Fix:** Skipped redundant addition
- **Verification:** Dependency available for use

---

**Total deviations:** 2 auto-fixed (1 blocking for field names, 1 skipped duplicate)
**Impact on plan:** All fixes necessary for correct proto compliance. No scope creep.

## Issues Encountered

None - plan executed smoothly with proto adaptations.

## Next Phase Readiness

- AuthClient complete with 16 RPCs
- Ready for 17-02-PLAN.md (Signal Protocol RPCs: prekeys, sender certificates)
- Pattern established for remaining service clients

---
*Phase: 17-network-auth-user-clients*
*Completed: 2026-02-03*
