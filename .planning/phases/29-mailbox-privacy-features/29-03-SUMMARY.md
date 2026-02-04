---
phase: 29-mailbox-privacy-features
plan: 03
subsystem: mailbox
tags: [cover-traffic, timing-obfuscation, privacy, exponential-distribution]

# Dependency graph
requires:
  - phase: 29-01
    provides: ContentEncoder for padding
  - phase: 27-03
    provides: DmConversationManager for peer discovery
provides:
  - CoverTrafficGenerator for timing obfuscation
  - CoverTrafficConfig for opt-in configuration
  - CoverTrafficMessage for decoupled message production
  - CoverTrafficStats for monitoring
affects: [services-ephemeral, app-bootstrap]

# Tech tracking
tech-stack:
  added: []
  patterns: [exponential-distribution-timing, callback-based-async-send]

key-files:
  created: []
  modified:
    - src/mailbox/cover_traffic.rs
    - src/mailbox/mod.rs

key-decisions:
  - "Exponential distribution for random intervals (more realistic than uniform)"
  - "Callback-based send function to decouple from network layer"
  - "Disabled by default (user opt-in for privacy vs bandwidth tradeoff)"
  - "64-byte dummy content size (matches typical short messages)"

patterns-established:
  - "Callback injection for async network operations"
  - "Statistics tracking with RwLock for async access"

issues-created: []

# Metrics
duration: 2min
completed: 2026-02-04
---

# Phase 29 Plan 03: Cover Traffic Summary

**CoverTrafficGenerator with exponential random intervals and callback-based message sending for timing obfuscation**

## Performance

- **Duration:** 2 min
- **Started:** 2026-02-04T15:50:39Z
- **Completed:** 2026-02-04T15:53:06Z
- **Tasks:** 3
- **Files modified:** 2

## Accomplishments

- CoverTrafficGenerator that produces dummy messages at random intervals
- Exponential distribution for realistic timing patterns (bursts + quiet periods)
- Callback-based design decouples from network layer
- Disabled by default with opt-in configuration
- Same ContentEncoder padding as real messages (indistinguishable)

## Task Commits

1. **Tasks 1-3: Cover traffic implementation** - `3b6079f` (feat)

**Plan metadata:** (combined with task commit)

## Files Created/Modified

- `src/mailbox/cover_traffic.rs` - Full implementation replacing stub
- `src/mailbox/mod.rs` - Re-exports for cover traffic types

## Decisions Made

- **Exponential distribution**: More realistic than uniform timing - real messaging has bursts followed by quiet periods
- **Callback-based send**: Generator produces `CoverTrafficMessage`, caller handles actual network send - keeps generator decoupled
- **Disabled by default**: Cover traffic uses bandwidth; user must opt-in for privacy enhancement
- **64-byte dummy content**: Matches typical short message size after padding

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Fixed rand API for choose()**
- **Found during:** Task 1 (implementation)
- **Issue:** Plan used `SliceRandom` but current rand version uses `IndexedRandom`
- **Fix:** Changed import from `rand::seq::SliceRandom` to `rand::prelude::IndexedRandom`
- **Files modified:** src/mailbox/cover_traffic.rs
- **Verification:** cargo check passes
- **Committed in:** 3b6079f

**2. [Rule 2 - Missing Critical] Adapted TwoWayAnonymity interface**
- **Found during:** Task 2 (implementation)
- **Issue:** Plan's proposed code assumed `send_to_peer(&peer, &bytes)` signature but actual interface requires `envelope_type` and `mailbox_client`
- **Fix:** Changed design to use callback pattern - generator produces `CoverTrafficMessage`, caller provides send function with full interface access
- **Files modified:** src/mailbox/cover_traffic.rs
- **Verification:** Clean separation, compiles correctly
- **Committed in:** 3b6079f

---

**Total deviations:** 2 auto-fixed (1 blocking, 1 missing critical), 0 deferred
**Impact on plan:** Deviations improved design - callback pattern is more flexible and better decoupled

## Issues Encountered

None - execution proceeded smoothly after adapting to actual codebase interfaces.

## Next Phase Readiness

- Phase 29 complete
- All mailbox privacy features implemented:
  - Content padding (29-01)
  - Key rolling (29-02)
  - Cover traffic (29-03)
- Ready for Phase 30 (Services Auth)

---
*Phase: 29-mailbox-privacy-features*
*Completed: 2026-02-04*
