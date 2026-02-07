# 71-02 Summary: Chat Interaction Callbacks

## Status: COMPLETED

## What Was Built

### Task 1: Wire Message Action Callbacks

Implemented `wire_chat_action_callbacks(window: &MainWindow, ctx: &AppContext)` in `src/main.rs` with 5 message action callbacks:

1. **message-clicked(message_id: String)**
   - Logs message selection for future multi-select or context menu integration
   - Placeholder for UI element selection state

2. **reply-clicked(message_id: String)**
   - Parses message ID and fetches message from MessageRepository
   - Creates reply preview with sender and truncated content
   - Updates composer placeholder to "Reply to {sender}: {preview}"
   - Future: Store reply target in ChatState for send

3. **react-clicked(message_id: String)**
   - Logs request to open emoji picker for specific message
   - Placeholder for emoji picker visibility toggle
   - Future: Integrate with ReactionService.add_reaction()

4. **thread-clicked(message_id: String)**
   - Opens thread panel via ChatState.open_thread()
   - Handles max 3 concurrent threads with LRU replacement
   - Future: Fetch and display thread messages

5. **more-clicked(message_id: String)**
   - Logs request for context menu
   - Placeholder for edit, delete, forward, pin actions

### Task 2: Wire Pagination and Header Callbacks

Added to `wire_header_callbacks(window: &MainWindow, ctx: &AppContext)`:

1. **search-clicked()**
   - Placeholder for search modal
   - Future: Set active-modal to "search"

2. **pinned-clicked()**
   - Gets active channel from ChatState
   - Logs pinned bar toggle request
   - Future: Toggle pinned bar visibility, fetch pinned messages

3. **chat-settings-clicked()**
   - Gets active channel from ChatState
   - Logs channel settings request
   - Future: Open channel settings modal

4. **load-more()**
   - Checks loading state via MessageState.is_loading()
   - Validates pagination cursor for has_more
   - Fetches older messages via MessageRepository.list_for_channel()
   - Updates pagination cursor with new oldest message
   - Resolves sender names from UserRepository
   - Prepends older messages to UI model via invoke_from_event_loop

### Task 3: Wire Composer Attachment Callbacks

Implemented `wire_composer_callbacks(window: &MainWindow, _ctx: &AppContext)`:

1. **attachment-clicked()**
   - Uses spawn_blocking for native file dialog (rfd)
   - Configures filters for images, documents, videos, audio, all
   - Supports multiple file selection
   - Updates composer placeholder to show attachment count
   - Future: Store pending attachments in ChatState

2. **emoji-clicked()**
   - Logs emoji picker toggle request
   - Future: Toggle emoji-picker-visible property

3. **gif-clicked()**
   - Logs GIF picker toggle request
   - Future: Toggle gif-picker-visible property

4. **mention-triggered(query: String)**
   - Handles empty query (close autocomplete)
   - Logs mention search query
   - Future: Search channel members and show autocomplete

## Files Modified

- `src/main.rs`: Added ~500 lines for new callbacks
  - `wire_chat_action_callbacks()` - 5 message action callbacks
  - `wire_header_callbacks()` - 4 header/pagination callbacks
  - `wire_composer_callbacks()` - 4 composer callbacks
  - Import for `slint::Model` trait for ModelRc iteration

## Verification

- [x] `cargo build` succeeds without errors
- [x] All 13 chat callbacks are wired
- [x] load-more implements cursor pagination
- [x] attachment-clicked opens native file picker
- [x] thread-clicked updates ChatState

## Implementation Notes

### Async Patterns

- Message fetching uses `tokio::spawn` for async operations
- File picker uses `spawn_blocking` with nested runtime (rfd requires blocking)
- UI updates use `slint::invoke_from_event_loop` for thread safety

### Placeholder Logic

Several callbacks are placeholders pending UI components:
- Emoji picker, GIF picker, mention autocomplete require UI widgets
- Search modal, context menu require modal components
- Pinned bar requires visibility toggle property

These placeholders log the interaction and document the expected future behavior.

### State Management

- MessageState: Pagination cursors and loading state per channel
- ChatState: Thread panels (max 3), reply targets
- Uses SharedString for Slint string interop

## Commits

- `8316997` feat(71-02): wire chat interaction callbacks

## Notes

- The commit also includes 71-01 sidebar callbacks that were implemented but not previously committed
- `_ctx` parameter in `wire_composer_callbacks` is unused pending mention search implementation
- Unused variable warnings suppressed with `_` prefix for future-use variables
