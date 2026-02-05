---
phase: 48-events-bus-handlers
plan: 01
subsystem: events, state
tags: [tokio, broadcast, async, events, state-management, reactive]

# Dependency graph
requires:
  - phase: 44-state-architecture
    provides: Store<T> pattern and state store implementations
  - phase: 45-message-state
    provides: MessageState and chat_state
  - phase: 46-pinned-state
    provides: PinnedState for pinned message tracking
  - phase: 47-streaming-offline
    provides: StreamingState and OfflineState
  - phase: 33-inbox-handlers
    provides: DomainEvent enum definition

provides:
  - EventBus for unified pub/sub across layers
  - UiEvent enum with 20+ UI-specific event variants
  - EventHandlers for domain event to state synchronization
  - Re-export of DomainEvent from inbox::handlers

affects: [ui-layer, services, application-shell, testing]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - "EventBus with dual broadcast channels (domain + UI)"
    - "EventHandlers spawn() pattern for async event loop"
    - "Re-export canonical types instead of duplicating"

key-files:
  created:
    - src/events/bus.rs
    - src/events/ui_events.rs
    - src/events/handlers.rs
  modified:
    - src/events/mod.rs

key-decisions:
  - "Re-export DomainEvent from inbox::handlers instead of duplicating"
  - "Separate channel capacities: 64 for domain (burst handling), 32 for UI"
  - "Sync handle_domain_event() method for testability"
  - "EventHandlers spawns background task, returns JoinHandle"

patterns-established:
  - "EventBus: Clone shares channels, emit methods return subscriber count"
  - "UiEvent: Category helper methods (is_navigation, is_modal, etc.)"
  - "EventHandlers: spawn() for background processing, handle_domain_event() for sync dispatch"

issues-created: []

# Metrics
duration: 18min
completed: 2026-02-05
---

# Phase 48-01: Events Bus & Handlers Summary

**EventBus with dual broadcast channels (domain/UI) and EventHandlers bridging domain events to reactive state stores**

## Performance

- **Duration:** 18 min
- **Started:** 2026-02-05T18:30:00Z
- **Completed:** 2026-02-05T18:48:00Z
- **Tasks:** 3
- **Files modified:** 4

## Accomplishments

- Implemented EventBus with domain channel (capacity 64) and UI channel (capacity 32)
- Created comprehensive UiEvent enum with 20 variants across 8 categories (navigation, modal, scroll, input, sidebar, search, selection, settings, toast)
- Implemented EventHandlers that spawns async loop to dispatch domain events to state stores
- Re-exported DomainEvent from inbox::handlers avoiding type duplication
- Added 40 unit tests across all modules

## Task Commits

Each task was committed atomically:

1. **Task 1: Implement EventBus** - `535874e` (feat)
2. **Task 2: Implement UiEvent types** - `a4c1bfb` (feat)
3. **Task 3: Implement EventHandlers** - `8d3cd5b` (feat)

## Files Created/Modified

- `src/events/bus.rs` - EventBus with dual broadcast channels for domain and UI events
- `src/events/ui_events.rs` - UiEvent enum with 20 variants, SidebarSection, SettingsTab, ToastLevel
- `src/events/handlers.rs` - EventHandlers bridging domain events to state stores
- `src/events/mod.rs` - Module exports and DomainEvent re-export

## Decisions Made

1. **Re-export DomainEvent instead of duplicate** - Per plan requirements, we import from inbox::handlers and re-export from events module. This maintains single source of truth.

2. **Channel capacities** - Domain channel at 64 to handle burst of incoming messages, UI channel at 32 for less frequent UI interactions.

3. **Sync handle_domain_event()** - Made the dispatch method synchronous for easier unit testing. The async spawn() wraps it in an event loop.

4. **Helper methods for typing** - Created add_typing_user() and remove_typing_user() helpers in EventHandlers to manage typing state cleanly.

## Deviations from Plan

None - plan executed exactly as written.

## Issues Encountered

1. **Test import path** - Initially used `crate::domain::message_content::MessageContent` but the type is defined in `message.rs` and re-exported. Fixed to use correct import path.

2. **MessageContent constructor** - Used `MessageContent::text()` but the enum uses `MessageContent::Text()` variant directly. Fixed to use correct variant name.

Both were minor test code issues, not affecting production code.

## Next Phase Readiness

- EventBus ready for use by UI layer and services
- EventHandlers can be spawned by application shell
- All state stores (MessageState, StreamingState, PinnedState, etc.) have methods needed by EventHandlers
- Some receipt events (read, delivered, viewed) marked with TODO for future services layer implementation

---
*Phase: 48-events-bus-handlers*
*Completed: 2026-02-05*
