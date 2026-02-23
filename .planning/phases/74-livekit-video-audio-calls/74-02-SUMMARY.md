---
phase: 74-livekit-video-audio-calls
plan: 02
subsystem: crypto
tags: [livekit, e2ee, hkdf, x25519, ecdh, call-encryption]

# Dependency graph
requires:
  - phase: 74-01
    provides: LiveKit dependencies and CallClient wrapper
  - phase: 14
    provides: Signal Protocol sealed sender and identity keys
provides:
  - CallKeyProvider for deriving E2EE keys from Signal identity
  - HKDF-based key derivation with call-specific binding
  - Key ratcheting for forward secrecy in long calls
  - SignalService integration for call key extraction
affects: [74-03, 74-04, 74-05]

# Tech tracking
tech-stack:
  added: []
  patterns: ["HKDF key derivation for call encryption", "ECDH on identity keys"]

key-files:
  created:
    - src/crypto/call_key_provider.rs
  modified:
    - src/crypto/mod.rs
    - src/services/signal_service.rs
    - Cargo.toml

key-decisions:
  - "Use ECDH on identity keys (not session keys) for call encryption"
  - "HKDF-SHA256 with call-specific info for key binding"
  - "Key ratcheting via epoch counter for forward secrecy"
  - "Update ffmpeg-next 7→8 for system compatibility"

patterns-established:
  - "CallKeyState lifecycle pattern for key management during calls"

issues-created: []

# Metrics
duration: 20min
completed: 2026-02-23
---

# Phase 74 Plan 02: Call Encryption Key Derivation Summary

**HKDF-based call key derivation from Signal identity keys for LiveKit E2EE**

## Performance

- **Duration:** 20 min
- **Started:** 2026-02-23T02:55:29Z
- **Completed:** 2026-02-23T03:15:51Z
- **Tasks:** 2
- **Files modified:** 4

## Accomplishments

- Created CallKeyProvider module with `derive_call_key()` function
- CallKeyState struct for managing key lifecycle during calls
- Key ratcheting for forward secrecy in long-running calls
- Added `derive_call_encryption_key()` to SignalService using ECDH
- Added `derive_call_key()` convenience method combining ECDH + HKDF

## Task Commits

Each task was committed atomically:

1. **Task 1: Create call_key_provider module** - `50d1fe2` (feat)
2. **Task 2: Add call key derivation to SignalService** - `2d12c36` (feat)

## Files Created/Modified

- `src/crypto/call_key_provider.rs` - Key derivation module with HKDF and ratcheting
- `src/crypto/mod.rs` - Added call_key_provider export
- `src/services/signal_service.rs` - ECDH key extraction and call key derivation
- `Cargo.toml` - Updated ffmpeg-next 7→8 for compatibility

## Decisions Made

- **ECDH on identity keys:** Use identity keys for call encryption rather than session root keys, as libsignal doesn't expose session internals
- **Key binding:** HKDF info parameter includes call_id to bind keys to specific calls
- **Ratcheting:** CallKeyState.ratchet() derives new key from current key + epoch for forward secrecy

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Updated ffmpeg-next to version 8**
- **Found during:** Task 1 verification (cargo check)
- **Issue:** ffmpeg-next 7.x incompatible with system ffmpeg 8.0 (missing avfft.h header)
- **Fix:** Updated ffmpeg-next = "7" → "8" in Cargo.toml
- **Files modified:** Cargo.toml, Cargo.lock
- **Verification:** cargo check passes
- **Committed in:** 50d1fe2 (Task 1 commit)

---

**Total deviations:** 1 auto-fixed (blocking)
**Impact on plan:** Required for compilation on systems with ffmpeg 8.0

## Issues Encountered

None

## Next Phase Readiness

- Call key derivation infrastructure complete
- Ready for 74-03 (CallState Management) which will use these keys
- LiveKit E2EE integration can use `SignalService.derive_call_key()`

---
*Phase: 74-livekit-video-audio-calls*
*Completed: 2026-02-23*
