# Anonymous DM Presence Implementation

## Overview

This document describes the complete implementation of presence updates for anonymous DMs in the Chatter Linux client.

## Implementation Components

### 1. Protocol Layer

#### Proto Definition (`src/proto/chat.v1.content.rs`)
- **AnonDmPresence** message type (tag 44)
  - `conversation_id`: 16-byte UUID
  - `status`: Enum (Online=2, Away=3, DoNotDisturb=4, Offline=1)
  - `timestamp`: Nanoseconds since epoch

### 2. Service Layer

#### Sending (`src/services/anon_dm/messaging.rs`)
```rust
pub async fn send_presence_update(
    &self,
    conversation_id: &AnonDmId,
    presence: UserPresence,
) -> Result<(), AnonDmError>
```
- Checks if conversation is ready and not blocked
- Creates AnonDmPresence message
- Sends via anonymous mailbox

### 3. Handler Layer

#### Receiving (`src/inbox/handlers/anon_dm.rs`)
```rust
fn handle_presence(
    &self,
    presence: AnonDmPresence,
    sender_id: UserId,
    ctx: &HandlerContext,
) -> HandlerResult
```
- Validates conversation and sender
- Maps status to UserPresence enum
- Emits `DomainEvent::UserPresenceChanged`

### 4. UI Layer

#### Event Processing (`src/bin_ui/features/auth/inbox/domain_handler.rs`)
- Handles `UserPresenceChanged` events
- Updates UI elements (placeholder for now):
  - Sidebar DM status indicators
  - Chat header presence status
  - User profile presence

#### Lifecycle Management (`src/bin_ui/features/auth/presence_lifecycle.rs`)
- **Initialize**: Sends online status on login
- **Shutdown**: Sends offline status before app exit
- **Idle Detection**: Auto-away after 5 minutes of inactivity
- **Chat Open**: Sends presence when opening specific chat

#### Activity Tracking (`src/bin_ui/features/chat/user_activity.rs`)
- Tracks user interactions
- Resets idle timer
- Brings user back online from away status
- Debounces updates (30 second minimum between changes)

### 5. Integration Points

#### App Startup (`src/bin_ui/features/auth/login.rs`)
```rust
// After successful login
super::presence_lifecycle::initialize_presence_lifecycle(&weak, ui_ctx.clone()).await;
```

#### App Shutdown (`src/main.rs`)
```rust
// Before disconnecting
bin_ui::features::auth::presence_lifecycle::shutdown_presence(&ui_ctx).await;
```

#### User Activity Tracking
- **Typing**: `src/bin_ui/features/chat/typing.rs`
- **Message Send**: `src/bin_ui/features/chat/send_message.rs`
- **Chat Open**: `src/bin_ui/features/sidebar/dm.rs`

## Presence States

1. **Online** - User is actively using the app
2. **Away** - User is idle (5+ minutes of inactivity)
3. **Do Not Disturb** - User manually set (future feature)
4. **Offline** - User is not connected or app is closed

## Message Flow

### Sending Presence Update
1. User action triggers presence change
2. `set_user_presence()` updates local state
3. `broadcast_anon_dm_presence()` sends to all conversations
4. Each conversation receives `AnonDmPresence` message

### Receiving Presence Update
1. `AnonDmPresence` received via inbox
2. `AnonDmHandler::handle_presence()` processes
3. `DomainEvent::UserPresenceChanged` emitted
4. UI updates to reflect peer's status

## Testing

To test the presence system:

1. **Basic Flow**:
   - Open two clients with anon DM conversation
   - Login triggers online status
   - Wait 5 minutes for auto-away
   - Type to return online
   - Close app for offline status

2. **Check Logs**:
   ```
   grep "presence" app.log
   ```
   - Look for: "Sending presence update"
   - Look for: "User presence updated"

3. **Verify Messages**:
   - Use network inspector to see `AnonDmPresence` messages
   - Check conversation_id and status fields

## Future Enhancements

1. **UI Components**:
   - Add presence dot indicators in sidebar
   - Show "last seen" timestamps
   - Add presence selector dropdown

2. **Extended Features**:
   - Custom status messages
   - Presence for regular DMs
   - Group presence indicators
   - Presence history/analytics

3. **Optimizations**:
   - Batch presence updates
   - Presence subscription model
   - Cache presence states
   - Reduce update frequency

## Configuration

Currently hardcoded values:
- **Idle timeout**: 5 minutes (`IDLE_TIMEOUT_SECS`)
- **Activity debounce**: 30 seconds (`ACTIVITY_DEBOUNCE_SECS`)

These could be made configurable via settings.

## Security Considerations

- Presence updates are ephemeral (not stored)
- Only sent to active conversations
- Respects blocking (no presence to blocked users)
- No presence leaks to non-contacts