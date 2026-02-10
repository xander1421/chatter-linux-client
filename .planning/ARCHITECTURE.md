# Chatter Linux Client Architecture

> **Purpose**: Prevent duplicate logic, clarify responsibilities, and provide quick reference for all client components.

---

## Table of Contents

1. [Overview](#overview)
2. [Module Structure](#module-structure)
3. [Core Components](#core-components)
4. [Initialization Order](#initialization-order)
5. [Message Flows](#message-flows)
6. [Encryption Layers](#encryption-layers)
7. [Proto Message Formats](#proto-message-formats)
8. [Database Schema](#database-schema)
9. [State Management](#state-management)
10. [Error Handling](#error-handling)
11. [UI Wiring Patterns](#ui-wiring-patterns)
12. [Network Layer](#network-layer)
13. [Key Storage](#key-storage)
14. [Common Mistakes](#common-mistakes)
15. [Quick Reference](#quick-reference)

**Related**: [Signal Protocol Audit](./SIGNAL_PROTOCOL_AUDIT.md) - Detailed verification against Signal spec

---

## Overview

The Chatter Linux client is a **Slint-based** desktop application with:
- **Signal Protocol** for end-to-end encryption
- **SQLCipher** for encrypted local storage
- **gRPC/ConnectRPC** for server communication
- **Two-layer DM encryption** (Signal + mailbox anonymity)
- **Sender keys** for efficient group encryption

### Architecture Principles

1. **Single Source of Truth**: All services come from `AppContext`
2. **Event-Driven UI**: Inbox events trigger UI updates
3. **Layered Encryption**: Different layers for different purposes
4. **Deduplication**: Server message IDs prevent duplicates
5. **Stateless Crypto Managers**: Pass stores as parameters

---

## Module Structure

```
src/
├── app/                    # Application lifecycle
│   ├── context.rs          # Dependency container (THE source of services)
│   ├── bootstrap.rs        # Startup sequence
│   └── shutdown.rs         # Cleanup
│
├── bin_ui/                 # Slint UI binary
│   └── features/           # Callback wiring (one file per feature area)
│       ├── auth.rs         # Login/register, inbox event handling
│       ├── chat.rs         # Message send (DM + channel)
│       ├── sidebar.rs      # Channel/DM list
│       ├── modals.rs       # Create/join channel, settings
│       └── ...
│
├── config.rs               # Hardcoded values (TRUST ROOT!)
│
├── crypto/                 # Cryptographic primitives
│   ├── signal/             # Signal Protocol
│   │   ├── identity.rs     # Identity key management
│   │   ├── prekey.rs       # PreKey bundle management
│   │   ├── session.rs      # PQXDH session establishment
│   │   ├── sender_key.rs   # Group sender keys
│   │   ├── sealed_sender.rs # Anonymous messaging + UAK
│   │   └── store.rs        # 6 libsignal trait impls
│   ├── dm_crypto.rs        # XChaCha20-Poly1305 for mailbox layer
│   └── ...
│
├── domain/                 # Domain models (pure data, no logic)
│   ├── ids.rs              # UserId, ChannelId, MessageId
│   ├── message.rs          # Message, MessageContent
│   ├── channel.rs          # Channel, ChannelConfig
│   └── user.rs             # User, UserProfile
│
├── error/                  # Error types
│   ├── crypto_error.rs     # Encryption failures
│   ├── network_error.rs    # gRPC errors
│   ├── storage_error.rs    # Database errors
│   └── service_error.rs    # Business logic errors
│
├── events/                 # Event system
│   └── domain_events.rs    # Cross-component events
│
├── inbox/                  # Message receiving
│   ├── provider.rs         # DeviceInboxProvider (THE polling loop)
│   ├── poller.rs           # Low-level polling
│   └── processor.rs        # Message processing
│
├── mailbox/                # Privacy-preserving DM system
│   ├── dm/                 # Direct message mailboxes
│   │   ├── bootstrap.rs    # DM conversation setup
│   │   ├── conversation.rs # Conversation state
│   │   └── two_way.rs      # TwoWayAnonymity polling
│   └── ...
│
├── network/                # Network layer
│   ├── grpc/               # gRPC clients
│   │   ├── auth_client.rs  # AuthService RPC
│   │   ├── message_client.rs # MessageService RPC
│   │   ├── mailbox_client.rs # MailboxService RPC
│   │   └── client_pool.rs  # Connection pooling
│   └── validation/         # Message validation pipeline
│
├── proto/                  # Generated protobuf types
│   ├── chat.v1.rs          # Core types (Message, Channel, etc.)
│   ├── chat.v1.content.rs  # Content types (DataMessage, etc.)
│   └── signal.v1.rs        # Signal Protocol types
│
├── services/               # Business logic
│   ├── signal_service.rs   # High-level Signal operations
│   ├── message_service.rs  # Channel message sending
│   ├── message_gateway.rs  # Message decryption routing
│   ├── channel_service.rs  # Channel CRUD + sender key rotation
│   ├── auth_service.rs     # Login/register orchestration
│   └── mailbox_manager.rs  # DM mailbox operations
│
├── state/                  # Reactive UI state
│   ├── auth_state.rs       # Login status
│   ├── chat_state.rs       # Current chat view
│   ├── ui_state.rs         # Theme, modals
│   └── store.rs            # Generic reactive container
│
├── storage/                # Persistence
│   ├── database.rs         # SQLCipher wrapper
│   ├── key_store.rs        # Signal key storage
│   ├── repositories/       # Data access
│   │   ├── message_repo.rs # Message CRUD
│   │   ├── channel_repo.rs # Channel CRUD
│   │   └── settings_repo.rs # Key-value settings
│   └── migrations/         # Schema migrations
│
├── system/                 # OS integration
│   ├── keyring.rs          # Secret Service / libsecret
│   └── directories.rs      # XDG paths
│
└── ui/                     # UI utilities
    ├── bridge.rs           # Slint ↔ Rust bridge
    └── adapters/           # Data conversion
```

---

## Core Components

### AppContext (`src/app/context.rs`)

**THE source of all services**. Never create services manually.

```rust
// ✅ CORRECT: Get from context
let signal_service = ctx.signal_service.clone();
let message_service = ctx.message_service();
let auth_service = ctx.auth_service();

// ❌ WRONG: Manual creation
let svc = MessageService::new(repo1, repo2, ...); // NO!
```

**What it holds**:
| Category | Fields |
|----------|--------|
| System | `directories`, `keyring` |
| Storage | `database`, `key_store`, `*_repo` |
| Crypto | `identity_manager`, `prekey_manager`, `session_manager`, `sender_key_manager` |
| Network | `grpc_pool`, `auth_interceptor` |
| Services | `signal_service`, `message_service`, `message_gateway`, `mailbox_manager` |
| State | `ui_state`, `auth_state`, `chat_state`, ... |
| Events | `event_bus` |

### DeviceInboxProvider (`src/inbox/provider.rs`)

**THE message polling loop**. Handles BOTH:
- Device inbox (channel messages)
- DM mailboxes (via TwoWayAnonymity)

```
DeviceInboxProvider::run_loop()
    │
    ├─→ poll device inbox → process_message() → route encrypted
    │
    └─→ poll DM mailboxes → process_dm_message() → store + emit event
```

**DON'T** create another polling loop anywhere!

### MessageGateway (`src/services/message_gateway.rs`) ⭐ IMPORTANT

**THE central encryption/decryption router**. All encrypted messages go through here.

```
┌─────────────────────────────────────────────────────────────────┐
│ RECEIVE PATH (all implemented, working)                         │
├─────────────────────────────────────────────────────────────────┤
│ DM mailbox → process_dm_message() → (sender_id, text)           │
│   ├── decrypt_dm_bytes() (mailbox layer)                        │
│   └── signal_service.decrypt_message() (sealed sender)          │
│                                                                  │
│ Device inbox → route_encrypted_message() → RouteResult          │
│   ├── is_sender_key_distribution → process_sender_key_dist...   │
│   ├── is_sender_key_message → process_group_encrypted_msg...    │
│   └── is_sender_key_request → process_sender_key_request()      │
└─────────────────────────────────────────────────────────────────┘
```

| Method | Purpose | Status |
|--------|---------|--------|
| `process_dm_message()` | DM decryption (2-layer) | ✅ Complete |
| `route_encrypted_message()` | Dispatch encrypted channel msgs | ✅ Complete |
| `process_sender_key_distribution()` | Store incoming SKDM | ✅ Complete |
| `process_group_encrypted_message()` | Decrypt sender key msg | ✅ Complete |
| `process_sender_key_request()` | Respond with our SKDM | ✅ Complete |

**To add new Content types (typing, reactions, etc.):**
- Receive side already works - just parse `Content` proto after decryption and dispatch by type
- Send side: use existing `SignalService::encrypt_message()` (DM) or `encrypt_for_group()` (channel)

**DON'T** call lower-level crypto directly!

### SignalService (`src/services/signal_service.rs`)

**High-level Signal Protocol**. Wraps all Signal operations.

| Method | Purpose |
|--------|---------|
| `encrypt_message()` | Sealed sender encrypt for DM |
| `decrypt_message()` | Sealed sender decrypt (validates cert!) |
| `encrypt_for_group()` | Sender key encrypt for channel |
| `decrypt_from_group()` | Sender key decrypt |
| `ensure_session()` | Establish session via PQXDH |
| `create_sender_key()` | Generate SKDM |
| `rotate_sender_key()` | Rotate after member leave |
| `clear_channel_sender_keys()` | Remove keys on channel leave |

### MessageService (`src/services/message_service.rs`)

**Channel message sending only**. NOT for DMs!

```rust
// Channel message flow:
MessageService::send_message()
    → check channel.config.require_encryption
    → if true: SignalService::encrypt_for_group()
    → distribute SKDM if new
    → MessageClient::send_message()
```

### MailboxManager (`src/services/mailbox_manager.rs`)

**DM mailbox operations**. Used by `chat.rs` for DM sending.

---

## Initialization Order

AppContext initializes in this exact order:

```
1. System
   └── Directories (XDG paths)
   └── Keyring (libsecret)

2. Storage
   └── Database passphrase (from keyring or generate)
   └── Database::open() (SQLCipher)
   └── Run migrations
   └── All repositories

3. Crypto
   └── KeyStore
   └── IdentityKeyManager
   └── PreKeyManager
   └── SessionManager (stateless)
   └── SenderKeyManager (stateless)

4. Network
   └── GrpcClientPool
   └── AuthInterceptor

5. Services
   └── SignalService
   └── MessageService
   └── MessageGateway
   └── MailboxManager

6. State
   └── UIState, AuthState, ChatState, etc.
   └── EventBus

7. Post-Login (lazy)
   └── DeviceInboxProvider (init_device_inbox_provider)
```

---

## Message Flows

### DM Send Flow

```
┌─────────────────────────────────────────────────────────────────┐
│ User types message in DM chat                                   │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ chat.rs: on_send_dm_message()                                   │
│                                                                 │
│   1. Get peer_id from current DM selection                      │
│   2. Load DmConversation for shared secret                      │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Create DataMessage proto                                        │
│                                                                 │
│   DataMessage {                                                 │
│       body: "Hello!",                                           │
│       timestamp: 1707580800000,  // millis since epoch          │
│   }                                                             │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ SignalService::encrypt_message()                                │
│                                                                 │
│   1. Ensure session exists (PQXDH if not)                       │
│   2. Get sender certificate from server                         │
│   3. SealedSenderManager::encrypt()                             │
│      → Double Ratchet encrypt                                   │
│      → Wrap in sealed sender envelope                           │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ DmCrypto::encrypt_dm_bytes()                                    │
│                                                                 │
│   XChaCha20-Poly1305 encrypt with conversation shared secret    │
│   → Hides content from server even if Signal layer compromised  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ MailboxManager::send_to_mailbox()                               │
│                                                                 │
│   Send to peer's anonymous receive mailbox                      │
│   → Server cannot link sender ↔ recipient                       │
└─────────────────────────────────────────────────────────────────┘
```

**Files**: `src/bin_ui/features/chat.rs`

### DM Receive Flow

```
┌─────────────────────────────────────────────────────────────────┐
│ DeviceInboxProvider::run_loop()                                 │
│                                                                 │
│   Runs continuously after login                                 │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ poll_dm_mailboxes()                                             │
│                                                                 │
│   1. TwoWayAnonymity::poll_all_mailboxes()                      │
│   2. For each message: get shared secret from conversation      │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ MessageGateway::process_dm_message()                            │
│                                                                 │
│   Layer 1: decrypt_dm_bytes(mailbox_encrypted, shared_secret)   │
│            → XChaCha20-Poly1305 decrypt                         │
│                                                                 │
│   Layer 2: SignalService::decrypt_message(signal_ciphertext)    │
│            → Validates sender certificate against TRUST_ROOT    │
│            → Double Ratchet decrypt                             │
│            → Returns (sender_address, plaintext)                │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Parse DataMessage proto                                         │
│                                                                 │
│   DataMessage::decode(plaintext)                                │
│   Falls back to raw UTF-8 for backwards compatibility           │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Store message                                                   │
│                                                                 │
│   1. Derive DM channel ID: ChannelId::from_dm_participants()    │
│   2. Create Message domain object                               │
│   3. message_repo.insert_or_ignore() → deduplication            │
│   4. channel_repo.increment_unread_count()                      │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Emit events                                                     │
│                                                                 │
│   InboxEvent::DmReceived { peer_id, text, timestamp }           │
│   InboxEvent::DmUnreadCountUpdated { channel_id, count }        │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ auth.rs: handle_inbox_event()                                   │
│                                                                 │
│   - DmReceived: Add message to chat view if DM selected         │
│   - DmUnreadCountUpdated: Update sidebar badge                  │
└─────────────────────────────────────────────────────────────────┘
```

**Files**: `src/inbox/provider.rs`, `src/services/message_gateway.rs`, `src/bin_ui/features/auth.rs`

### Channel Send Flow

```
┌─────────────────────────────────────────────────────────────────┐
│ User types message in channel                                   │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ chat.rs: on_send_channel_message()                              │
│                                                                 │
│   Calls MessageService::send_message()                          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ MessageService::send_message()                                  │
│                                                                 │
│   1. Get channel from repo                                      │
│   2. Check channel.config.require_encryption                    │
└─────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┴───────────────┐
              │                               │
              ▼                               ▼
┌──────────────────────────┐    ┌──────────────────────────┐
│ require_encryption=false │    │ require_encryption=true  │
│                          │    │                          │
│ Send plaintext           │    │ SignalService::          │
│                          │    │   encrypt_for_group()    │
│                          │    │                          │
│                          │    │ Returns:                 │
│                          │    │ (ciphertext, Option<SKDM>)│
└──────────────────────────┘    └──────────────────────────┘
                                              │
                                              ▼
                              ┌─────────────────────────────────┐
                              │ If SKDM returned (first message)│
                              │                                 │
                              │ distribute_sender_key()         │
                              │ → Send SKDM to all members      │
                              └─────────────────────────────────┘
                                              │
                                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ MessageClient::send_message()                                   │
│                                                                 │
│   gRPC call to server                                           │
└─────────────────────────────────────────────────────────────────┘
```

**Files**: `src/bin_ui/features/chat.rs`, `src/services/message_service.rs`

### Channel Receive Flow

```
┌─────────────────────────────────────────────────────────────────┐
│ DeviceInboxProvider::run_loop()                                 │
│                                                                 │
│   InboxPoller::run_once() → Vec<DeviceMessage>                  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ process_message(msg)                                            │
│                                                                 │
│   1. Try: EncryptedData::decode(msg.envelope)                   │
│   2. Check associated_data marker                               │
└─────────────────────────────────────────────────────────────────┘
                              │
         ┌────────────────────┼────────────────────┐
         │                    │                    │
         ▼                    ▼                    ▼
┌────────────────┐  ┌────────────────┐  ┌────────────────┐
│ "sender_key_   │  │ "sender_key"   │  │ "sender_key_   │
│  distribution" │  │                │  │  request"      │
│                │  │                │  │                │
│ process_       │  │ route_         │  │ process_       │
│ sender_key_    │  │ encrypted_     │  │ sender_key_    │
│ distribution() │  │ message()      │  │ request()      │
│                │  │                │  │                │
│ → Store sender │  │ → Decrypt      │  │ → Refresh      │
│   key locally  │  │ → Parse DataMsg│  │   our SKDM     │
└────────────────┘  │ → Store msg    │  └────────────────┘
                    └────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ If decrypted successfully:                                      │
│                                                                 │
│   1. Parse DataMessage proto                                    │
│   2. Create Message domain object                               │
│   3. message_repo.insert_or_ignore()                            │
│   4. channel_repo.increment_unread_count()                      │
│   5. Emit InboxEvent::MessageReceived                           │
└─────────────────────────────────────────────────────────────────┘
```

**Files**: `src/inbox/provider.rs`, `src/services/message_gateway.rs`

### Sender Key Rotation

```
┌─────────────────────────────────────────────────────────────────┐
│ Member leaves or is kicked                                      │
└─────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┴───────────────┐
              │                               │
              ▼                               ▼
┌──────────────────────────┐    ┌──────────────────────────┐
│ leave_channel()          │    │ kick_user()              │
│ (voluntary leave)        │    │ (admin removes member)   │
│                          │    │                          │
│ clear_channel_           │    │ rotate_sender_key()      │
│ sender_keys()            │    │                          │
│                          │    │ → Delete old key         │
│ → Remove local keys      │    │ → Create new key         │
│ → Leaver can't decrypt   │    │ → SKDM sent on next msg  │
│   future messages        │    │ → Kicked user excluded   │
└──────────────────────────┘    └──────────────────────────┘
```

**Files**: `src/services/channel_service.rs`, `src/services/signal_service.rs`

---

## Encryption Layers

### DM Messages: Two-Layer Encryption

```
┌─────────────────────────────────────────────────────────────────┐
│ LAYER 2: Mailbox Encryption                                     │
│ Algorithm: XChaCha20-Poly1305                                   │
│ Key: X25519 shared secret (from DM bootstrap)                   │
│ Purpose: SENDER ANONYMITY - server cannot link sender/recipient │
│ File: src/crypto/dm_crypto.rs                                   │
│                                                                 │
│   ┌─────────────────────────────────────────────────────────┐   │
│   │ LAYER 1: Signal Protocol (Sealed Sender)                │   │
│   │ Algorithm: Double Ratchet + X25519 + AES-256-GCM        │   │
│   │ Purpose: FORWARD SECRECY - past messages safe if key    │   │
│   │          compromised                                    │   │
│   │ Validates: Sender certificate against TRUST_ROOT        │   │
│   │ File: src/crypto/signal/sealed_sender.rs                │   │
│   │                                                         │   │
│   │   ┌─────────────────────────────────────────────────┐   │   │
│   │   │ PLAINTEXT: DataMessage proto                    │   │   │
│   │   │                                                 │   │   │
│   │   │ DataMessage {                                   │   │   │
│   │   │     body: "Hello, world!",                      │   │   │
│   │   │     timestamp: 1707580800000,                   │   │   │
│   │   │     channel_id: "",  // empty for DMs           │   │   │
│   │   │ }                                               │   │   │
│   │   └─────────────────────────────────────────────────┘   │   │
│   └─────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

### Channel Messages: Sender Key Encryption

```
┌─────────────────────────────────────────────────────────────────┐
│ EncryptedData proto                                             │
│ File: src/proto/chat.v1.rs                                      │
│                                                                 │
│ EncryptedData {                                                 │
│     version: CryptoVersion::V1,                                 │
│     nonce: [...],                                               │
│     ciphertext: <serialized GroupEncrypted>,                    │
│     auth_tag: [...],                                            │
│     associated_data: b"sender_key",  ← ROUTING MARKER           │
│     device_id: "device-123",                                    │
│ }                                                               │
│                                                                 │
│   ┌─────────────────────────────────────────────────────────┐   │
│   │ GroupEncrypted proto                                    │   │
│   │ File: src/proto/chat.v1.content.rs                      │   │
│   │                                                         │   │
│   │ GroupEncrypted {                                        │   │
│   │     channel_id: "ch_abc123",                            │   │
│   │     sender_device_id: 1,                                │   │
│   │     ciphertext: <sender key encrypted bytes>,           │   │
│   │ }                                                       │   │
│   │                                                         │   │
│   │   ┌─────────────────────────────────────────────────┐   │   │
│   │   │ PLAINTEXT: DataMessage proto                    │   │   │
│   │   │                                                 │   │   │
│   │   │ DataMessage {                                   │   │   │
│   │   │     body: "Hello, channel!",                    │   │   │
│   │   │     timestamp: 1707580800000,                   │   │   │
│   │   │     channel_id: "ch_abc123",                    │   │   │
│   │   │ }                                               │   │   │
│   │   └─────────────────────────────────────────────────┘   │   │
│   └─────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

### Associated Data Markers

Used to route encrypted messages to correct handler:

| Marker | Type | Handler |
|--------|------|---------|
| `b"sender_key"` | Encrypted content | `process_group_encrypted_message()` |
| `b"sender_key_distribution"` | SKDM | `process_sender_key_distribution()` |
| `b"sender_key_request"` | Key request | `process_sender_key_request()` |

---

## Proto Message Formats

### DataMessage (`chat.v1.content.rs`)

The universal message content wrapper:

```protobuf
message DataMessage {
    string body = 1;                        // Message text
    repeated AttachmentPointer attachments = 2;
    fixed64 timestamp = 7;                  // Millis since epoch
    Quote quote = 8;                        // Reply reference
    Delete delete = 17;                     // Delete for everyone

    // Chatter extensions (field 100+)
    bool ephemeral = 100;
    uint32 ephemeral_ttl_seconds = 101;
    bool targets_ai_bot = 102;
    repeated string mentioned_integrations = 103;
    repeated string mentioned_file_ids = 104;
    string channel_id = 105;                // For group context
    string author_name = 106;               // Display name
}
```

### Content Wrapper (`chat.v1.content.rs`)

Outer envelope with padding for traffic analysis protection:

```protobuf
message Content {
    bytes ephemeral_reply_to = 100;  // Reply mailbox address
    fixed64 timestamp = 101;
    bytes padding = 200;              // Random padding to bucket size

    oneof content {
        DataMessage data_message = 1;
        TypingMessage typing = 2;
        ReceiptMessage receipt = 3;
        Reaction reaction = 4;
        PinMessage pin = 5;
        EditMessage edit = 6;
        KeyExchange key_exchange = 7;
        AttachmentMessage attachment = 8;
        SenderKeyDistribution sender_key = 9;
        GroupEncrypted group_encrypted = 10;
        SenderKeyRequest sender_key_request = 11;
        // ... more types
    }
}
```

### DeviceMessage (`chat.v1.rs`)

What arrives in device inbox:

```protobuf
message DeviceMessage {
    int64 queue_id = 1;              // For pagination
    string message_guid = 2;          // Deduplication key
    bytes envelope = 3;               // Encrypted content
    int32 envelope_type = 4;          // Type hint
    fixed64 server_timestamp = 5;
    string source_user_id = 6;
    uint32 source_device_id = 7;
    string destination_user_id = 8;
    uint32 destination_device_id = 9;
}
```

---

## Database Schema

### Overview

```
┌─────────────────────────────────────────────────────────────────┐
│ SQLCipher Database (encrypted with passphrase from keyring)    │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐             │
│  │   users     │  │  channels   │  │  messages   │             │
│  └─────────────┘  └─────────────┘  └─────────────┘             │
│                                                                 │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐             │
│  │  settings   │  │ attachments │  │  reactions  │             │
│  └─────────────┘  └─────────────┘  └─────────────┘             │
│                                                                 │
│  Signal Protocol Keys:                                          │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐             │
│  │local_identity│ │remote_idents│  │  prekeys    │             │
│  └─────────────┘  └─────────────┘  └─────────────┘             │
│                                                                 │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐             │
│  │signed_prekeys│ │kyber_prekeys│  │  sessions   │             │
│  └─────────────┘  └─────────────┘  └─────────────┘             │
│                                                                 │
│  ┌─────────────┐  ┌─────────────┐                              │
│  │ sender_keys │  │dm_conversat.│                              │
│  └─────────────┘  └─────────────┘                              │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Key Tables

**messages** (v004):
```sql
CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL REFERENCES channels(id),
    sender_id TEXT,              -- NULL for sealed sender
    content_type INTEGER NOT NULL,
    content_json TEXT NOT NULL,
    thread_id TEXT,
    reply_to TEXT,
    attachments_json TEXT NOT NULL DEFAULT '[]',
    is_pinned INTEGER NOT NULL DEFAULT 0,
    is_edited INTEGER NOT NULL DEFAULT 0,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    edited_at TEXT,
    reaction_counts_json TEXT NOT NULL DEFAULT '[]',
    current_user_reactions_json TEXT NOT NULL DEFAULT '[]',
    mentioned_integrations_json TEXT NOT NULL DEFAULT '[]',
    targets_ai_bot INTEGER NOT NULL DEFAULT 0
);

-- Key indexes
CREATE INDEX idx_messages_channel_created ON messages(channel_id, created_at DESC);
```

**Signal Key Tables** (v010):
```sql
-- Local device identity (single row, id=1)
CREATE TABLE local_identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    registration_id INTEGER NOT NULL,
    identity_key_pair BLOB NOT NULL,
    created_at TEXT NOT NULL
);

-- Remote identity keys (TOFU - trust on first use)
CREATE TABLE remote_identities (
    address TEXT PRIMARY KEY,      -- format: "user_id:device_id"
    identity_key BLOB NOT NULL,
    first_seen_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Double Ratchet sessions
CREATE TABLE sessions (
    address TEXT PRIMARY KEY,
    record BLOB NOT NULL,
    updated_at TEXT NOT NULL
);

-- Group sender keys
CREATE TABLE sender_keys (
    sender TEXT NOT NULL,
    distribution_id TEXT NOT NULL,
    record BLOB NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (sender, distribution_id)
);
```

---

## State Management

### Reactive Store Pattern

All state uses `Store<T>` - a reactive container with subscriptions:

```rust
// src/state/store.rs
pub struct Store<T> {
    data: RwLock<T>,
    tx: broadcast::Sender<T>,
}

impl<T: Clone + Send + 'static> Store<T> {
    pub fn get(&self) -> T { ... }
    pub fn set(&self, value: T) { ... }
    pub fn update(&self, f: impl FnOnce(&mut T)) { ... }
    pub fn subscribe(&self) -> broadcast::Receiver<T> { ... }
}
```

### State Types

| State | Purpose | Key Fields |
|-------|---------|------------|
| `AuthState` | Login status | `user_id`, `device_id`, `display_name`, `is_logged_in` |
| `ChatState` | Current view | `selected_channel`, `selected_dm`, `pending_attachments` |
| `UIState` | Preferences | `theme`, `compact_mode`, `active_modal` |
| `MessageState` | Pagination | `cursors`, `has_more` |
| `PresenceState` | Online status | `user_presences` |
| `OfflineState` | Offline queue | `pending_messages` |

### State Flow

```
User action
    │
    ▼
Callback in features/*.rs
    │
    ▼
state.update(|s| s.field = new_value)
    │
    ▼
Store broadcasts to subscribers
    │
    ▼
UI updates automatically (Slint bindings)
```

---

## Error Handling

### Error Type Hierarchy

```
ServiceError                    (business logic)
    ├── Storage(StorageError)   (database)
    ├── Network(NetworkError)   (gRPC)
    ├── Crypto(CryptoError)     (encryption)
    ├── Validation(String)      (input validation)
    ├── NotLoggedIn             (auth required)
    └── ...

CryptoError                     (src/error/crypto_error.rs)
    ├── EncryptionFailed
    ├── DecryptionFailed
    ├── InvalidSenderCertificate
    ├── SealedSenderSelfSend
    ├── InvalidKeyLength
    └── ...

StorageError                    (src/error/storage_error.rs)
    ├── Database(rusqlite::Error)
    ├── Migration(String)
    ├── NotFound
    └── InvalidData(String)

NetworkError                    (src/error/network_error.rs)
    ├── Connection(String)
    ├── Timeout
    ├── Status { code, message }
    └── ...
```

### Error Handling Patterns

```rust
// Convert lower-level errors with ?
async fn send_message(...) -> Result<(), ServiceError> {
    let channel = channel_repo.get(&id)?;  // StorageError → ServiceError
    let encrypted = signal_service.encrypt(...).await?;  // CryptoError → ServiceError
    client.send(...).await.map_err(NetworkError::from)?;
    Ok(())
}

// Handle specific crypto errors
match signal_service.decrypt_message(&ciphertext).await {
    Ok((sender, plaintext)) => { ... }
    Err(ServiceError::Crypto(CryptoError::InvalidSenderCertificate(msg))) => {
        // Certificate validation failed - possible attack
        tracing::warn!("Invalid sender certificate: {}", msg);
    }
    Err(e) => {
        // Other error
        tracing::error!("Decryption failed: {}", e);
    }
}
```

---

## UI Wiring Patterns

### Feature Module Structure

Each `features/*.rs` file handles one area:

```rust
// src/bin_ui/features/chat.rs

/// Wire all chat-related callbacks
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    wire_send_message(window, ctx);
    wire_load_messages(window, ctx);
    wire_scroll_load_more(window, ctx);
}

fn wire_send_message(window: &MainWindow, ctx: &AppContext) {
    let ctx = ctx.clone();
    let window_weak = window.as_weak();

    window.on_send_message(move |channel_id, text| {
        let ctx = ctx.clone();
        let window = window_weak.upgrade().unwrap();

        // Spawn async task
        slint::spawn_local(async move {
            match send_message_impl(&ctx, channel_id, text).await {
                Ok(()) => {
                    // Update UI on success
                    window.set_message_input("".into());
                }
                Err(e) => {
                    // Show error
                    window.set_error_message(e.to_string().into());
                }
            }
        }).unwrap();
    });
}
```

### Callback Wiring Order

All callbacks wired in `features/mod.rs`:

```rust
pub fn wire_all(window: &MainWindow, ctx: &AppContext) {
    auth::wire(window, ctx);       // Login/register
    navigation::wire(window);       // View switching
    chat::wire(window, ctx);       // Message send/receive
    sidebar::wire(window, ctx);    // Channel/DM list
    chat_actions::wire(window, ctx); // Reactions, pins, etc.
    header::wire(window, ctx);     // Header actions
    composer::wire(window, ctx);   // Message composer
    modals::wire(window, ctx);     // Modal dialogs
}
```

### Event Subscription Pattern

```rust
// In auth.rs after successful login
fn subscribe_to_inbox_events(
    provider: Arc<DeviceInboxProvider>,
    window: slint::Weak<MainWindow>,
) {
    let mut rx = provider.subscribe();

    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if let Some(window) = window.upgrade() {
                // Must use invoke_from_event_loop for thread safety
                slint::invoke_from_event_loop(move || {
                    handle_inbox_event(event, &window);
                }).unwrap();
            }
        }
    });
}

fn handle_inbox_event(event: InboxEvent, window: &MainWindow) {
    match event {
        InboxEvent::DmReceived { peer_id, text, timestamp, .. } => {
            // Add message to chat view if this DM is selected
            if window.get_selected_dm_peer_id().as_str() == peer_id.to_string() {
                add_message_to_view(window, &text, timestamp);
            }
        }
        InboxEvent::DmUnreadCountUpdated { peer_id, unread_count, .. } => {
            // Update sidebar badge
            update_dm_badge(window, &peer_id, unread_count);
        }
        _ => {}
    }
}
```

---

## Network Layer

### gRPC Client Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ GrpcClientPool (src/network/grpc/client_pool.rs)               │
│                                                                 │
│ - Manages single channel to server                              │
│ - Provides cloneable channel for clients                        │
│ - Holds AuthInterceptor for JWT injection                       │
└─────────────────────────────────────────────────────────────────┘
                              │
         ┌────────────────────┼────────────────────┐
         │                    │                    │
         ▼                    ▼                    ▼
┌────────────────┐  ┌────────────────┐  ┌────────────────┐
│  AuthClient    │  │ MessageClient  │  │ MailboxClient  │
│                │  │                │  │                │
│ - Login        │  │ - SendMessage  │  │ - SendToMailbox│
│ - Register     │  │ - GetMessages  │  │ - PollMailbox  │
│ - GetPreKeys   │  │ - React        │  │ - Acknowledge  │
│ - GetSenderCert│  │ - Pin/Unpin    │  │                │
└────────────────┘  └────────────────┘  └────────────────┘
```

### AuthInterceptor

Automatically adds JWT to all requests:

```rust
// Added to every gRPC request
Authorization: Bearer <jwt_token>
```

Set after login:
```rust
ctx.auth_interceptor.set_token(token);
```

---

## Key Storage

### Signal Protocol Keys

Stored in SQLCipher database via `KeyStore`:

| Table | Key Type | Purpose |
|-------|----------|---------|
| `local_identity` | Identity key pair | Our long-term identity |
| `remote_identities` | Identity keys | Others' verified identities |
| `prekeys` | One-time prekeys | X3DH key agreement |
| `signed_prekeys` | Signed prekeys | X3DH with signature |
| `kyber_prekeys` | Kyber prekeys | Post-quantum security |
| `sessions` | Session records | Double Ratchet state |
| `sender_keys` | Sender keys | Group encryption |

### Trust Root

**CRITICAL**: The trust root is HARDCODED in `src/config.rs`:

```rust
pub const TRUST_ROOT_PUBLIC_KEY_BYTES: [u8; 32] = [
    0x6b, 0xb8, 0xd7, 0xac, ...
];
```

**Never fetch from server** - this prevents MITM attacks during registration.

### Signal Protocol Audit

For detailed verification of the Signal Protocol implementation against the official spec, see:
**[SIGNAL_PROTOCOL_AUDIT.md](./SIGNAL_PROTOCOL_AUDIT.md)**

Covers: X3DH/PQXDH, Double Ratchet, Sender Keys, Sealed Sender, UAK, Trust Root, PreKey rotation.

---

## Common Mistakes

### 1. Creating Services Manually

```rust
// ❌ WRONG
let message_service = MessageService::new(repo1, repo2, signal_service);

// ✅ CORRECT
let message_service = ctx.message_service();
```

### 2. Creating Another Polling Loop

```rust
// ❌ WRONG - polling already in DeviceInboxProvider
tokio::spawn(async move {
    loop {
        let messages = poll_messages().await;
        // ...
    }
});

// ✅ CORRECT - subscribe to events
let mut rx = provider.subscribe();
while let Ok(event) = rx.recv().await {
    handle_event(event);
}
```

### 3. Calling Crypto Directly

```rust
// ❌ WRONG
let ciphertext = SealedSenderManager::encrypt(...).await?;
let plaintext = decrypt_dm_message(...)?;

// ✅ CORRECT
let ciphertext = signal_service.encrypt_message(...).await?;
let (sender, plaintext) = message_gateway.process_dm_message(...).await?;
```

### 4. Using MessageService for DMs

```rust
// ❌ WRONG - MessageService is for channels only
message_service.send_message(dm_channel_id, text).await?;

// ✅ CORRECT - DMs go through chat.rs flow
// In chat.rs:
let encrypted = signal_service.encrypt_message(&mut auth_client, peer_id, device_id, &plaintext).await?;
let mailbox_encrypted = encrypt_dm_bytes(&encrypted, &shared_secret)?;
mailbox_manager.send_to_mailbox(peer_id, &mailbox_encrypted).await?;
```

### 5. Storing Without Deduplication

```rust
// ❌ WRONG - fails on duplicate
message_repo.insert(&message)?;

// ✅ CORRECT - handles duplicates gracefully
match message_repo.insert_or_ignore(&message)? {
    true => { /* new message */ }
    false => { /* duplicate, already have it */ }
}
```

### 6. Wrong DM Channel ID

```rust
// ❌ WRONG - channel IDs must be deterministic
let channel_id = ChannelId::new();

// ✅ CORRECT - derive from participants
let channel_id = ChannelId::from_dm_participants(&user_a, &user_b);
// Returns same ID regardless of order
```

### 7. Fetching Trust Root from Server

```rust
// ❌ WRONG - defeats the purpose of trust root
let trust_root = auth_client.get_server_trust_root().await?;

// ✅ CORRECT - use hardcoded value
use crate::config::TRUST_ROOT_PUBLIC_KEY;
let trust_root = &*TRUST_ROOT_PUBLIC_KEY;
```

### 8. Blocking the UI Thread

```rust
// ❌ WRONG - blocks UI
window.on_send(move |text| {
    let result = futures::executor::block_on(send_impl(text));  // BLOCKS!
});

// ✅ CORRECT - spawn async task
window.on_send(move |text| {
    slint::spawn_local(async move {
        let result = send_impl(text).await;
        // ...
    }).unwrap();
});
```

---

## Quick Reference

### File Responsibility Matrix

| I want to... | Look in... | Notes |
|--------------|------------|-------|
| Send a DM | `bin_ui/features/chat.rs` | Uses SignalService + MailboxManager |
| Receive a DM | `inbox/provider.rs::poll_dm_mailboxes()` | Decrypts via MessageGateway |
| Send channel message | `services/message_service.rs` | Handles encryption decision |
| Receive channel message | `inbox/provider.rs::process_message()` | Routes via MessageGateway |
| Decrypt any message | `services/message_gateway.rs` | Central decryption router |
| Encrypt for Signal | `services/signal_service.rs` | High-level API |
| Store a message | `storage/repositories/message_repo.rs` | Use insert_or_ignore() |
| Handle incoming events | `bin_ui/features/auth.rs::handle_inbox_event()` | UI updates |
| Rotate sender keys | `services/channel_service.rs` | On leave/kick |
| Bootstrap DM | `mailbox/dm/bootstrap.rs` | Key exchange |
| Get a service | `app/context.rs` | THE source |
| Add a callback | `bin_ui/features/*.rs` | One file per area |

### Service Methods Quick Reference

**SignalService**:
- `encrypt_message()` → DM send (sealed sender)
- `decrypt_message()` → DM receive (validates cert)
- `encrypt_for_group()` → Channel send (sender key)
- `decrypt_from_group()` → Channel receive
- `ensure_session()` → Establish PQXDH session
- `create_sender_key()` → Generate SKDM
- `rotate_sender_key()` → After kick
- `clear_channel_sender_keys()` → After leave

**MessageGateway**:
- `process_dm_message()` → Decrypt both layers
- `route_encrypted_message()` → Route by type
- `process_sender_key_distribution()` → Store SKDM
- `process_incoming_message()` → Sealed sender fallback

**MessageService**:
- `send_message()` → Channel send (NOT DM!)
- `get_channel_encryption_settings()` → Check require_encryption

**ChannelService**:
- `leave_channel()` → Clears local sender keys
- `kick_user()` → Rotates sender key

### Repository Methods Quick Reference

**MessageRepository**:
- `insert()` → New message (fails on duplicate)
- `insert_or_ignore()` → Deduplication (returns bool)
- `get_by_id()` → Single message
- `get_by_channel()` → Paginated list

**ChannelRepository**:
- `insert()` → New channel
- `increment_unread_count()` → Add 1 to unread
- `clear_unread_count()` → Reset to 0
- `get_by_id()` → Single channel

**SettingsRepository**:
- `get::<T>(key)` → Typed get
- `set(key, value)` → Set any serializable
- `get_string(key)` → String shorthand

### InboxEvent Types

| Event | When | Data |
|-------|------|------|
| `DmReceived` | DM decrypted | `peer_id`, `text`, `timestamp` |
| `DmDecryptionFailed` | DM decrypt failed | `peer_id`, `error` |
| `DmUnreadCountUpdated` | New DM stored | `channel_id`, `peer_id`, `unread_count` |
| `DmPollCompleted` | Poll cycle done | `total_messages` |
| `MessageReceived` | Channel message | `message_id`, `envelope_type` |
| `MessageProcessed` | Successfully handled | `message_id` |
| `MessageFailed` | Processing failed | `message_id`, `error` |
| `PollCompleted` | Inbox poll done | `count`, `has_more` |
| `PollFailed` | Inbox poll failed | `error` |

---

## Summary

The Chatter Linux client follows these core principles:

1. **AppContext is the single source of services** - never create manually
2. **DeviceInboxProvider is the only polling loop** - subscribe to events
3. **MessageGateway routes all decryption** - don't call crypto directly
4. **DMs and channels have different flows** - know which is which
5. **Always use insert_or_ignore for received messages** - handle duplicates
6. **Trust root is hardcoded** - never fetch from server
7. **Use async properly** - spawn_local, not block_on
