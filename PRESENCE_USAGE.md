# Anonymous DM Presence Updates

This document describes how to use the presence update feature for anonymous DMs.

## Overview

Presence updates allow users to share their online status (online, away, do-not-disturb, offline) with their anonymous DM peers. The implementation sends presence updates through the existing anonymous DM infrastructure using a new `AnonDmPresence` message type.

## Implementation Details

### Proto Definition
- Added `AnonDmPresence` message type to `chat.v1.content.proto`
- Contains conversation ID, status enum, and timestamp
- Status values: Online, Away, DoNotDisturb, Offline

### Sending Presence Updates

```rust
use chatter_linux::proto::UserPresence;
use chatter_linux::bin_ui::features::auth::inbox::handle_user_presence_change;

// When user status changes (e.g., from UI control)
async fn on_user_status_change(ui_ctx: &Arc<UIContext>, new_status: &str) {
    let presence = match new_status {
        "online" => UserPresence::Online,
        "away" => UserPresence::Away,
        "dnd" => UserPresence::DoNotDisturb,
        _ => UserPresence::Offline,
    };
    
    // Broadcast to all active anon DM conversations
    handle_user_presence_change(ui_ctx, presence).await;
}

// Send to specific conversation
async fn on_open_chat(ui_ctx: &Arc<UIContext>, conversation_id: &AnonDmId) {
    // Send online presence when opening a chat
    send_anon_dm_presence(ui_ctx, conversation_id, UserPresence::Online).await;
}
```

### Receiving Presence Updates

Presence updates are automatically handled by the inbox event system:
1. `AnonDmHandler` receives the `AnonDmPresence` message
2. Emits `DomainEvent::UserPresenceChanged`
3. `domain_handler` updates the UI accordingly

### UI Integration Points

When the UI is ready for presence indicators, update these locations:

1. **Sidebar DM List** - Add presence indicator (green/yellow/red/gray dot)
2. **Chat Header** - Show peer's current status
3. **User Profile** - Display detailed presence information

Update the `domain_handler.rs` code to actually modify UI elements:

```rust
// In domain_handler.rs, UserPresenceChanged handler
if let Some(w) = weak.upgrade() {
    // Update DM sidebar
    let dm_model = w.get_dm_model();
    for i in 0..dm_model.row_count() {
        if let Some(mut dm) = dm_model.row_data(i) {
            if dm.peer_id == user_id_str {
                dm.presence_status = presence_str.into();
                dm.last_seen = timestamp as i64;
                dm_model.set_row_data(i, dm);
                break;
            }
        }
    }
    
    // Update chat header if active
    if w.get_current_dm_peer_id() == user_id_str {
        w.set_current_user_presence(presence_str.into());
    }
}
```

## When to Send Presence Updates

1. **App Launch** - Send "online" to all conversations
2. **App Focus** - Send "online" when app gains focus
3. **Idle Detection** - Send "away" after inactivity period
4. **User Action** - Send status when user manually changes it
5. **App Close** - Send "offline" before shutting down

## Example: Automatic Presence on App Events

```rust
// On app startup
tokio::spawn(async move {
    broadcast_anon_dm_presence(&ui_ctx, UserPresence::Online).await;
});

// On app idle (after 5 minutes)
start_idle_timer(Duration::from_secs(300), move || {
    tokio::spawn(async move {
        broadcast_anon_dm_presence(&ui_ctx, UserPresence::Away).await;
    });
});

// On app shutdown
broadcast_anon_dm_presence(&ui_ctx, UserPresence::Offline).await;
```

## Testing

To test presence updates:

1. Open two clients with an active anon DM conversation
2. Change status in one client
3. Verify the other client receives the `UserPresenceChanged` event
4. Check logs for "User presence updated" messages

## Future Enhancements

1. Add presence state store for caching/persistence
2. Implement presence for regular DMs and channels
3. Add "last seen" timestamps
4. Support custom status messages
5. Add typing indicators alongside presence