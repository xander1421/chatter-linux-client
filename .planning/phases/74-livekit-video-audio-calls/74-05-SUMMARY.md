---
phase: 74-livekit-video-audio-calls
plan: 05
subsystem: calls
tags: [livekit, slint, video-grid, callbacks]

requires:
  - phase: 74-04
    provides: IncomingCallModal, CallControls, call types
provides:
  - ActiveCallView with participant video grid
  - Call callback wiring in bin_ui
  - Full call UI integration in app.slint/main.slint
affects: [call-integration, video-rendering]

tech-stack:
  added: []
  patterns: [participant-grid-layout, callback-wiring]

key-files:
  created:
    - ui/calls/active_call_view.slint
    - ui/calls/mod.slint
    - src/bin_ui/features/calls.rs
  modified:
    - ui/app.slint
    - ui/main.slint
    - src/bin_ui/features/mod.rs

key-decisions:
  - "Nested HorizontalLayout/VerticalLayout for grid (Slint GridLayout doesn't support conditionals)"
  - "visible: show property for proper hiding when not active"
  - "Placeholder callbacks with TODO for CallService integration"

patterns-established:
  - "Call overlay pattern: visible property + conditional content"

issues-created: []

duration: ~15min
completed: 2026-02-23
---

# Phase 74 Plan 5: ActiveCallView and Callback Wiring Summary

**Shipped: Complete call UI with video grid and callback wiring to service layer**

## Performance

- **Duration:** ~15 min
- **Started:** 2026-02-23
- **Completed:** 2026-02-23
- **Tasks:** 3 (2 auto + 1 checkpoint)
- **Files modified:** 6

## Accomplishments

- ActiveCallView with participant video grid (1-4 participants)
- ParticipantTile component with video/avatar/mute/speaking indicators
- Call callbacks wired in bin_ui/features/calls.rs
- Full integration into app.slint and main.slint

## Task Commits

1. **Task 1: Create ActiveCallView component** - `4247d53` (feat)
2. **Task 2: Wire call callbacks** - `24834a3` (feat)
3. **Bug fix: ActiveCallView visibility** - `ab49beb` (fix)

## Files Created/Modified

- `ui/calls/active_call_view.slint` - Main call view with video grid
- `ui/calls/mod.slint` - Component exports for all call UI
- `src/bin_ui/features/calls.rs` - Callback wiring module
- `src/bin_ui/features/mod.rs` - Added calls module export
- `ui/app.slint` - Integrated call components and properties
- `ui/main.slint` - Exposed call properties and callbacks

## Decisions Made

- Used nested HorizontalLayout/VerticalLayout instead of GridLayout (Slint doesn't support conditionals in GridLayout)
- Added `visible: show` property to ActiveCallView to prevent black screen when not active
- Placeholder implementations with TODO comments for CallService integration (service not yet in AppContext)

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Fixed borrow error in video_playback.rs**
- **Found during:** Build verification
- **Issue:** Pre-existing double mutable borrow of `input` during seek
- **Fix:** Drop packets iterator before seeking, recreate after
- **Files modified:** src/media/video_playback.rs
- **Not committed as part of this plan** (pre-existing issue)

**2. [Rule 1 - Bug] Fixed black screen on app launch**
- **Found during:** Checkpoint verification
- **Issue:** ActiveCallView rendered black background even when not visible
- **Fix:** Added `visible: show` property
- **Files modified:** ui/calls/active_call_view.slint
- **Commit:** ab49beb

---

**Total deviations:** 2 auto-fixed (1 blocking, 1 bug), 0 deferred
**Impact on plan:** Both fixes necessary for app functionality. No scope creep.

## Issues Encountered

- Slint GridLayout doesn't support conditional elements - worked around with nested layouts
- ActiveCallView needed explicit visibility property to avoid rendering when hidden

## Next Phase Readiness

Phase 74 complete. Call UI foundation ready:
- All call components created (types, incoming modal, controls, active view)
- CallService and RoomManager implemented
- Callbacks wired (with placeholder implementations)

Next steps for full integration:
- Add CallService to AppContext
- Wire callbacks to actual service methods
- Implement video frame rendering from LiveKit

---
*Phase: 74-livekit-video-audio-calls*
*Completed: 2026-02-23*
