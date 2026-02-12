# Chatter Linux Client Documentation

Domain-specific documentation for the Chatter Linux client.

## Quick Navigation

### Status & Overview
- [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md) - **What's done, what's missing, system glue**
- [ARCHITECTURE.md](../ARCHITECTURE.md) - Full architecture overview

### Service Layer
- [Signal Service](services/signal-service.md) - Signal Protocol operations
- [Message Service](services/message-service.md) - Message sending orchestration
- [Attachment Service](services/attachment-service.md) - File upload/download
- [Auth Service](services/auth-service.md) - Authentication flows
- [Mailbox Manager](services/mailbox-manager.md) - DM privacy mailboxes

### UI Callbacks
- [Auth Callbacks](ui-callbacks/auth.md) - Login/register/logout
- [Chat Callbacks](ui-callbacks/chat.md) - Message sending, typing, reactions
- [Composer Callbacks](ui-callbacks/composer.md) - Attachments, emoji, mentions
- [Modal Callbacks](ui-callbacks/modals.md) - Channels, settings, friends

### Inbox System
- [Polling](inbox/polling.md) - DeviceInboxProvider loop
- [Events](inbox/events.md) - InboxEvent types and handling
- [DM Receive](inbox/dm-receive.md) - Two-layer decryption flow
- [Channel Receive](inbox/channel-receive.md) - Sender key decryption

### Crypto Layer
- [Signal Protocol](crypto/signal-protocol.md) - PQXDH, Double Ratchet
- [Sealed Sender](crypto/sealed-sender.md) - Anonymous messaging
- [Sender Keys](crypto/sender-keys.md) - Group encryption
- [DM Encryption](crypto/dm-encryption.md) - Mailbox layer

### Storage Layer
- [Key Store](storage/key-store.md) - Signal key persistence
- [Repositories](storage/repositories.md) - Data access patterns

## Layer Responsibilities

```
┌────────────────────────────────────────────────┐
│ UI Layer (bin_ui/features/)                    │
│ → Wire callbacks, update UI state, spawn async │
└────────────────────────────────────────────────┘
                      │
                      ▼
┌────────────────────────────────────────────────┐
│ Service Layer (services/)                      │
│ → Business logic, orchestration                │
└────────────────────────────────────────────────┘
                      │
                      ▼
┌────────────────────────────────────────────────┐
│ Crypto / Network / Storage                     │
│ → Raw operations                               │
└────────────────────────────────────────────────┘
```

## Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `CHATTER_SERVER_URL` | No | `http://localhost:50051` | Server URL |
| `TENOR_API_KEY` | No | (none) | Tenor API key for GIF search |

## Key Rules

1. **UI never does crypto** - Always delegate to SignalService
2. **UI never builds protos** - Use `build_*_content()` helpers
3. **Services from AppContext** - Never create manually
4. **One polling loop** - DeviceInboxProvider only
5. **Deduplication** - Always `insert_or_ignore()`
