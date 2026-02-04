---
phase: 30-services-auth
plan: 02
subsystem: auth
tags: [auth-service, login, register, logout, signal-auth, basic-auth]

# Dependency graph
requires:
  - phase: 30-services-auth
    provides: DeviceCredentials for device token generation and UAK derivation
  - phase: 17-network-auth-user-clients
    provides: AuthClient and AuthInterceptor for server communication
  - phase: 11-crypto-identity-sessions
    provides: IdentityKeyManager for identity key generation
  - phase: 12-crypto-prekeys
    provides: PreKeyManager for prekey generation
provides:
  - AuthService for orchestrating auth flows
  - AuthError enum for auth-specific errors
  - LoginResult struct for auth outcomes
  - RegistrationResult in auth_client for capturing user_id
affects: [services-account, state-auth, ui-auth-screens]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Service orchestration: AuthService coordinates crypto, network, storage layers"
    - "Basic auth configuration: set_basic() after successful login/register"
    - "Registration event streaming: capture UserRegistered for user_id"

key-files:
  created: []
  modified:
    - src/services/auth_service.rs
    - src/network/grpc/auth_client.rs

key-decisions:
  - "SHA256 password hashing (server expects pre-hashed)"
  - "Profile key generated fresh on each login (UAK derivation)"
  - "Identity keys preserved on logout (support re-login)"
  - "RegistrationResult captures user_id from UserRegistered event stream"

patterns-established:
  - "Auth flows store user_id and device_id in settings_repo"
  - "Auth interceptor configured immediately after successful auth"
  - "Profile created/updated as part of login flow"

issues-created: []

# Metrics
duration: 4min
completed: 2026-02-04
---

# Phase 30 Plan 02: AuthService Summary

**AuthService orchestrating login, registration, and logout flows with Signal-style Basic auth configuration**

## Performance

- **Duration:** 4 min
- **Started:** 2026-02-04T16:08:38Z
- **Completed:** 2026-02-04T16:12:40Z
- **Tasks:** 3
- **Files modified:** 2

## Accomplishments

- AuthService struct with all dependencies (identity_manager, prekey_manager, user_repo, settings_repo, key_store, auth_interceptor)
- Login flow: hash password → get/generate identity → generate credentials → authenticate → persist state → configure interceptor
- Register flow: generate identity → generate prekeys → generate credentials → register → persist state → configure interceptor
- Logout flow: notify server → clear interceptor → clear settings (preserve identity keys)
- RegistrationResult type for capturing user_id from registration event stream

## Task Commits

All 3 tasks committed together as they were interdependent:

1. **Tasks 1-3: AuthService implementation** - `5a73a3e` (feat)

**Plan metadata:** Pending (this commit)

## Files Created/Modified

- `src/services/auth_service.rs` - Full AuthService implementation replacing stub
- `src/network/grpc/auth_client.rs` - Added RegistrationResult, updated register_and_wait

## Decisions Made

- SHA256 password hashing: Server expects pre-hashed passwords, using SHA256 for now
- Fresh profile key on login: Each login generates new profile key for UAK derivation (could be persisted in future)
- Preserve identity keys on logout: Allows re-login without regenerating identity
- RegistrationResult type: Added to auth_client.rs to capture user_id from UserRegistered event in registration stream

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Session proto lacks user_id field**
- **Found during:** Task 2 (login flow implementation)
- **Issue:** Session proto doesn't contain user_id, only device_id and auth tokens
- **Fix:** For login: extract user_id from AuthResponse.user_profile. For register: added RegistrationResult capturing UserRegistered event from stream
- **Files modified:** src/network/grpc/auth_client.rs
- **Verification:** cargo check passes, user_id properly extracted
- **Committed in:** 5a73a3e

---

**Total deviations:** 1 auto-fixed (blocking), 0 deferred
**Impact on plan:** Fix necessary to extract user_id from server responses. No scope creep.

## Issues Encountered

None - plan executed as written once proto structure was understood.

## Next Phase Readiness

- AuthService ready for UI integration
- Login, register, logout flows complete
- Basic auth properly configured via AuthInterceptor
- Phase 30 complete, ready for Phase 31 (Services Account)

---
*Phase: 30-services-auth*
*Completed: 2026-02-04*
