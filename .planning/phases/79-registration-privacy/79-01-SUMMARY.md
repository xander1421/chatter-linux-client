---
phase: 79-registration-privacy
plan: 01
subsystem: auth
tags: [signal-protocol, uak, profile-key, privacy, registration]

# Dependency graph
requires:
  - phase: 30-services-auth
    provides: AuthService registration flow
provides:
  - UAK-only registration (profile_key never sent to server)
  - Local profile_key storage for future profile encryption
  - Bytes storage methods in SettingsRepository
affects: [79-02 (display_name encryption), profile encryption]

# Tech tracking
tech-stack:
  added: []
  patterns: [UAK derivation client-side, profile_key local storage]

key-files:
  created: []
  modified:
    - src/network/grpc/auth_client.rs
    - src/services/auth_service.rs
    - src/storage/repositories/settings_repo.rs

key-decisions:
  - "UAK (16 bytes) sent instead of profile_key (32 bytes) - matches Signal"
  - "profile_key stored in SQLCipher encrypted DB for future profile encryption"
  - "Base64 encoding for bytes storage in settings table"
  - "profile_key field set to Vec::new() in RegisterRequest (deprecated)"

patterns-established:
  - "UAK derivation happens client-side before any network call"
  - "Cryptographic keys stored via set_bytes/get_bytes in SettingsRepository"

issues-created: []

# Metrics
duration: 4min
completed: 2026-03-05
---

# Phase 79 Plan 01: Stop Sending Profile Key Summary

**Profile key now stays client-side; only derived UAK (16 bytes) sent to server during registration**

## Performance

- **Duration:** ~4 min
- **Started:** 2026-03-05T05:42:53Z
- **Completed:** 2026-03-05T05:46:28Z
- **Tasks:** 3
- **Files modified:** 3

## Accomplishments

- auth_client.rs register functions now accept `&[u8; 16]` UAK instead of `&[u8; 32]` profile_key
- auth_service.rs derives UAK client-side using `derive_uak()` before registration
- Profile key stored locally in SQLCipher database via new `set_bytes()` method
- UAK sent in `prekey_bundle.unidentified_access_key` field
- `profile_key` field in RegisterRequest set to empty `Vec::new()` (deprecated)

## Task Commits

Each task was committed atomically:

1. **Task 1: Update auth_client to accept UAK** - `b0a5557` (feat)
2. **Task 2: Derive UAK client-side in auth_service** - `5641b79` (feat)
3. **Task 3: Add bytes storage for profile_key** - `9e02e3c` (feat)

**Plan metadata:** (this commit)

## Files Created/Modified

- `src/network/grpc/auth_client.rs` - Changed register() and register_and_wait() to accept UAK parameter
- `src/services/auth_service.rs` - Added UAK derivation, local profile_key storage, pass UAK to server
- `src/storage/repositories/settings_repo.rs` - Added get_bytes() and set_bytes() for raw key storage

## Decisions Made

- **UAK in prekey_bundle:** Signal sends UAK in the prekey bundle's `unidentified_access_key` field, so we follow that pattern
- **Deprecated profile_key field:** Set to empty Vec rather than removing to maintain proto wire compatibility
- **Base64 for bytes storage:** JSON serialization requires string encoding; base64 is standard for binary data

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None.

## Next Phase Readiness

- Profile key stored locally, ready for 79-02 (display_name encryption)
- UAK correctly derived and sent to server
- All existing tests pass (except pre-existing unrelated mailbox test issues)

---
*Phase: 79-registration-privacy*
*Completed: 2026-03-05*
