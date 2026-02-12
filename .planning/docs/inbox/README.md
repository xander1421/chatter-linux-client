# Inbox System

Message receiving and processing.

## Overview

The inbox system handles all incoming messages:
- Device inbox (channel messages, sender key distributions)
- DM mailboxes (private messages via anonymous mailboxes)

## Documentation

- [Polling](polling.md) - DeviceInboxProvider run loop
- [Events](events.md) - InboxEvent types and handling
- [DM Receive](dm-receive.md) - Two-layer decryption flow
- [Channel Receive](channel-receive.md) - Sender key decryption

## Key Principle

**There is exactly ONE polling loop: DeviceInboxProvider**

Never create additional polling loops. Subscribe to InboxEvents instead.

## Architecture

```
DeviceInboxProvider
    │
    ├─→ poll_device_inbox()
    │   │
    │   └─→ For each message:
    │       │
    │       ├─→ SKDM → Store sender key
    │       ├─→ Sender Key Msg → Decrypt, dispatch
    │       ├─→ SK Request → Send our SKDM
    │       └─→ Sealed Sender → Decrypt
    │
    └─→ poll_dm_mailboxes()
        │
        └─→ For each DM:
            │
            └─→ Two-layer decrypt → Store → Emit event
```

## Files

| File | Purpose |
|------|---------|
| `provider.rs` | DeviceInboxProvider (the run loop) |
| `poller.rs` | Low-level gRPC polling |
| `events.rs` | InboxEvent enum |
| `config.rs` | Polling configuration |
| `error.rs` | InboxError types |
| `state.rs` | Polling state tracking |

## Usage

```rust
// Initialize after login
ctx.init_device_inbox_provider();

// Get provider
let provider = ctx.get_device_inbox_provider()?;

// Start polling (background task)
tokio::spawn(async move {
    provider.run_loop().await;
});

// Subscribe to events
let mut rx = provider.subscribe();
while let Ok(event) = rx.recv().await {
    handle_event(event);
}
```

## Event Flow

```
Message arrives
    │
    ▼
DeviceInboxProvider processes
    │
    ├─→ Decrypts (via MessageGateway)
    │
    ├─→ Stores (via repositories)
    │
    └─→ Emits InboxEvent
        │
        ▼
    Subscribers receive
        │
        ▼
    UI updates (via invoke_from_event_loop)
```
