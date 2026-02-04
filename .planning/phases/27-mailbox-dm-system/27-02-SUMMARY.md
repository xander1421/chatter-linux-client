---
phase: 27-mailbox-dm-system
plan: 02
subsystem: mailbox
tags: [dm, bootstrap, sealed-sender, key-exchange, x25519, ephemeral-mailbox]

# Dependency graph
requires:
  - phase: 27-01
    provides: DmConversation, DmConversationManager, ConversationKey, DmMailbox, PeerMailbox
  - phase: 26
    provides: TokenManager, MailboxToken, blind signature infrastructure
provides:
  - DmBootstrap for initiating DM conversations
  - BootstrapResult with prepared bootstrap message
  - BootstrapResponse for handling incoming bootstrap
  - 64-byte combined key format (mailbox + conversation)
  - is_bootstrap_message() detection helper
affects: [27-03 two-way messaging, services-message, inbox-handlers]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - 64-byte combined key format for bootstrap messages
    - Token consumption during mailbox creation
    - X25519 ECDH for shared secret derivation

key-files:
  created: []
  modified:
    - src/mailbox/dm/bootstrap.rs
    - src/mailbox/dm/mod.rs
    - src/error/mailbox_error.rs

key-decisions:
  - "64-byte combined key: first 32 bytes = mailbox pk, next 32 = conversation ephemeral pk"
  - "Bootstrap detection via ephemeral_pk length (64 = bootstrap, 32 = regular)"
  - "Token consumed after mailbox creation (not before)"

patterns-established:
  - "Bootstrap message format: KeyExchange with 64-byte ephemeral_pk"
  - "Response mirrors bootstrap format for symmetric handling"

issues-created: []

# Metrics
duration: 7min
completed: 2026-02-04
---

# Phase 27 Plan 02: DM Bootstrap Summary

**DmBootstrap with initiate_dm() for starting DM conversations and handle_incoming_bootstrap() for responding to peer bootstrap messages**

## Performance

- **Duration:** 7 min
- **Started:** 2026-02-04T14:17:36Z
- **Completed:** 2026-02-04T14:24:32Z
- **Tasks:** 2
- **Files modified:** 3

## Accomplishments

- Implemented DmBootstrap struct integrating TokenManager and DmConversationManager
- Created initiate_dm() for creating mailbox and preparing bootstrap message
- Added handle_incoming_bootstrap() for processing peer's bootstrap and deriving shared secret
- Designed 64-byte combined key format for bootstrap messages (mailbox || conversation key)
- Added ConversationExists error variant for duplicate conversation handling

## Task Commits

Each task was committed atomically:

1. **Task 1: Implement DmBootstrap for initiating DM conversations** - `f82ffbb` (feat)
2. **Task 2: Implement bootstrap response handling** - `7ae6f91` (feat)

**Plan metadata:** TBD (docs: complete plan)

## Files Created/Modified

- `src/mailbox/dm/bootstrap.rs` - Full DmBootstrap implementation with 8 unit tests
- `src/mailbox/dm/mod.rs` - Re-export bootstrap types (DmBootstrap, BootstrapResult, BootstrapResponse, DEFAULT_MAILBOX_TTL_SECONDS)
- `src/error/mailbox_error.rs` - Added ConversationExists error variant

## Decisions Made

- **64-byte combined key format**: Bootstrap messages encode both mailbox public key (32 bytes) and conversation ephemeral key (32 bytes) in a single KeyExchange message, enabling detection via length check
- **Token consumption timing**: Token is marked as used after successful mailbox creation, ensuring tokens aren't wasted on failed requests
- **Existing DmMailbox.token_id field**: Used the existing field design rather than adding a separate parameter - cleaner API

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

None

## Next Phase Readiness

- Bootstrap infrastructure complete for initiating and responding to DMs
- Ready for 27-03: Two-way DM messaging using established mailboxes
- Shared secret derived via X25519 ECDH, ready for message encryption

---
*Phase: 27-mailbox-dm-system*
*Completed: 2026-02-04*
