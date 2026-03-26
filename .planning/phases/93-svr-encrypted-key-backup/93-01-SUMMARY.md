---
phase: 93-svr-encrypted-key-backup
plan: 01
subsystem: api, database
tags: [protobuf, connectrpc, postgres, svr, backup, encryption]

# Dependency graph
requires:
  - phase: 92-call-orchestration-to-core
    provides: stable core architecture for adding new server services
provides:
  - svr.proto with StoreBackup/FetchBackup/DeleteBackup RPCs
  - Migration 000028 (svr_backups + svr_fetch_attempts tables)
  - Go domain types (SvrBackup, FetchAttemptState, error sentinels)
  - Generated ConnectRPC Go service interfaces
affects: [93-02, 93-03, 93-04, 93-05]

# Tech tracking
tech-stack:
  added: []
  patterns: [SVR zero-knowledge backup storage, HMAC-derived backup_id_hash for server-side indexing]

key-files:
  created:
    - pkg/proto/chat/v1/svr/svr.proto
    - scripts/migrations/000028_create_svr_backups.up.sql
    - scripts/migrations/000028_create_svr_backups.down.sql
    - internal/services/svr/types/types.go
    - pkg/proto/chat/v1/svr/svr.pb.go
    - pkg/proto/chat/v1/svr/chatsvrconnect/svr.connect.go
  modified: []

key-decisions:
  - "backup_id_hash (HMAC-SHA256) used as server-side index — server never sees PIN or SVR secret"
  - "Single active backup per user (user_id PRIMARY KEY), no version history"
  - "Rate limiting via separate svr_fetch_attempts table with lockout support"
  - "min_len 48 for encrypted_backup (12 nonce + 16 tag + 20 min payload)"

patterns-established:
  - "SVR proto pattern: zero-knowledge storage with HMAC-derived identifiers"
  - "Rate limit table pattern: separate attempts table with lockout timestamp"

issues-created: []

# Metrics
duration: 5min
completed: 2026-03-27
---

# Phase 93-01: SVR Proto Service and Database Schema Summary

**SVR proto with StoreBackup/FetchBackup/DeleteBackup RPCs, migration 000028 with rate-limited fetch attempts, and Go domain types**

## Performance

- **Duration:** 5 min
- **Started:** 2026-03-27
- **Completed:** 2026-03-27
- **Tasks:** 2
- **Files created:** 6

## Accomplishments
- SvrService proto with 3 RPCs and buf/validate annotations on all byte fields
- PostgreSQL migration with svr_backups (1:1 per user) and svr_fetch_attempts tables
- Go domain types with rate limit constants and error sentinels
- Generated ConnectRPC service/handler interfaces

## Task Commits

Each task was committed atomically:

1. **Task 1: Create svr.proto with backup RPCs and messages** - `d319c38` (feat)
2. **Task 2: Create migration 000028 and Go domain types** - `b192489` (feat)

**Generated code:** `f03ef4a` (chore: generate proto code for SVR service)

## Files Created/Modified
- `pkg/proto/chat/v1/svr/svr.proto` - SvrService with 3 RPCs and validation annotations
- `scripts/migrations/000028_create_svr_backups.up.sql` - svr_backups + svr_fetch_attempts tables
- `scripts/migrations/000028_create_svr_backups.down.sql` - Drop tables
- `internal/services/svr/types/types.go` - SvrBackup, FetchAttemptState, constants, errors
- `pkg/proto/chat/v1/svr/svr.pb.go` - Generated protobuf Go code
- `pkg/proto/chat/v1/svr/chatsvrconnect/svr.connect.go` - Generated ConnectRPC interfaces

## Decisions Made
- backup_id_hash (HMAC-SHA256 of SVR secret) as the server-side lookup index — server never learns PIN
- Encrypted backup min_len 48 bytes (12 nonce + 16 tag + 20 min payload) via buf/validate
- Separate svr_fetch_attempts table for rate limiting (cleaner than embedding in svr_backups)

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Rust proto code not generated**
- **Found during:** Final verification (buf generate)
- **Issue:** Buf API token expired, preventing remote plugin execution for Rust code gen
- **Fix:** Go code generated successfully via local protoc; Rust generation deferred to when buf token is refreshed
- **Impact:** Client-side plans (93-03+) will need generated Rust proto before starting

**2. [Deviation] First commit included 3 pre-staged files**
- **Found during:** Task 1 commit
- **Issue:** 3 docs files were already staged in the server repo before plan execution
- **Fix:** Acknowledged; files were unrelated docs changes already staged by previous work
- **Impact:** None — the svr.proto was correctly included

## Issues Encountered
- Pre-existing buf lint warnings from `signal/v1/sealed_sender.proto` (corrupted comment) and temporalio venv duplicate symbols — unrelated to new code

## Next Phase Readiness
- Proto contract established for all 5 plans to build against
- Go ConnectRPC interfaces ready for 93-02 (server service implementation)
- Rust proto generation needed before 93-03 (refresh buf token)

---
*Phase: 93-svr-encrypted-key-backup*
*Completed: 2026-03-27*
