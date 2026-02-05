---
phase: 34-services-ephemeral-messages
plan: 01
subsystem: storage
tags: [sqlite, ephemeral, ttl, repository]

# Dependency graph
requires:
  - phase: 04-domain-privacy-entities
    provides: EphemeralMessage, MessageTTL domain types
  - phase: 06-storage-core-repositories
    provides: MessageRepository pattern, Arc<Database> DI
provides:
  - ephemeral_messages database table
  - EphemeralRepository with insert/get/mark_read/list_expired/delete
affects: [services-ephemeral-messages, inbox-message-handlers]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Separate tracking table for ephemeral state (not inline in messages)"
    - "Conditional indexes for efficient cleanup queries"

key-files:
  created:
    - src/storage/migrations/v015_ephemeral_messages.rs
    - src/storage/repositories/ephemeral_repo.rs
  modified:
    - src/storage/migrations/mod.rs
    - src/storage/repositories/mod.rs

key-decisions:
  - "Separate ephemeral_messages table (not columns on messages) for efficient cleanup queries"
  - "datetime() SQL function for expires_at recalculation on mark_read"
  - "Idempotent mark_read (only updates if countdown_started = 0)"

patterns-established:
  - "Indexed partial queries (WHERE countdown_started = 1) for cleanup"
  - "TTL fallback on read when stored seconds don't match predefined enum"

issues-created: []

# Metrics
duration: 3min
completed: 2026-02-05
---

# Phase 34 Plan 01: Ephemeral Storage Summary

**SQLite storage layer for ephemeral message tracking with migration and repository**

## Performance

- **Duration:** 3 min
- **Started:** 2026-02-05T12:00:00Z
- **Completed:** 2026-02-05T12:03:00Z
- **Tasks:** 2
- **Files modified:** 4

## Accomplishments

- Created v015_ephemeral_messages migration with indexed table
- Implemented EphemeralRepository with full CRUD operations
- mark_read() correctly recalculates expires_at from read time
- list_expired() enables efficient cleanup of expired messages

## Task Commits

Each task was committed atomically:

1. **Task 1: Create ephemeral messages migration** - `17b6f41` (feat)
2. **Task 2: Create EphemeralRepository** - `3897478` (feat)

## Files Created/Modified

- `src/storage/migrations/v015_ephemeral_messages.rs` - Migration creating ephemeral_messages table
- `src/storage/migrations/mod.rs` - Register V015EphemeralMessages in default_migrations()
- `src/storage/repositories/ephemeral_repo.rs` - EphemeralRepository with CRUD operations
- `src/storage/repositories/mod.rs` - Export EphemeralRepository

## Decisions Made

- **Separate table vs inline columns**: Kept ephemeral tracking in separate table to enable efficient cleanup queries without scanning all messages
- **Idempotent mark_read**: Only updates if countdown_started = 0, prevents repeated reads from resetting expiry
- **TTL fallback**: When stored ttl_seconds doesn't match a MessageTTL variant, falls back to closest predefined duration

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None

## Next Phase Readiness

- EphemeralRepository ready for use by EphemeralService
- Migration v015 will auto-apply on database open
- Ready for 34-02: EphemeralService business logic

---
*Phase: 34-services-ephemeral-messages*
*Completed: 2026-02-05*
