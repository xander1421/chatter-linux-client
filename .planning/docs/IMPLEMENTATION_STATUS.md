# Implementation Status

Complete status of all features, missing implementations, and system glue.

---

## Table of Contents

1. [Service Layer Status](#service-layer-status)
2. [UI Callback Status](#ui-callback-status)
3. [Missing Implementations](#missing-implementations)
4. [System Glue](#system-glue)
5. [Separation of Concerns Audit](#separation-of-concerns-audit)

---

## Service Layer Status

### Fully Implemented ✅

| Service | File | Status | Notes |
|---------|------|--------|-------|
| SignalService | `signal_service.rs` | ✅ Complete | Encrypt/decrypt, sessions, sender keys |
| MessageService | `message_service.rs` | ✅ Complete | Channel + DM sending |
| MessageGateway | `message_gateway.rs` | ✅ Complete | Decryption routing |
| AttachmentService | `attachment_service.rs` | ✅ Complete | Upload with chunking |
| AuthService | `auth_service.rs` | ✅ Complete | Login/register/logout |
| MailboxManager | `mailbox_manager.rs` | ✅ Complete | DM privacy mailboxes |
| ChannelService | `channel/mod.rs` | ✅ Complete | Channel CRUD, membership |
| UserService | `user_service.rs` | ✅ Complete | Friends, profiles |

### Partially Implemented ⚠️

| Service | Status | Missing |
|---------|--------|---------|
| AttachmentService | ✅ Complete | Upload and download implemented |
| SearchService | ⚠️ Basic | Full-text search works, semantic search stub |
| AIService | ⚠️ Provider-based | Depends on external AI providers |
| NotificationService | ⚠️ Basic | Desktop notifications, no push |

### Group Mailbox Infrastructure ✅

Per NDSS '21 paper: per-group ephemeral mailboxes for traffic analysis resistance.

| Component | Files | Status |
|-----------|-------|--------|
| GroupCoordinator | `mailbox/group/coordination.rs` | ✅ Wired to create/join/invite |
| GroupFanout (Send) | `mailbox/group/fanout.rs` | ✅ Wired to MessageService.send_message() |
| GroupFanout (Receive) | `inbox/polling/group.rs` | ✅ Wired to DeviceInboxProvider |
| GroupMailboxManager | `mailbox/group/member_mailbox.rs` | ✅ State persistence working |

**Bootstrap Flow** (Wired):
1. `create_channel()` / `join_channel()` → `setup_group_mailbox()` → sends announcement
2. Other members receive announcement → store mailbox address

**Send Flow** (Wired - Dual Delivery):
- Messages sent to channel_id via MessageClient (backward compatibility)
- ALSO fan-out to N member mailboxes via GroupFanout (privacy)
- Uses `send_via_group_mailboxes()` after channel send

**Receive Flow** (Wired):
- `poll_group_mailboxes()` called in polling loop
- Messages decrypted via MessageGateway
- Announcements processed via GroupCoordinator

**Note:** Full privacy (server doesn't store group messages) requires server changes. Currently, messages go through both paths during the transition period.

### Other Infrastructure 🔧

| Component | Files | Status |
|-----------|-------|--------|
| OfflineQueue | `services/offline_queue.rs` | 🔧 Exists, not used |

---

## UI Callback Status

### Fully Wired ✅

| Feature | File | Callbacks |
|---------|------|-----------|
| Login/Register | `auth/login.rs`, `register.rs` | `login`, `register`, `go-to-*` |
| Send Message | `chat/send_message.rs` | `send` (DM + channel) |
| Typing Indicator | `chat/typing.rs` | `typing-changed` |
| Reactions | `chat/reactions.rs` | `reaction-selected` |
| Pin Message | `chat/pin.rs` | `pin-message` |
| Edit Message | `chat/edit.rs` | `edit-message`, `send-edit`, `cancel-edit` |
| Delete Message | `chat/delete.rs` | `delete-message`, `delete-everyone-message` |
| Attachments | `composer.rs` | `attachment-clicked`, `remove-attachment` |
| Channel Select | `sidebar.rs` | `channel-selected`, `dm-selected` |
| Settings | `modals/settings.rs` | All settings callbacks |
| Friends | `modals/friends.rs` | Search, request, accept, decline |
| Sessions | `modals/sessions.rs` | `revoke-session`, `revoke-all-sessions` |
| Profile | `modals/profile.rs` | View/edit profile |
| Search | `header.rs` | `search-messages`, `search-result-clicked` |
| Thread View | `thread.rs` | `close-thread`, `send-thread-reply` |
| Emoji Picker | `composer.rs` | `emoji-clicked`, `composer-emoji-selected`, category/search |
| GIF Picker | `composer.rs` | `gif-clicked`, `gif-selected`, `gif-search` (Tenor API) |
| Mention Autocomplete | `composer.rs` | `mention-triggered`, `mention-suggestion-selected` (real members) |

### Wired but UI Not Built ⚠️

| Callback | File | Status |
|----------|------|--------|
| `chat-settings-clicked` | `header.rs` | ⚠️ Logs message, no modal UI |
| `forward-message` | `friend_requests.rs` | ⚠️ Logs message, forwarding not implemented |

### Slint Components Not Wired 🔧

| Component | File | Missing |
|-----------|------|---------|
| InviteMember modal | `invite_member.slint` | Exists, not wired |

---

## Missing Implementations

### High Priority

| Feature | What's Missing | Effort |
|---------|---------------|--------|
| **Message Forwarding** | Wire forward flow, create `ForwardService` | Medium |
| **Channel Settings Modal** | Create Slint UI, wire to `header.rs` | Medium |

### Medium Priority

| Feature | What's Missing | Effort |
|---------|---------------|--------|
| **GIF Preview Images** | Load images from preview_url (async image loading) | Small |
| **Scroll to Message** | Implement `pinned-go-to-message` scroll logic | Small |
| **Invite Member** | Wire `invite_member.slint` to channel service | Small |

### Low Priority / Future

| Feature | What's Missing | Notes |
|---------|---------------|-------|
| **Offline Queue** | Wire `OfflineState` to retry failed sends | Needs design |
| **Key Rotation Automation** | Background service for prekey refresh | Needs threshold logic |
| **Semantic Search** | LocalAI integration for embeddings | Depends on AI service |
| **Push Notifications** | Server-side push, client registration | Server changes needed |
| **Voice/Video Calls** | Full WebRTC implementation | Large effort |

---

## System Glue

### How Components Connect

```
┌─────────────────────────────────────────────────────────────────┐
│                        APPLICATION STARTUP                       │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  main.rs                                                        │
│    │                                                            │
│    ├─→ AppContext::new()                                        │
│    │     ├─→ Initialize system (Directories, Keyring)           │
│    │     ├─→ Open database (SQLCipher)                          │
│    │     ├─→ Create repositories                                │
│    │     ├─→ Create crypto managers                             │
│    │     ├─→ Create network pool                                │
│    │     ├─→ Create services                                    │
│    │     └─→ Create state stores                                │
│    │                                                            │
│    ├─→ MainWindow::new()                                        │
│    │                                                            │
│    └─→ features::wire_all(window, ctx)                          │
│          ├─→ auth::wire()                                       │
│          ├─→ navigation::wire()                                 │
│          ├─→ chat::wire()                                       │
│          ├─→ sidebar::wire()                                    │
│          ├─→ composer::wire()                                   │
│          ├─→ header::wire()                                     │
│          ├─→ chat_actions::wire()                               │
│          ├─→ thread::wire()                                     │
│          └─→ modals::wire()                                     │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Post-Login Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                         AFTER LOGIN                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  auth/login.rs: on_login()                                      │
│    │                                                            │
│    ├─→ AuthService::login()                                     │
│    │     ├─→ Server auth                                        │
│    │     ├─→ Set JWT token                                      │
│    │     └─→ Load/create identity                               │
│    │                                                            │
│    ├─→ ctx.init_device_inbox_provider()                         │
│    │     └─→ Creates DeviceInboxProvider (OnceLock)             │
│    │                                                            │
│    ├─→ ctx.init_group_fanout()                                  │
│    │     └─→ Creates GroupFanout (OnceLock)                     │
│    │                                                            │
│    ├─→ ctx.spawn_dm_bootstrap_handler()                         │
│    │     └─→ Listens for KeyExchange events                     │
│    │                                                            │
│    ├─→ Start inbox polling (background task)                    │
│    │     └─→ provider.run_loop()                                │
│    │                                                            │
│    ├─→ Subscribe to InboxEvents                                 │
│    │     └─→ Updates UI on message receive                      │
│    │                                                            │
│    └─→ Load initial data (channels, DMs, friends)               │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Message Send Flow (Complete)

```
┌─────────────────────────────────────────────────────────────────┐
│                       MESSAGE SEND FLOW                          │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  UI: User clicks Send                                           │
│    │                                                            │
│    ▼                                                            │
│  send_message.rs: on_send(text)                                 │
│    │                                                            │
│    ├─→ Is DM? (check selected_dm_id)                            │
│    │     │                                                      │
│    │     ▼ YES                                                  │
│    │   MessageService::send_dm_text()                           │
│    │     ├─→ Build DataMessage proto                            │
│    │     ├─→ SignalService::encrypt_message() [Layer 1]         │
│    │     ├─→ DmCrypto::encrypt_dm_bytes() [Layer 2]             │
│    │     └─→ MailboxManager::send_to_mailbox()                  │
│    │                                                            │
│    └─→ Is Channel?                                              │
│          │                                                      │
│          ▼ YES                                                  │
│        MessageService::send_message()                           │
│          ├─→ Check channel.config.require_encryption            │
│          ├─→ SignalService::encrypt_for_group() [if encrypted]  │
│          ├─→ Distribute SKDM if first message                   │
│          └─→ MessageClient::send_message()                      │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Message Receive Flow (Complete)

```
┌─────────────────────────────────────────────────────────────────┐
│                      MESSAGE RECEIVE FLOW                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  DeviceInboxProvider::run_loop()                                │
│    │                                                            │
│    ├─→ poll_device_inbox()                                      │
│    │     ├─→ Fetch messages from server                         │
│    │     └─→ For each message:                                  │
│    │           │                                                │
│    │           ├─→ Route by associated_data marker              │
│    │           │     ├─→ "sender_key_distribution" → Store SKDM │
│    │           │     ├─→ "sender_key" → Decrypt with sender key │
│    │           │     └─→ "sender_key_request" → Send our SKDM   │
│    │           │                                                │
│    │           ├─→ Parse Content proto                          │
│    │           ├─→ Store in MessageRepository                   │
│    │           └─→ Emit InboxEvent                              │
│    │                                                            │
│    └─→ poll_dm_mailboxes()                                      │
│          ├─→ For each DM conversation:                          │
│          │     ├─→ Poll peer's send mailbox                     │
│          │     └─→ For each message:                            │
│          │           │                                          │
│          │           ├─→ MessageGateway::process_dm_message()   │
│          │           │     ├─→ Decrypt Layer 2 (mailbox)        │
│          │           │     └─→ Decrypt Layer 1 (Signal)         │
│          │           │                                          │
│          │           ├─→ Parse Content proto                    │
│          │           ├─→ Store message                          │
│          │           └─→ Emit InboxEvent::DmReceived            │
│          │                                                      │
│          └─→ Emit InboxEvent::DmPollCompleted                   │
│                                                                 │
│  ─────────────────────────────────────────────────────────────  │
│                                                                 │
│  auth/inbox_events.rs: handle_inbox_event()                     │
│    │                                                            │
│    ├─→ DmReceived → Add to chat if selected                     │
│    ├─→ DmUnreadCountUpdated → Update sidebar badge              │
│    ├─→ MessageProcessed → Refresh channel if selected           │
│    └─→ PollFailed → Log error                                   │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Content Builder Registry

All content types use builders from `message_gateway.rs`:

| Builder | Content Type | Used For |
|---------|--------------|----------|
| `build_text_content()` | DataMessage | Text messages |
| `build_typing_content()` | TypingMessage | Typing indicators |
| `build_typing_content_with_channel()` | TypingMessage | Channel typing |
| `build_read_receipt_content()` | ReceiptMessage | Read receipts |
| `build_delivery_receipt_content()` | ReceiptMessage | Delivery receipts |
| `build_reaction_content()` | Reaction | DM reactions |
| `build_reaction_content_with_channel()` | Reaction | Channel reactions |
| `build_pin_content()` | PinMessage | DM pin/unpin |
| `build_pin_content_with_channel()` | PinMessage | Channel pin/unpin |
| `build_edit_content()` | EditMessage | DM message edits |
| `build_edit_content_with_channel()` | EditMessage | Channel edits |
| `build_delete_for_me_content()` | Delete | Local delete |
| `build_delete_for_everyone_content()` | Delete | DM delete for all |
| `build_delete_for_everyone_content_with_channel()` | Delete | Channel delete |

---

## Separation of Concerns Audit

### ✅ PASS - No Violations Found

| Check | Result |
|-------|--------|
| Proto building in UI | ✅ None - uses `build_*_content()` |
| Direct crypto calls in UI | ✅ None - uses services |
| `encode_to_vec()` in UI | ✅ None |
| `SealedSender*` in UI | ✅ None |
| `encrypt_dm_bytes` in UI | ✅ None |
| File chunking in UI | ✅ None - uses AttachmentService |
| `ctx.key_store` in UI | ✅ Only for DI to AuthService |

### Proto Usage in UI (Acceptable)

These are acceptable proto usages (data types, not building content):

| File | Usage | Why Acceptable |
|------|-------|----------------|
| `chat/helpers.rs` | `Content` type | Passing to service methods |
| `auth/initial_data.rs` | `friend_requests_filter` | API filter enum |
| `modals/create_channel.rs` | `IntegrationType` | Enum for AI provider |
| `modals/friends.rs` | `user_event` | Event handling |

### SignalService Usage in UI (Correct)

| File | Usage | Why Correct |
|------|-------|-------------|
| `modals/settings.rs` | `get_identity_fingerprint()` | High-level API |
| `modals/settings.rs` | `refresh_prekeys()` | High-level API |
| `auth/login.rs` | Passed to services | Dependency injection |
| `modals/create_channel.rs` | `create_sender_key()` | High-level API |

---

## Summary

### What Works ✅

- Full message sending (DM + channel with encryption)
- Full message receiving (two-layer DM, sender key channel)
- Attachments (upload + download with chunking)
- All auth flows (login, register, logout)
- Friends system (search with friend/pending status, request, accept, decline)
- Channel management (create, join, leave)
- Settings and preferences
- Session management
- Search (basic full-text)
- Reactions, pins, edits, deletes
- Thread view
- Emoji picker (categories, search, insertion)
- GIF picker (Tenor API v2 integration)
- Mention autocomplete (real channel member search)
- Group mailbox fanout (NDSS '21 privacy)

### What's Missing ⚠️

- GIF preview images (async image loading from URLs)
- Message forwarding
- Channel settings modal
- Offline queue (infrastructure exists)

### Architecture Quality ✅

- Clean separation of concerns
- Services properly encapsulate business logic
- UI only calls services, never crypto directly
- All content building uses helpers
- Single polling loop (DeviceInboxProvider)
- Event-driven UI updates
