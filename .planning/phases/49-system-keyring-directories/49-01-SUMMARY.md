---
phase: 49-system-keyring-directories
plan: 01
subsystem: system
tags: [xdg, keyring, directories, secret-service, credentials]

# Dependency graph
requires:
  - phase: 05
    provides: Database::open(path, passphrase) interface
provides:
  - XDG-compliant directory management
  - Secure keyring credential storage
  - Database path resolution
  - SystemError for system operations
affects: [bootstrap, database-init, logout]

# Tech tracking
tech-stack:
  added: []  # directories, keyring already in Cargo.toml
  patterns: [xdg-base-directories, secret-service-api]

key-files:
  created:
    - src/system/directories.rs
    - src/error/system_error.rs
  modified:
    - src/system/keyring.rs
    - src/system/mod.rs
    - src/error/mod.rs

key-decisions:
  - "Use directories crate ProjectDirs (not BaseDirs/UserDirs) for app-specific paths"
  - "Runtime directory is optional (best-effort creation)"
  - "Keyring entries created per-operation (not cached in struct)"
  - "Keyring tests marked #[ignore] for CI environments without Secret Service"

patterns-established:
  - "XDG directory pattern: com.chatter.chatter-linux qualifier"
  - "Keyring service/username pattern: chatter-linux/database"

issues-created: []

# Metrics
duration: 2min
completed: 2026-02-05
---

# Phase 49 Plan 01: System Keyring & Directories Summary

**XDG-compliant directories and Secret Service keyring integration for database passphrase storage**

## Performance

- **Duration:** 2 min
- **Started:** 2026-02-05T18:33:17Z
- **Completed:** 2026-02-05T18:35:22Z
- **Tasks:** 3
- **Files modified:** 5

## Accomplishments

- Implemented Directories struct with XDG-compliant paths (config, data, cache, runtime)
- Implemented Keyring struct using Secret Service API for secure credential storage
- Added SystemError type for system integration errors
- Added convenience methods for database_path() and logs_dir()
- Created unit tests (3 passing, 2 ignored for CI)

## Task Commits

Each task was committed atomically:

1. **Task 1: Implement Directories module** - `8e938f1` (feat)
2. **Task 2: Implement Keyring module** - `b329021` (feat)
3. **Task 3: Update mod.rs and add tests** - `3d852d2` (feat)

## Files Created/Modified

- `src/system/directories.rs` - XDG directory management with ProjectDirs
- `src/system/keyring.rs` - Secret Service keyring integration
- `src/system/mod.rs` - Module re-exports for Directories and Keyring
- `src/error/system_error.rs` - SystemError enum for system operations
- `src/error/mod.rs` - Added system_error module and re-export

## Decisions Made

- **ProjectDirs over BaseDirs:** Using app-specific ProjectDirs (com.chatter.chatter-linux) rather than general BaseDirs/UserDirs for proper XDG compliance
- **Optional runtime directory:** Runtime dir (/run/user/{uid}/) created with best-effort, not required
- **Lazy keyring entries:** keyring::Entry created per-operation rather than stored in struct (recommended pattern)
- **CI-safe tests:** Keyring tests marked `#[ignore]` since they require a running Secret Service daemon

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None

## Next Phase Readiness

- System integration foundation complete
- Ready for Phase 50 (System Tray & Notifications) which depends on these modules
- Database bootstrap can now use Directories::database_path() and Keyring::get_passphrase()

---
*Phase: 49-system-keyring-directories*
*Completed: 2026-02-05*
