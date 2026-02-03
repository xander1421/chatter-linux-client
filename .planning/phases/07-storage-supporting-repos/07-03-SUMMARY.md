---
phase: 07-storage-supporting-repos
plan: 03
subsystem: database
tags: [rusqlite, sqlcipher, drafts, upsert, migrations]

# Dependency graph
requires:
  - phase: 07-02
    provides: Repository pattern, JSON TEXT for arrays
provides:
  - V007Drafts migration for drafts table
  - DraftRepository with CRUD, key-based lookup, cleanup
affects: [message-compose, channel-ui, sync-service]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - COALESCE in unique index for NULL handling
    - Upsert with ON CONFLICT on expression index
    - Key-based lookup pattern

key-files:
  created:
    - src/storage/migrations/v007_drafts.rs
  modified:
    - src/storage/migrations/mod.rs
    - src/storage/repositories/draft_repo.rs
    - src/storage/repositories/mod.rs
    - src/storage/mod.rs

key-decisions:
  - "COALESCE(thread_id, '') for unique index (SQLite NULL handling)"
  - "Upsert instead of separate insert/update (simpler API)"
  - "delete_empty_drafts for cleanup (trim + empty attachments check)"

patterns-established:
  - "Expression-based unique index with COALESCE for nullable columns"

issues-created: []

# Metrics
duration: 3min
completed: 2026-02-03
---

# Phase 7 Plan 03: Draft Tables Summary

**V007 drafts migration with DraftRepository supporting upsert, key-based lookup, and cleanup operations**

## Performance

- **Duration:** 3 min
- **Started:** 2026-02-03T14:25:00Z
- **Completed:** 2026-02-03T14:28:00Z
- **Tasks:** 3
- **Files modified:** 5

## Accomplishments

- V007Drafts migration creates drafts table with COALESCE-based unique index
- DraftRepository with upsert for insert/update semantics
- Key-based lookup (get_by_key, delete_by_key) for channel+thread drafts
- delete_empty_drafts cleanup method for housekeeping

## Task Commits

Each task was committed atomically:

1. **Task 1: Create V007 migration** - `39e61ba` (feat)
2. **Task 2: Implement DraftRepository CRUD** - `f8a8d17` (feat)
3. **Task 3: Add DraftRepository re-export** - `339f69b` (feat)

**Plan metadata:** (this commit) (docs: complete plan)

## Files Created/Modified

- `src/storage/migrations/v007_drafts.rs` - V007 drafts table migration
- `src/storage/migrations/mod.rs` - Register V007Drafts
- `src/storage/repositories/draft_repo.rs` - Full DraftRepository implementation
- `src/storage/repositories/mod.rs` - Re-export DraftRepository
- `src/storage/mod.rs` - Top-level re-export

## Decisions Made

- Used COALESCE(thread_id, '') in unique index to handle NULL thread_id (SQLite treats NULLs as distinct)
- Upsert API instead of separate insert/update for simpler usage (drafts identified by key)
- delete_empty_drafts checks both TRIM(content) = '' AND attachments_json = '[]'

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None

## Next Phase Readiness

- DraftRepository ready for message compose service integration
- Ready for 07-04-PLAN.md (Settings table + repository)

---
*Phase: 07-storage-supporting-repos*
*Completed: 2026-02-03*
