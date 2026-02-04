---
phase: 29-mailbox-privacy-features
plan: "02"
subsystem: mailbox
tags: [key-rolling, forward-secrecy, ed25519, rotation, expiry]

# Dependency graph
requires:
  - phase: 27-mailbox-dm-conversations
    provides: DmConversationManager, DmMailbox types
  - phase: 28-mailbox-group-fanout
    provides: GroupMailboxManager, GroupMailbox types
provides:
  - KeyRollingService for mailbox rotation
  - RotationTarget enum (DM/Group)
  - RotationNotification for peer key exchange
  - Automatic expiry detection and rotation
affects: [cover-traffic, mailbox-polling, privacy-features]

# Tech tracking
tech-stack:
  added: []
  patterns: [overlap-period-deletion, rotation-threshold-detection]

key-files:
  created: []
  modified:
    - src/mailbox/key_rolling.rs
    - src/mailbox/mod.rs

key-decisions:
  - "24-hour rotation threshold (rotate when mailbox expires within 24 hours)"
  - "1-hour overlap period (old mailbox stays valid briefly for in-flight messages)"
  - "PendingDeletion struct tracks old mailboxes awaiting cleanup"
  - "RwLock for pending_deletions (async-safe concurrent access)"

patterns-established:
  - "Rotation returns RotationNotification for caller to send to peers"
  - "Separate get_candidates() from rotate() for flexible scheduling"

issues-created: []

# Metrics
duration: 2min
completed: 2026-02-04
---

# Phase 29 Plan 02: Key Rolling Summary

**KeyRollingService for automated mailbox rotation with overlap period for in-flight message handling**

## Performance

- **Duration:** 2 min
- **Started:** 2026-02-04T15:44:39Z
- **Completed:** 2026-02-04T15:46:47Z
- **Tasks:** 3
- **Files modified:** 2

## Accomplishments

- Implemented KeyRollingService with DM and group mailbox rotation
- Added rotation candidate detection (24-hour threshold before expiry)
- Implemented overlap period handling (1-hour grace for in-flight messages)
- Created RotationNotification struct for peer key exchange
- Exported key_rolling types from mailbox module

## Task Commits

All tasks were completed in a single commit (tasks 1-3 were closely integrated):

1. **Task 1-3: Define types and implement rotation** - `1ad0aa3` (feat)

**Plan metadata:** (this commit)

## Files Created/Modified

- `src/mailbox/key_rolling.rs` - KeyRollingService with rotation logic and types
- `src/mailbox/mod.rs` - Export key_rolling types

## Decisions Made

- Used 24-hour rotation threshold (rotate mailboxes expiring within 24 hours)
- Used 1-hour overlap period (old mailbox remains briefly for in-flight messages)
- PendingDeletion uses RwLock for async-safe access
- Rotation returns notification for caller to handle peer communication

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None.

## Next Phase Readiness

- Key rolling infrastructure ready
- Ready for 29-03 (cover traffic) to integrate with rotation
- RotationNotification can be used by message handlers to announce new mailbox addresses

---
*Phase: 29-mailbox-privacy-features*
*Completed: 2026-02-04*
