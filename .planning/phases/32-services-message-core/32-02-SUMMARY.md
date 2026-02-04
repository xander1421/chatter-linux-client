---
phase: 32-services-message-core
plan: 02
subsystem: services
tags: [message, grpc, crud, local-storage, domain-mapping]

# Dependency graph
requires:
  - phase: 32-01
    provides: MessageService base with send_message, error types, proto conversion
  - phase: 18-network-channel-message-clients
    provides: MessageClient.edit_message, delete_message, get_messages
  - phase: 06-storage-core-repositories
    provides: MessageRepository.get, update, insert
provides:
  - MessageService.edit_message with ownership validation
  - MessageService.delete_message with soft-delete pattern
  - MessageService.get_messages with server-first fetch
  - proto_to_domain_message_with_sender helper for optional sender_id
affects: [33-services-message-advanced, 34-services-ephemeral]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - Ownership validation before edit/delete (check sender_id matches current user)
    - Soft delete pattern (mark_deleted instead of hard delete)
    - Server-first fetch with local storage upsert

key-files:
  created: []
  modified:
    - src/services/message_service.rs

key-decisions:
  - "Soft delete for message deletion (keep for history/sync)"
  - "Server-first fetch strategy for get_messages (always fetch from server, then upsert locally)"
  - "Ownership validation allows edit/delete if sender_id is None (sealed sender messages)"
  - "Upsert pattern for storing fetched messages (insert if new, update if exists)"

patterns-established:
  - "Ownership validation: if sender_id.is_some() && sender_id != current_user, return NotAuthor"
  - "proto_to_domain_message_with_sender for messages where sender comes from proto"

issues-created: []

# Metrics
duration: 6min
completed: 2026-02-05
---

# Phase 32 Plan 02: Edit/Delete/Get Messages Summary

**Edit, delete, and get message operations with ownership validation, soft-delete, and server-first fetch**

## Performance

- **Duration:** 6 min
- **Started:** 2026-02-05T17:00:00Z
- **Completed:** 2026-02-05T17:06:00Z
- **Tasks:** 3
- **Files modified:** 1

## Accomplishments

- Implemented edit_message with ownership validation and local storage update
- Implemented delete_message with soft-delete pattern (mark_deleted, not hard delete)
- Implemented get_messages with server-first fetch and local upsert storage
- Added proto_to_domain_message_with_sender helper for messages from server list

## Task Commits

Combined commit for all three tasks (tightly coupled operations in same file):

1. **Tasks 1-3: edit_message, delete_message, get_messages** - `eb40936` (feat)

**Plan metadata:** (this commit)

## Files Created/Modified

- `src/services/message_service.rs` - Added edit_message, delete_message, get_messages, proto_to_domain_message_with_sender

## Decisions Made

- **Soft delete for messages:** Use mark_deleted() instead of hard delete via message_repo.delete(). Keeps messages for history/sync.
- **Server-first fetch:** Always fetch from server in get_messages for v1. Local cache is for offline access.
- **Ownership validation with None bypass:** If sender_id is None (sealed sender), allow edit/delete. Server will enforce actual ownership.
- **Upsert pattern:** Check if message exists in local DB before insert to avoid conflicts.

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None

## Next Phase Readiness

- Phase 32 (Services Message Core) complete
- Full MessageService API: send_message, edit_message, delete_message, get_messages
- Ready for Phase 33: Services Message Advanced (forward, quote, pin, thread, draft)
- Patterns established for ownership validation and soft-delete

---
*Phase: 32-services-message-core*
*Completed: 2026-02-05*
