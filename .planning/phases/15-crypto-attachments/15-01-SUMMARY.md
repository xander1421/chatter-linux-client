---
phase: 15-crypto-attachments
plan: 01
subsystem: crypto
tags: [aes-cbc, hmac-sha256, attachment-encryption, signal-compatible, constant-time]

# Dependency graph
requires:
  - phase: 10-crypto-signal-store
    provides: sha2, hmac, subtle crates
provides:
  - AttachmentCrypto for Signal-compatible file encryption
  - EncryptedAttachment struct with ciphertext, key, digest, plaintext_size
  - Log-bucketing padding to prevent file size inference
  - Verify-then-decrypt pattern with constant-time comparisons
affects: [19-network-supporting-clients, 32-services-message-core, 52-media-file-handling]

# Tech tracking
tech-stack:
  added: [aes 0.8, cbc 0.1, block-padding 0.3]
  patterns: [stateless-service, verify-then-decrypt, log-bucketing]

key-files:
  created: []
  modified:
    - src/crypto/attachment_crypto.rs
    - src/crypto/mod.rs
    - src/error/crypto_error.rs
    - Cargo.toml

key-decisions:
  - "AES-256-CBC + HMAC-SHA256 to match Signal format (not AES-GCM)"
  - "64-byte combined key: [AES key (32)] + [MAC key (32)]"
  - "Zero-padding with log-bucketing, not PKCS7 (matches Signal)"
  - "Constant-time comparison for both digest and HMAC verification"
  - "Round padded sizes to 16-byte multiples for AES block alignment"

patterns-established:
  - "Log-bucketing padding: max(541, ceil(1.05^ceil(log(size)/log(1.05))))"
  - "Ciphertext format: IV (16) || AES-CBC(padded) || HMAC-SHA256 (32)"
  - "Verify digest -> verify HMAC -> decrypt (ordered verification)"

issues-created: []

# Metrics
duration: 12min
completed: 2026-02-03
---

# Phase 15 Plan 01: Crypto Attachments Summary

**Signal-compatible AES-256-CBC + HMAC-SHA256 attachment encryption with log-bucketing padding**

## Performance

- **Duration:** 12 min
- **Started:** 2026-02-03T19:30:00Z
- **Completed:** 2026-02-03T19:42:00Z
- **Tasks:** 3
- **Files modified:** 4

## Accomplishments

- Added AES-CBC dependencies: aes 0.8, cbc 0.1, block-padding 0.3
- Added 4 attachment error variants to CryptoError:
  - AttachmentEncryptionFailed(String)
  - AttachmentDecryptionFailed(String)
  - AttachmentMacMismatch
  - AttachmentDigestMismatch
- Implemented AttachmentCrypto with Signal-compatible format:
  - `encrypt()`: Generates random 64-byte key, pads with log-bucketing, encrypts with AES-256-CBC, authenticates with HMAC-SHA256
  - `decrypt()`: Verifies digest, verifies HMAC, decrypts, removes padding
  - `compute_digest()`: SHA-256 utility for ciphertext
  - `padded_size()`: Log-bucketing formula implementation
- Comprehensive test suite with 14 tests covering:
  - Round-trip encryption/decryption
  - Empty and large (1MB) file handling
  - Digest and MAC mismatch detection
  - Padding size verification

## Task Commits

Each task was committed atomically:

1. **Task 1: Add AES-CBC dependencies** - `69563ae` (chore)
2. **Task 2: Add attachment error variants** - `f9443b3` (feat)
3. **Task 3: Implement AttachmentCrypto** - `dd40403` (feat)

**Plan metadata:** `a5562b6` (docs: complete plan)

## Files Created/Modified

- `Cargo.toml` - Added aes, cbc, block-padding dependencies
- `src/error/crypto_error.rs` - Added 4 attachment error variants
- `src/crypto/attachment_crypto.rs` - Full AttachmentCrypto implementation (455 lines including tests)
- `src/crypto/mod.rs` - Added AttachmentCrypto, EncryptedAttachment, KEY_SET_LENGTH exports

## Decisions Made

- **CBC+HMAC over GCM**: Matches Signal's exact format for interoperability. Signal uses CBC+HMAC for streaming support and backup tier compatibility.
- **Zero padding with log-bucketing**: Signal pads with zeros (not PKCS7) and uses log-bucketing formula to prevent file size inference.
- **Block-aligned padding**: All padded sizes rounded to 16-byte multiples for AES block alignment.
- **Constant-time everywhere**: Both digest and HMAC comparisons use `subtle::ConstantTimeEq` to prevent timing attacks.

## Deviations from Plan

None - plan executed as specified.

## Issues Encountered

None - plan executed smoothly.

## Next Phase Readiness

- AttachmentCrypto ready for use in file upload/download flows
- Encryption key returned for inclusion in E2E encrypted AttachmentPointer
- Crypto layer complete (Phases 10-15 done)
- Ready for Phase 16 (Network layer begins)

---
*Phase: 15-crypto-attachments*
*Completed: 2026-02-03*
