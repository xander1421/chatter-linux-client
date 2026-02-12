# Service Layer

Business logic and orchestration services.

## Core Principle

**Services are the single entry point for business logic.**

UI layer calls services → Services call crypto/network/storage

## Available Services

| Service | File | Purpose |
|---------|------|---------|
| [SignalService](signal-service.md) | `signal_service.rs` | Signal Protocol encryption/decryption |
| [MessageService](message-service.md) | `message_service.rs` | Message sending orchestration |
| [AttachmentService](attachment-service.md) | `attachment_service.rs` | File upload/download |
| [AuthService](auth-service.md) | `auth_service.rs` | Login/register/logout |
| [MailboxManager](mailbox-manager.md) | `mailbox_manager.rs` | DM privacy mailboxes |
| MessageGateway | `message_gateway.rs` | Central decryption router |

## Getting Services

Always get from AppContext:

```rust
// ✅ CORRECT
let signal_service = ctx.signal_service.clone();
let message_service = ctx.message_service();
let auth_service = ctx.auth_service();
let attachment_service = ctx.attachment_service();

// ❌ WRONG - Never create manually
let svc = MessageService::new(repo1, repo2, ...);
```

## Service Responsibilities

```
┌─────────────────────────────────────────────────────────────────┐
│ SignalService                                                   │
│ - Encrypt/decrypt messages (sealed sender, sender keys)         │
│ - Session management (PQXDH)                                    │
│ - Sender key rotation                                           │
│ - PreKey management                                             │
│ - Identity fingerprints                                         │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ MessageService                                                  │
│ - Orchestrate channel message sending                           │
│ - Orchestrate DM sending (two-layer encryption)                 │
│ - Distribute sender keys to new members                         │
│ - Handle saved messages                                         │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ AttachmentService                                               │
│ - File upload with chunking                                     │
│ - Upload verification                                           │
│ - (Future) File download                                        │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ AuthService                                                     │
│ - Registration with key generation                              │
│ - Login with session establishment                              │
│ - Logout with cleanup                                           │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ MailboxManager                                                  │
│ - Create anonymous mailboxes                                    │
│ - Send to peer mailboxes                                        │
│ - Manage blind signature tokens                                 │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ MessageGateway                                                  │
│ - Route encrypted messages by type                              │
│ - Two-layer DM decryption                                       │
│ - Sender key message handling                                   │
│ - Content builders (typing, reactions, etc.)                    │
└─────────────────────────────────────────────────────────────────┘
```

## Content Builders

Use these instead of building protos directly:

```rust
use chatter_linux::services::{
    build_text_content,
    build_typing_content,
    build_typing_content_with_channel,
    build_read_receipt_content,
    build_delivery_receipt_content,
    build_reaction_content,
    build_reaction_content_with_channel,
    build_pin_content,
    build_edit_content,
    build_delete_for_me_content,
    build_delete_for_everyone_content,
};
```

## Anti-Patterns

### 1. Crypto in UI

```rust
// ❌ WRONG
window.on_send(move || {
    let ciphertext = SealedSenderManager::encrypt(...).await?;
});

// ✅ CORRECT
window.on_send(move || {
    message_service.send_dm_text(...).await?;
});
```

### 2. Proto Building in UI

```rust
// ❌ WRONG
let content = Content {
    typing: Some(TypingMessage { ... }),
    ..Default::default()
};

// ✅ CORRECT
let content = build_typing_content(is_typing);
```

### 3. Manual Service Creation

```rust
// ❌ WRONG
let svc = SignalService::new(key_store, settings_repo);

// ✅ CORRECT
let svc = ctx.signal_service.clone();
```
