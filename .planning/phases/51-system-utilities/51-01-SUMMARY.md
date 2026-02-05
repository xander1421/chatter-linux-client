---
phase: 51-system-utilities
plan: 01
subsystem: system
tags: [arboard, clipboard, wayland, xdg-open, autostart, desktop-entry]

# Dependency graph
requires:
  - phase: 49-system-keyring-directories
    provides: XDG directories, SystemError pattern
  - phase: 50-system-tray-notifications
    provides: Stateless service patterns
provides:
  - Clipboard with Wayland-native text and image operations
  - ExternalOpener for URLs, files, and folders
  - AutostartManager for XDG autostart entries
affects: [ui-settings, file-handling, app-lifecycle]

# Tech tracking
tech-stack:
  added:
    - arboard (3.x with wayland-data-control feature)
  patterns:
    - Stateless utility structs (ExternalOpener)
    - Per-operation instance creation (Clipboard)
    - XDG desktop entry management

key-files:
  created: []
  modified:
    - src/system/clipboard.rs
    - src/system/open.rs
    - src/system/autostart.rs
    - src/system/mod.rs
    - src/error/system_error.rs
    - Cargo.toml

key-decisions:
  - "arboard with wayland-data-control for native Wayland clipboard support (auto-fallback to X11)"
  - "ExternalOpener is stateless (struct as namespace, all methods independent)"
  - "AutostartManager uses dirs crate for config dir (independent of Directories struct)"
  - "URL validation limited to http/https/mailto for security"

patterns-established:
  - "Desktop file management pattern for XDG autostart"

issues-created: []

# Metrics
duration: 5min
completed: 2026-02-05
---

# Phase 51 Plan 01: System Utilities Summary

**Clipboard with Wayland support via arboard, ExternalOpener for URLs/files, AutostartManager for XDG .desktop entries**

## Performance

- **Duration:** 5 min
- **Started:** 2026-02-05T18:50:02Z
- **Completed:** 2026-02-05T18:55:06Z
- **Tasks:** 3
- **Files modified:** 6

## Accomplishments

- Implemented Clipboard with native Wayland support (arboard with wayland-data-control feature)
- Implemented ExternalOpener for opening URLs, files, and folders via xdg-open
- Implemented AutostartManager for XDG autostart .desktop file management
- Added 7 new SystemError variants for clipboard, open, and autostart operations
- Added 20 unit tests (all passing, 5 ignored for CI environments)

## Task Commits

Each task was committed atomically:

1. **Task 1: Implement Clipboard with arboard** - `4e95f8a` (feat)
2. **Task 2: Implement ExternalOpener** - `2dc0b9e` (feat)
3. **Task 3: Implement AutostartManager** - `554d3d3` (feat)

## Files Created/Modified

- `Cargo.toml` - Added arboard dependency with wayland-data-control feature
- `src/system/clipboard.rs` - Clipboard struct with get/set text and image, ImageData struct
- `src/system/open.rs` - ExternalOpener with open_url, open_file, open_folder, reveal_in_folder
- `src/system/autostart.rs` - AutostartManager with is_enabled, enable, disable, toggle
- `src/system/mod.rs` - Updated exports for Clipboard, ImageData, ExternalOpener, AutostartManager
- `src/error/system_error.rs` - Added ClipboardInit/Read/Write, OpenUrl/File, InvalidUrl, Autostart variants

## Decisions Made

- **arboard over wl-clipboard-rs**: arboard provides unified API with automatic Wayland/X11 fallback
- **Stateless ExternalOpener**: All methods could be standalone functions, struct provides namespace
- **URL scheme whitelist**: Only http/https/mailto allowed for security (no file:// or ftp://)
- **AutostartManager uses dirs crate**: Independent of Directories struct for flexibility

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None.

## Next Phase Readiness

- System integration layer complete
- Ready for Phase 52 (Media File Handling) which can use ExternalOpener for file preview
- Clipboard available for copy/paste in future UI phases
- Autostart settings available for UI settings screen

---
*Phase: 51-system-utilities*
*Completed: 2026-02-05*
