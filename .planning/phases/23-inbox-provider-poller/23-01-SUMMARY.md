---
phase: 23-inbox-provider-poller
plan: 01
subsystem: inbox
tags: [polling, ed25519, broadcast, events, mailbox]

# Dependency graph
requires:
  - phase: 19
    provides: MailboxClient with 7 RPCs (poll, challenge, acknowledge)
  - phase: 22
    provides: MessageValidationPipeline (dedup, validate, order)
provides:
  - DeviceInboxProvider coordinator with start/stop lifecycle
  - InboxPoller with challenge-response Ed25519 authentication
  - InboxConfig with builder pattern for polling configuration
  - InboxEvent broadcast for message lifecycle monitoring
  - InboxStats for performance metrics
affects: [24-inbox-handlers, 25-inbox-sync, services-realtime]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "Adaptive polling intervals (active vs idle)"
    - "Challenge-response authentication with Ed25519"
    - "broadcast channel for events (capacity 64)"
    - "watch channel for shutdown coordination"

key-files:
  created: []
  modified:
    - src/inbox/provider.rs
    - src/inbox/poller.rs
    - src/inbox/mod.rs

key-decisions:
  - "Event channel capacity 64 for inbox events"
  - "Adaptive polling: 5s active, 30s idle, 2-min activity threshold"
  - "Ed25519 signing for challenge-response auth"
  - "Batch acknowledgment with configurable batch size"

patterns-established:
  - "DeviceInboxProvider as main coordinator pattern"
  - "InboxPoller run_once() returns Option for shutdown"
  - "Process messages through validation pipeline before emitting events"

issues-created: []

# Metrics
duration: 4min
completed: 2026-02-03
---

# Phase 23 Plan 01: Inbox Provider & Poller Summary

**DeviceInboxProvider coordinator with InboxPoller using Ed25519 challenge-response auth, configurable polling intervals, and broadcast event system**

## Performance

- **Duration:** 4 min
- **Started:** 2026-02-03T19:45:51Z
- **Completed:** 2026-02-03T19:50:06Z
- **Tasks:** 3
- **Files modified:** 3

## Accomplishments

- InboxConfig with builder pattern for poll intervals (5s/30s), batch sizes (100), retry attempts (3), and ack batch size (50)
- InboxEvent enum with 6 variants for monitoring message lifecycle (MessageReceived, MessageProcessed, MessageFailed, PollCompleted, PollFailed, AcknowledgeFailed)
- InboxState enum for tracking polling state transitions (Idle → Polling → Processing → Stopped)
- InboxStats struct for performance metrics (messages received/processed/failed, last poll time/count)
- InboxPoller with challenge-response Ed25519 authentication and adaptive polling intervals
- DeviceInboxProvider coordinator with start/stop lifecycle, event broadcasting, and validation pipeline integration

## Task Commits

Each task was committed atomically:

1. **Task 1: InboxConfig and InboxEvent types** - `f628da5` (feat)
2. **Task 2: InboxPoller with polling loop** - `65834cf` (feat)
3. **Task 3: DeviceInboxProvider coordinator** - included in Task 1 (same file)

## Files Created/Modified

- `src/inbox/provider.rs` - InboxConfig, InboxConfigBuilder, InboxEvent, InboxState, InboxStats, InboxError, DeviceInboxProvider
- `src/inbox/poller.rs` - InboxPoller with challenge-response auth and adaptive polling
- `src/inbox/mod.rs` - Module documentation and re-exports

## Decisions Made

- **Event channel capacity 64** - Balance between memory and not losing events during burst
- **Adaptive polling intervals** - 5s when active (messages in last 2 min), 30s when idle
- **Ed25519 challenge-response** - Matches Signal protocol for mailbox authentication
- **Validation before events** - Messages pass through MessageValidationPipeline before MessageReceived is emitted

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None.

## Next Phase Readiness

- Ready for Phase 23-02 (if exists) or Phase 24 (Inbox Message Handlers)
- DeviceInboxProvider emits MessageReceived events for handlers to process
- Handlers will decrypt sealed sender envelopes and process message content

---
*Phase: 23-inbox-provider-poller*
*Completed: 2026-02-03*
