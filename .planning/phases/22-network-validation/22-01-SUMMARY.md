---
phase: 22-network-validation
plan: "01"
subsystem: network
tags: [validation, deduplication, bounded-set, admission-control]

# Dependency graph
requires:
  - phase: 20-network-streaming
    provides: Message routing patterns, broadcast channel patterns
  - phase: 21-network-resilience
    provides: RwLock patterns for concurrent access
provides:
  - BoundedHashSet for memory-bounded set operations with FIFO eviction
  - MessageDeduplicator for thread-safe duplicate message detection
  - ContentValidator for message admission control
  - ValidationError enum for validation failure reporting
affects: [23-inbox, 24-inbox-handlers]

# Tech tracking
tech-stack:
  added: []
  patterns:
    - BoundedHashSet with VecDeque for FIFO eviction tracking
    - RwLock for concurrent read-heavy validation access
    - Content type estimation matching proto variants

key-files:
  created:
    - src/util/bounded_vec.rs (BoundedHashSet implementation)
    - src/network/validation/deduplicator.rs (MessageDeduplicator)
    - src/network/validation/validator.rs (ContentValidator)
  modified:
    - src/network/validation/mod.rs (exports)

key-decisions:
  - "FIFO eviction over LRU - simpler, sufficient for 'recently seen' tracking"
  - "Empty author_id allowed - sealed sender messages have no author per Signal protocol"
  - "Content size estimation uses proto type matching - handles all 9 content variants"

patterns-established:
  - "BoundedHashSet: HashSet + VecDeque for bounded collections with eviction"
  - "Validation returns Result<(), ValidationError> for composable checking"

issues-created: []

# Metrics
duration: 4min
completed: 2026-02-03
---

# Phase 22 Plan 01: Deduplicator & Validator Summary

**BoundedHashSet utility with FIFO eviction, MessageDeduplicator for thread-safe duplicate detection, ContentValidator for message admission control**

## Performance

- **Duration:** 4 min
- **Started:** 2026-02-03T18:28:53Z
- **Completed:** 2026-02-03T18:32:40Z
- **Tasks:** 3
- **Files modified:** 4

## Accomplishments

- BoundedHashSet with FIFO eviction using VecDeque for insertion order tracking
- MessageDeduplicator with RwLock for concurrent GUID tracking (10k default capacity)
- ContentValidator with ValidationError enum covering ID format, timestamp bounds, and content size
- Sealed sender support (empty author_id passes validation per Signal protocol)

## Task Commits

Each task was committed atomically:

1. **Task 1: Implement BoundedHashSet utility** - `f6c5903` (feat)
2. **Task 2: Implement MessageDeduplicator** - `121cc71` (feat)
3. **Task 3: Implement ContentValidator with ValidationError** - `63080d3` (feat)

## Files Created/Modified

- `src/util/bounded_vec.rs` - BoundedHashSet with FIFO eviction, insert returns bool for duplicate detection
- `src/network/validation/deduplicator.rs` - Thread-safe MessageDeduplicator with is_duplicate(), mark_processed(), stats()
- `src/network/validation/validator.rs` - ContentValidator, ValidationError, ValidationLimits with configurable bounds
- `src/network/validation/mod.rs` - Module exports for all public types

## Decisions Made

- **FIFO eviction over LRU**: Simpler implementation using VecDeque is sufficient for deduplication since we only need to know "was this recently seen", not "when was it last accessed"
- **RwLock for deduplicator**: Read-heavy access pattern (is_duplicate called frequently) benefits from shared read locks
- **Empty author_id allowed**: Sealed sender messages legitimately have no author - validation must not reject them

## Deviations from Plan

### Auto-fixed Issues

**1. [Rule 3 - Blocking] Fixed Content type variants to match actual proto**
- **Found during:** Task 3 (ContentValidator implementation)
- **Issue:** Plan specified Media, FileAttachment, Reply variants that don't exist in proto. Actual variants are Text, Encrypted, File, Reaction, Mixed, AiBot, SignalEncrypted, SealedSender, Call
- **Fix:** Updated estimate_content_size() to use correct proto variant names (name instead of filename, body instead of ciphertext, sealed_body instead of envelope)
- **Files modified:** src/network/validation/validator.rs
- **Verification:** cargo build --lib succeeds, all 28 tests pass
- **Committed in:** 63080d3 (part of Task 3 commit)

---

**Total deviations:** 1 auto-fixed (blocking - proto mismatch)
**Impact on plan:** Proto field names corrected to match actual generated types. No scope creep.

## Issues Encountered

None - plan executed smoothly after proto field correction.

## Next Phase Readiness

- Deduplicator and validator ready for integration into message routing pipeline
- Phase 22 has additional plans for orderer component
- All components designed for composition in validation pipeline

---
*Phase: 22-network-validation*
*Completed: 2026-02-03*
