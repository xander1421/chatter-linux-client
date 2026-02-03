# Phase 4 Plan 01: Draft and Ephemeral Entities Summary

**Implemented per-channel draft persistence and self-destructing message support with TTL lifecycle management.**

## Accomplishments

- Added `DraftId` to the strongly-typed ID system using `impl_uuid_id!` macro
- Implemented `DraftKey` for uniquely identifying drafts by channel and optional thread context
- Implemented `Draft` entity with full support for:
  - Per-channel and per-thread draft persistence
  - Reply-to context preservation
  - Pending attachment tracking
  - Mention detection and management
  - Word count calculation
- Enhanced `MessageTTL` enum with duration conversion, parsing, and display methods
- Implemented `EphemeralState` enum for tracking message lifecycle (Pending, Countdown, Expired)
- Implemented `EphemeralMessage` entity with:
  - TTL-based countdown management
  - Read-time expiration calculation
  - State computation and remaining time tracking
  - Human-readable countdown display formatting

## Files Created/Modified

- `src/domain/ids.rs` - Added `DraftId` to UUID-based ID types
- `src/domain/draft.rs` - Full implementation of `Draft` and `DraftKey` with 10 unit tests
- `src/domain/ephemeral.rs` - Full implementation of `EphemeralMessage`, `EphemeralState`, and enhanced `MessageTTL` with 14 unit tests
- `src/domain/mod.rs` - Added re-exports for `Draft`, `DraftKey`, `DraftId`, `EphemeralMessage`, `EphemeralState`, `MessageTTL`

## Decisions Made

1. **DraftKey as HashMap-compatible**: Implemented `Hash` and `Eq` traits for `DraftKey` to enable use as HashMap keys for efficient draft lookup
2. **Idempotent mark_read**: `EphemeralMessage::mark_read()` is idempotent - calling it multiple times does not reset the countdown
3. **Countdown starts on read**: Expiration time is recalculated when the recipient reads the message, not when sent, matching Signal's behavior
4. **Empty draft detection**: `is_empty()` checks both trimmed content and attachments, allowing whitespace-only drafts to be considered empty

## Issues Encountered

None - implementation followed established patterns from Phase 2/3 without issues.

## Verification Results

- `cargo check --lib` - Passed
- `cargo clippy --lib` - Passed (no Rust warnings)
- `cargo test domain::draft` - 10 tests passed
- `cargo test domain::ephemeral` - 14 tests passed

## Next Step

Ready for 04-02-PLAN.md (EncryptionStatus, KeyInfo, PrivacySettings entities)
