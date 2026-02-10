# 71-01 Summary: Sidebar Navigation Callbacks

## Status: COMPLETED

## What Was Built

### Task 1: Wire Sidebar Navigation Callbacks

Implemented `wire_sidebar_callbacks(window: &MainWindow, ctx: &AppContext)` in `src/main.rs` with all 5 sidebar callbacks:

1. **channel-selected(channel_id: SharedString)**
   - Parses channel ID from string
   - Updates `ChatState.active_channel_id`
   - Sets loading state
   - Calls `load_channel_into_ui()` to fetch channel info and messages
   - Updates UI properties via `invoke_from_event_loop`

2. **dm-selected(dm_id: SharedString)**
   - Same pattern as channel-selected (DMs are channels with type DirectMessage)
   - Updates `selected-dm-id` UI property
   - Clears `selected-channel-id` for mutual exclusivity

3. **saved-messages-clicked()**
   - Sets channel name to "Saved Messages"
   - Sets channel type to "saved"
   - Clears channel/DM selections
   - TODO: Implement actual saved message fetching

4. **settings-clicked()**
   - Synchronous UI update (no async needed)
   - Populates settings properties from `AuthState` (display_name, username)
   - Loads privacy settings (read_receipts, typing_indicators) from `SettingsRepository`
   - Opens settings modal via `active-modal = "settings"`

5. **profile-clicked()**
   - Synchronous UI update
   - Populates profile modal with current user info from `AuthState`
   - Sets `profile-is-self = true`
   - Opens profile modal via `active-modal = "user-profile"`

### Task 2: Helper Functions for UI Data Conversion

Implemented helper functions for converting domain types to Slint model types:

1. **`load_channel_into_ui()`** - Async function that:
   - Fetches channel from `ChannelRepository`
   - Fetches messages from `MessageRepository` (limit 50)
   - Resolves sender names from `UserRepository` (profile display_name, fallback to username)
   - Updates all chat UI properties via `invoke_from_event_loop`

2. **`channel_to_slint(channel: &Channel) -> ChannelListItemData`**
   - Maps domain Channel to Slint struct

3. **`dm_to_slint(channel: &Channel, ...) -> DMListItemData`**
   - Maps DM channel to Slint struct with user info

4. **`message_to_slint(msg: &Message, sender_name: &str) -> MessageItemData`**
   - Maps domain Message to full Slint struct
   - Includes sender info, timestamps, reactions, thread status

5. **`channel_type_to_string(ct: ChannelType) -> &'static str`**
   - Converts enum to "public", "private", or "dm"

6. **`get_initials(name: &str) -> String`**
   - Extracts first letter of each word (max 2) for avatar initials

## Files Modified

- `src/main.rs`: Added ~250 lines
  - New imports (Arc, VecModel, ModelRc, domain types)
  - `wire_sidebar_callbacks()` function call in main()
  - `wire_sidebar_callbacks()` implementation
  - `load_channel_into_ui()` async helper
  - 5 conversion helper functions

## Verification

- [x] `cargo build` succeeds without errors
- [x] `cargo clippy` passes with no new warnings from main.rs
- [x] All 5 sidebar callbacks are wired
- [x] Helper functions compile and convert types correctly

## Notes

- `channel_to_slint` and `dm_to_slint` are marked `#[allow(dead_code)]` - they will be used in 71-05 for initial data loading
- Saved messages view is a placeholder - actual fetching to be implemented when saved message service is ready
- Message sender names resolved via profile first, then username fallback
