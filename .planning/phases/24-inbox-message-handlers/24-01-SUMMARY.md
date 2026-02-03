---
phase: 24-inbox-message-handlers
plan: 01
subsystem: inbox
tags: [signal-protocol, protobuf, prost, async-trait, broadcast-channels]

# Dependency graph
requires:
  - phase: 23-inbox-provider-poller
    provides: ProcessedMessage, ContentType, MessageProcessor
provides:
  - InboxHandler trait for message processing
  - HandlerContext with broadcast event emission
  - DomainEvent enum for UI/storage consumption
  - ChatHandler for DataMessage parsing
  - AttachmentHandler for standalone attachments
  - HandlerResult, HandlerOutcome, HandlerError types
affects: [24-02-reaction-receipt-handlers, 24-03-sync-typing-handlers, storage, ui]

# Tech tracking
tech-stack:
  added: []
  patterns: [stateless-handler-pattern, sha256-deterministic-uuid, broadcast-event-emission]

key-files:
  created:
    - src/inbox/handlers/mod.rs (handler infrastructure)
    - src/inbox/handlers/chat.rs (ChatHandler implementation)
    - src/inbox/handlers/attachment.rs (AttachmentHandler implementation)
  modified: []

key-decisions:
  - "Use SHA256-based deterministic UUIDs instead of uuid v5 (v5 feature not enabled)"
  - "Stateless handlers - all context passed via HandlerContext parameter"
  - "DomainEvent uses broadcast channels (capacity 64) for multi-subscriber event emission"
  - "Message IDs derived from timestamps using Signal convention (author, timestamp tuple)"

patterns-established:
  - "Stateless handler pattern: no struct fields, all state via HandlerContext"
  - "Proto parsing pattern: decode Content -> match oneof variant -> build domain type"
  - "Deterministic UUID pattern: SHA256 hash first 16 bytes with version/variant bits set"

issues-created: []

# Metrics
duration: 18min
completed: 2026-02-03
---

# Phase 24 Plan 01: ChatHandler & AttachmentHandler Summary

**Inbox handler infrastructure with ChatHandler for DataMessage and AttachmentHandler for standalone attachments, using stateless trait pattern with broadcast event emission**

## Performance

- **Duration:** 18 min
- **Started:** 2026-02-03T00:00:00Z
- **Completed:** 2026-02-03T00:18:00Z
- **Tasks:** 3
- **Files modified:** 3

## Accomplishments
- Defined InboxHandler trait with async handle() method and content_types() selector
- Implemented HandlerContext with broadcast channel for DomainEvent emission
- ChatHandler parses DataMessage proto including text, attachments, quotes, deletes, AI mentions
- AttachmentHandler parses standalone AttachmentMessage for file metadata
- Comprehensive test coverage (21 tests) for both handlers

## Task Commits

Each task was committed atomically:

1. **Task 1: Define handler traits and common types** - `3130469` (feat)
2. **Task 2: Implement ChatHandler for DataMessage** - `a2785f6` (feat)
3. **Task 3: Implement AttachmentHandler and tests** - `193fd2d` (feat)

## Files Created/Modified
- `src/inbox/handlers/mod.rs` - Handler trait, context, result types, DomainEvent enum with 12 variants
- `src/inbox/handlers/chat.rs` - ChatHandler for DataMessage with delete handling
- `src/inbox/handlers/attachment.rs` - AttachmentHandler for standalone attachments

## Decisions Made
- Used SHA256-based deterministic UUIDs instead of Uuid::new_v5 since the v5 feature is not enabled in Cargo.toml
- Message IDs derived from timestamps following Signal convention where messages are identified by (author, timestamp) tuple
- DomainEvent includes comprehensive variants for all message lifecycle events (received, edited, deleted, attachment ready, typing, receipts, reactions, pins)

## Deviations from Plan

None - plan executed exactly as written

## Issues Encountered
- UUID v5 not available - resolved by implementing SHA256-based deterministic UUID generation
- Datelike trait not in scope for test assertions - resolved by adding use chrono::Datelike

## Next Phase Readiness
- Handler infrastructure complete, ready for Phase 24-02 (ReactionHandler, ReceiptHandler)
- DomainEvent enum extensible for additional event types
- Stateless handler pattern established for remaining handlers

---
*Phase: 24-inbox-message-handlers*
*Completed: 2026-02-03*
