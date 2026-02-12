# Inbox Events

**File**: `src/inbox/events.rs`

Events emitted by DeviceInboxProvider for UI updates.

## Event Types

```rust
pub enum InboxEvent {
    // === DM Events ===

    /// DM message successfully decrypted
    DmReceived {
        peer_id: UserId,
        text: String,
        timestamp: DateTime<Utc>,
        message_id: MessageId,
    },

    /// DM decryption failed
    DmDecryptionFailed {
        peer_id: UserId,
        error: String,
    },

    /// DM unread count changed
    DmUnreadCountUpdated {
        channel_id: ChannelId,
        peer_id: UserId,
        unread_count: u32,
    },

    /// DM poll cycle completed
    DmPollCompleted {
        total_messages: u32,
    },

    // === Channel Events ===

    /// Channel message received (before processing)
    MessageReceived {
        message_id: String,
        envelope_type: i32,
    },

    /// Channel message successfully processed
    MessageProcessed {
        message_id: String,
        channel_id: ChannelId,
        content_type: String,
    },

    /// Channel message processing failed
    MessageFailed {
        message_id: String,
        error: String,
    },

    // === Poll Events ===

    /// Device inbox poll completed
    PollCompleted {
        count: u32,
        has_more: bool,
    },

    /// Device inbox poll failed
    PollFailed {
        error: String,
    },

    // === Sender Key Events ===

    /// Sender key distribution received
    SenderKeyReceived {
        channel_id: ChannelId,
        sender_id: UserId,
    },

    /// Sender key request received (need to send our key)
    SenderKeyRequested {
        channel_id: ChannelId,
        requester_id: UserId,
    },

    // === Group Mailbox Events ===

    /// Group mailbox poll completed
    GroupMailboxPollCompleted {
        total_groups: u32,
        total_messages: u32,
    },

    /// Group mailbox announcement received
    GroupMailboxAnnouncement {
        channel_id: ChannelId,
        member_id: UserId,
        mailbox_pk: Vec<u8>,
        expires_at: DateTime<Utc>,
    },
}
```

## Subscribing to Events

```rust
// Get provider
let provider = ctx.get_device_inbox_provider()?;

// Subscribe
let mut rx = provider.subscribe();

// Handle in loop
tokio::spawn(async move {
    while let Ok(event) = rx.recv().await {
        // Must use invoke_from_event_loop for UI updates
        slint::invoke_from_event_loop(move || {
            handle_inbox_event(event, &window);
        }).unwrap();
    }
});
```

## Event Handling (auth.rs)

```rust
fn handle_inbox_event(event: InboxEvent, window: &MainWindow) {
    match event {
        InboxEvent::DmReceived { peer_id, text, timestamp, message_id } => {
            // Add message to chat view if this DM is selected
            let selected = window.get_selected_dm_id();
            if selected.as_str() == peer_id.to_string() {
                add_message_to_view(window, &message_id, &text, timestamp);
            }
        }

        InboxEvent::DmUnreadCountUpdated { peer_id, unread_count, .. } => {
            // Update sidebar badge
            update_dm_badge(window, &peer_id, unread_count);
        }

        InboxEvent::MessageProcessed { channel_id, .. } => {
            // Refresh channel if selected
            let selected = window.get_selected_channel_id();
            if selected.as_str() == channel_id.to_string() {
                refresh_channel_messages(window, &channel_id);
            }
        }

        InboxEvent::PollFailed { error } => {
            tracing::error!("Inbox poll failed: {}", error);
            // Maybe show connection status indicator
        }

        _ => {}
    }
}
```

## Event Flow

```
DeviceInboxProvider::run_loop()
    │
    ├─→ Process message
    │   │
    │   └─→ Emit event via broadcast channel
    │       │
    │       └─→ All subscribers receive
    │           │
    │           └─→ auth.rs: handle_inbox_event()
    │               │
    │               └─→ Update UI
```

## Thread Safety

Events are processed on background thread. UI updates must use:

```rust
slint::invoke_from_event_loop(move || {
    // UI updates here
    window.set_some_property(value);
}).unwrap();
```

## Emitting Events

In DeviceInboxProvider:

```rust
// Emit event
self.tx.send(InboxEvent::DmReceived {
    peer_id,
    text,
    timestamp,
    message_id,
}).ok();  // .ok() ignores "no receivers" error
```

## Domain Events via EventBus

For cross-component events (not just UI):

```rust
// EventBus is separate from InboxEvent
if let Some(bus) = &self.event_bus {
    bus.publish(DomainEvent::KeyExchangeReceived {
        peer_id,
        mailbox_address,
    });
}
```

Domain events used for:
- DM bootstrap completion
- Group membership changes
- Key rotation triggers
