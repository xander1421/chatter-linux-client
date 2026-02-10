# Feature Audit: Sealed Sender Coverage

> **Purpose**: Verify all E2E features use sealed sender envelope as per proto design.

---

## Infrastructure Status: COMPLETE ✅

The Signal Protocol infrastructure is **fully implemented** in `MessageGateway`:

| Component | Location | Status |
|-----------|----------|--------|
| DM Decryption (2-layer) | `message_gateway.rs:process_dm_message()` | ✅ Ready |
| Sealed Sender Decrypt | `message_gateway.rs:process_incoming_message()` | ✅ Ready |
| Sender Key Decrypt | `message_gateway.rs:process_group_encrypted_message()` | ✅ Ready |
| SKDM Processing | `message_gateway.rs:process_sender_key_distribution()` | ✅ Ready |
| SKDM Request Handling | `message_gateway.rs:process_sender_key_request()` | ✅ Ready |
| Message Routing | `message_gateway.rs:route_encrypted_message()` | ✅ Ready |
| DM Send (Signal + Mailbox) | `chat.rs` → `signal_service.encrypt_message()` | ✅ Ready |
| Channel Send (Sender Key) | `message_service.rs` → `signal_service.encrypt_for_group()` | ✅ Ready |

**The remaining work is NOT about building infrastructure - it's about making features USE the existing infrastructure instead of calling server APIs.**

---

## Proto Design (chat.v1.content.rs)

The `Content` proto is the **universal envelope for ALL E2E encrypted content**:

```
Content (protobuf) → Sealed Sender Encrypt → Ephemeral Mailbox Delivery
```

### Content oneof - ALL should go through sealed sender:

| Tag | Type | Description | Receive | Send | Notes |
|-----|------|-------------|---------|------|-------|
| 1 | `DataMessage` | Text, quotes, delete-for-everyone | ✅ | ✅ | `build_text_content()` |
| 2 | `TypingMessage` | Typing indicators | ✅ | ✅ | `build_typing_content()` |
| 3 | `ReceiptMessage` | Read/delivery receipts | ✅ | ✅ | `build_read_receipt_content()` |
| 4 | `Reaction` | Emoji reactions | ✅ | ✅ | `build_reaction_content()` |
| 5 | `PinMessage` | Pin/unpin messages | ✅ | ✅ | `build_pin_content()` |
| 6 | `EditMessage` | Edit messages | ✅ | ✅ | `build_edit_content()` |
| 7 | `KeyExchange` | Ephemeral key exchange | ✅ | ✅ | Via mailbox bootstrap |
| 8 | `AttachmentMessage` | File pointers | ⚠️ | ⚠️ | In DataMessage |
| 9 | `SenderKeyDistribution` | SKDM | ✅ | ✅ | Via device inbox |
| 10 | `GroupEncrypted` | Sender key content | ✅ | ✅ | Channel messages |
| 11 | `SenderKeyRequest` | Request SKDM | ⚠️ | ⚠️ | Stub only |
| 12 | `GroupMailboxAddress` | Group mailbox exchange | ❌ | ❌ | Not implemented |
| 13 | `CallKeyExchange` | Call E2EE keys | ❌ | ❌ | Out of scope |
| 14 | `DeleteForMeSync` | Multi-device delete sync | ✅ | ✅ | `build_delete_for_me_content()` |

---

## Feature Status: Registration → Delete

### 1. Registration & Auth

| Feature | Sealed Sender? | Status | Notes |
|---------|----------------|--------|-------|
| Register | N/A | ✅ Done | Server API, no encryption needed |
| Login | N/A | ✅ Done | Server API, returns JWT |
| Logout | N/A | ✅ Done | Server API |
| PreKey upload | N/A | ✅ Done | Server API (keys are public) |
| Device management | N/A | ✅ Done | Server API |

### 2. Session Establishment

| Feature | Sealed Sender? | Status | Notes |
|---------|----------------|--------|-------|
| Fetch PreKey bundle | N/A | ✅ Done | Server API |
| X3DH/PQXDH handshake | N/A | ✅ Done | Local crypto |
| Session storage | N/A | ✅ Done | Local SQLCipher |

### 3. Direct Messages (DMs)

| Feature | Should Use | Receive | Send | Location |
|---------|------------|---------|------|----------|
| **Send text** | Sealed + Mailbox | ✅ | ✅ | `chat.rs` → `build_text_content()` |
| **Receive text** | Sealed + Mailbox | ✅ | - | `provider.rs` → `DmContent::Text` |
| Typing | Sealed + Mailbox | ✅ | ✅ | `build_typing_content()` |
| Read receipt | Sealed + Mailbox | ✅ | ✅ | `build_read_receipt_content()` |
| Delivery receipt | Sealed + Mailbox | ✅ | ✅ | `build_delivery_receipt_content()` |
| Edit message | Sealed + Mailbox | ✅ | ✅ | `build_edit_content()` |
| Delete for everyone | Sealed + Mailbox | ✅ | ✅ | `build_delete_for_everyone_content()` |
| Delete for me | Sealed (sync) | ✅ | ✅ | `build_delete_for_me_content()` |
| React to message | Sealed + Mailbox | ✅ | ✅ | `build_reaction_content()` |
| Pin message | Sealed + Mailbox | ✅ | ✅ | `build_pin_content()` |

### 4. Channel Messages

| Feature | Should Use | Status | Location |
|---------|------------|--------|----------|
| **Send text** | Sender Key | ✅ Done | `message_service.rs` → `signal_service.encrypt_for_group()` |
| **Receive text** | Sender Key | ✅ Done | `provider.rs` → `route_encrypted_message()` |
| **SKDM distribute** | Device inbox | ✅ Done | `message_service.rs` → `distribute_sender_key()` |
| **SKDM process** | Device inbox | ✅ Done | `message_gateway.rs` |
| **Key rotation** | - | ✅ Done | `channel_service.rs` on leave/kick |
| Send typing | Sender Key | ✅ Done | `chat.rs` → `wire_typing_changed()` → `send_channel_content()` |
| Edit message | Sender Key | ✅ Done | `chat.rs` → `wire_send_edit()` → `send_channel_content()` |
| Delete for everyone | Sender Key | ✅ Done | `chat.rs` → `wire_delete_everyone_message()` → `send_channel_content()` |
| Delete for me | Sender Key | ✅ Done | `chat.rs` → `wire_delete_message()` → `send_channel_content()` |
| React to message | Sender Key | ✅ Done | `chat.rs` → `wire_reaction_selected()` → `send_channel_content()` |
| Pin message | Sender Key | ✅ Done | `chat.rs` → `wire_pin_message()` → `send_channel_content()` |

**Channel control messages** now use sender key encryption via `MessageService::send_control_content()`. Control messages (typing, reactions, edits, deletes, pins) are encrypted with sender key and sent via the `SendMessage` RPC with encrypted content.

### 5. Attachments

| Feature | Should Use | Status | Notes |
|---------|------------|--------|-------|
| Upload encrypted | AES-GCM | ✅ Done | `attachment_service.rs` |
| Download decrypt | AES-GCM | ✅ Done | `attachment_service.rs` |
| Send pointer (DM) | Sealed + Mailbox | ⚠️ Partial | Pointer in DataMessage, but not full Content envelope |
| Send pointer (Channel) | Sender Key | ⚠️ Partial | Same |

---

## Gap Analysis

### Critical Gaps: RESOLVED ✅

All critical privacy gaps have been addressed:

1. ~~**Typing indicators via server API**~~ → ✅ Now uses sealed sender (DM) / sender key (channel)
2. ~~**Read receipts via server API**~~ → ✅ Now uses sealed sender (DM only, channel read receipts N/A)
3. ~~**Reactions via server API**~~ → ✅ Now uses sealed sender (DM) / sender key (channel)
4. ~~**Edit/Delete via server API**~~ → ✅ Now uses sealed sender (DM) / sender key (channel)
5. ~~**Pin operations via server API**~~ → ✅ Now uses sealed sender (DM) / sender key (channel)

### Remaining Gaps (Functionality)

1. **SenderKeyRequest not fully implemented**
   - Stub exists but not wired for actual key requests
   - Low priority: SKDM distribution handles most cases

2. **GroupMailboxAddress not implemented**
   - Needed for anonymous group messaging
   - Future feature for enhanced channel privacy

### Current Coverage

```
Properly Encrypted (via Sealed Sender / Sender Key):
  ✅ DM text send/receive
  ✅ Channel text send/receive
  ✅ Sender key distribution
  ✅ File encryption (content)
  ✅ DM bootstrap key exchange
  ✅ Typing indicators (DM + Channel)
  ✅ Read receipts (DM)
  ✅ Reactions (DM + Channel)
  ✅ Edit messages (DM + Channel)
  ✅ Delete for everyone (DM + Channel)
  ✅ Delete for me (DM + Channel)
  ✅ Pin messages (DM + Channel)
```

---

## Required Changes

> **Key Insight**: No new crypto infrastructure needed. Just wire features to use existing `MessageGateway` paths.

### Phase A: DM Content Types (Sealed Sender + Mailbox)

Each feature needs to follow the **existing pattern** in `chat.rs`:
1. Create `Content` proto with specific type (e.g., `TypingMessage`)
2. Serialize to bytes with `encode_to_vec()`
3. Encrypt via `signal_service.encrypt_message()` (sealed sender)
4. Wrap with `encrypt_dm_bytes()` (mailbox layer)
5. Send via `mailbox_manager.send_to_mailbox()`

**Receive side already works** - `MessageGateway::process_dm_message()` decrypts and returns plaintext. Just need to parse Content proto and dispatch by type.

| Content Type | Send Function Needed | Receive Handler |
|--------------|---------------------|-----------------|
| `TypingMessage` | `send_dm_typing()` | `typing_handler.rs` |
| `ReceiptMessage` | `send_dm_receipt()` | `receipt_handler.rs` |
| `Reaction` | `send_dm_reaction()` | `reaction_handler.rs` |
| `EditMessage` | `send_dm_edit()` | `edit_handler.rs` |
| `DataMessage.delete` | `send_dm_delete()` | via `chat_handler.rs` |
| `PinMessage` | `send_dm_pin()` | `pin_handler.rs` |
| `DeleteForMeSync` | `send_delete_for_me_sync()` | `sync_handler.rs` ✅ |

### Phase B: Channel Content Types (Sender Key)

Each feature needs:
1. Create `Content` proto with specific type
2. Serialize to bytes
3. `SignalService::encrypt_for_group()`
4. Send via device inbox or group mailbox

Same content types as DM but through sender key path.

### Phase C: Content Envelope Standardization

Currently `DataMessage` is sent directly. Should be wrapped:

```rust
// Current (partial)
let data_message = DataMessage { body, timestamp, .. };
let plaintext = data_message.encode_to_vec();

// Should be (full Content envelope)
let content = Content {
    timestamp: now_millis(),
    padding: generate_padding(plaintext_size),
    content: Some(content::Content::DataMessage(data_message)),
    ..Default::default()
};
let plaintext = content.encode_to_vec();
```

This enables:
- Traffic analysis protection (padding)
- Consistent handling of all content types
- Ephemeral reply-to addresses

---

## Implementation Priority

### P0: Security Critical
1. Wrap all messages in `Content` envelope with padding
2. Send typing via sealed sender (leaks timing/relationship)
3. Send read receipts via sealed sender (leaks read status)

### P1: Privacy Important
4. Send reactions via sealed sender
5. Send edit/delete via sealed sender
6. Send pin operations via sealed sender

### P2: Multi-device
7. Implement `DeleteForMeSync` send
8. Implement other sync message types

### P3: Group Privacy
9. Implement `GroupMailboxAddress` for anonymous groups
10. Full `SenderKeyRequest` flow

---

## Verification Checklist

Implementation status:

- [x] All `Content` oneof types can be sent via sealed sender (DM)
- [x] All `Content` oneof types can be sent via sender key (channel)
- [ ] Server cannot see content type (all padded to bucket sizes) - padding not yet implemented
- [x] Typing indicators don't leak to server
- [x] Read receipts don't leak to server (DM only)
- [x] Reactions don't leak to server
- [x] Edit operations don't leak original content
- [x] Delete operations don't leak what was deleted
- [x] Multi-device sync works for delete-for-me

---

## Summary

**Infrastructure**: Complete ✅ - `MessageGateway` handles all encryption/decryption paths.

**Receive side**: Complete ✅ - All Content types parsed and dispatched:

DM via `DmContent` enum (sealed sender + mailbox):
- `DmContent::Text` → `InboxEvent::DmReceived`
- `DmContent::Typing` → `InboxEvent::DmTyping`
- `DmContent::Receipt` → `InboxEvent::DmReceipt`
- `DmContent::Reaction` → `InboxEvent::DmReaction`
- `DmContent::Pin` → `InboxEvent::DmPin`
- `DmContent::Edit` → `InboxEvent::DmEdit`
- `DmContent::DeleteForMe` → `InboxEvent::DmDeleteForMe`

Channel via `RouteResult::ControlContent` (sender key):
- `content::Content::Typing` → `InboxEvent::ChannelTyping`
- `content::Content::Reaction` → `InboxEvent::ChannelReaction`
- `content::Content::Pin` → `InboxEvent::ChannelPin`
- `content::Content::Edit` → `InboxEvent::ChannelEdit`
- `content::Content::DataMessage` (with delete) → `InboxEvent::ChannelDeleteForEveryone`
- `content::Content::DeleteForMeSync` → `InboxEvent::ChannelDeleteForMe`

**Send side**: Complete ✅ - Content builders available in `message_gateway.rs`:
- `build_text_content()`, `build_typing_content()`, `build_typing_content_with_channel()`
- `build_read_receipt_content()`, `build_delivery_receipt_content()`
- `build_reaction_content()`, `build_reaction_content_with_channel()`
- `build_pin_content()`, `build_pin_content_with_channel()`
- `build_edit_content()`, `build_edit_content_with_channel()`
- `build_delete_for_me_content()`, `build_delete_for_everyone_content()`, `build_delete_for_everyone_content_with_channel()`

**Channel Context Embedding**:
- Channel control messages embed `channel_id` INSIDE the encrypted Content proto
- Server never sees channel context for control messages
- `_with_channel` variants set the channel_id field in protos that have it
- Reaction proto lacks channel_id field - encoded in target_author_aci as `"channel:{channel_id}:{author_aci}"`

**Usage pattern**:
```rust
// Build Content proto
let content = build_typing_content(true); // started typing
let plaintext = content.encode_to_vec();

// Encrypt with Signal Protocol
let sealed = signal_service.encrypt_message(&mut auth, recipient_id, device_id, &plaintext).await?;

// Encrypt for mailbox
let encrypted = encrypt_dm_bytes(&sealed, &shared_secret)?;

// Send to mailbox
mailbox_manager.send_to_mailbox(&peer_mailbox_pk, encrypted, EnvelopeType::SealedSender).await?;
```

**UI Wiring Progress**:
- ✅ Typing indicators: `wire_typing_changed()` in `chat.rs`
  - DM: sealed sender via `send_dm_content()`
  - Channel: sender key via `send_channel_content()`
- ✅ Reactions: `wire_reaction_selected()` in `chat.rs`
  - DM: sealed sender via `send_dm_content()`
  - Channel: sender key via `send_channel_content()`
  - ReactionPicker component integrated in app.slint
  - Looks up message to get proper sender_id and timestamp
- ✅ Read receipts: `send_dm_read_receipts()` in `chat.rs` - sent when DM is opened
  - Triggered from `wire_dm_selected()` in sidebar.rs
  - Collects timestamps from recent peer messages
- ✅ Context menu: `MessageContextMenu` component in `message_context_menu.slint`
  - Shows reply, edit, delete, pin, forward, copy options
  - `wire_more_clicked()` - looks up message ownership and pin status
  - `wire_reply_message()` - sets composer placeholder for reply
  - `wire_edit_message()` - enters edit mode with original text in composer
  - `wire_send_edit()` - DM: sealed sender, Channel: sender key
  - `wire_cancel_edit()` - exits edit mode without sending
  - `wire_delete_message()` - DM: sealed sender, Channel: sender key (delete for me)
  - `wire_delete_everyone_message()` - DM: sealed sender, Channel: sender key
  - `wire_pin_message()` - DM: sealed sender, Channel: sender key
  - `wire_forward_message_action()` - opens forward modal with preview
  - `wire_copy_message()` - copies message text to clipboard
- ✅ Edit mode UI: Edit bar in `message_composer.slint`
  - Shows "Editing message" indicator with cancel button
  - Composer pre-populated with original text

**Receive-side UI Updates** (in `auth.rs`):
- ✅ Typing indicators: Shows "User is typing..." in header for selected conversation
- ✅ Message edits: Finds message by `timestamp_ms`, updates content, marks `is_edited`
- ✅ Delete for everyone: Finds message, marks `is_deleted`, shows "This message was deleted"
- ✅ Delete for me: Same as above but shows "Message deleted"
- ✅ Reactions: Parses/updates reaction_summary string (e.g., "👍 3  ❤️ 2")
- ✅ Read receipts: Updates `delivery_status` field ("sent", "delivered", "read") for own messages
  - Receipt types: 1=Delivery, 2=Read, 3=Viewed
  - Matches messages by timestamp_ms to find affected messages
- ✅ Pins: Updates PinnedBar state (total count, preview, current index)
  - Finds message content by timestamp_ms for preview text
  - Handles pin/unpin for both DM and channel conversations

**Helper functions** in `chat.rs`:
- `send_dm_content()` - reusable for all DM content types (sealed sender)
- `send_channel_content()` - reusable for all channel content types (sender key)
- `send_dm_read_receipts()` - batch send read receipts for a conversation

---

## NDSS 2021: Ephemeral Mailbox System

The implementation follows the protocol from **"Improving Signal's Sealed Sender"** (NDSS 2021) to prevent Statistical Disclosure Attacks.

### The Attack Problem

Standard sealed sender hides the sender from the server, but **delivery receipts** create a timing correlation:
1. Alice sends sealed message to Bob's device inbox
2. Bob sends receipt back to Alice's device inbox
3. Server correlates: `sealed_message(T) → receipt(T+δ) → Alice & Bob communicating`

Within 5-10 messages, the server can de-anonymize sealed sender users.

### The Solution: Ephemeral Mailboxes

Instead of using device inboxes, both parties communicate via **anonymous ephemeral mailboxes**:

```
Alice                    Server                     Bob
  |                         |                         |
  |-- Create mailbox ------>| (blind signature)       |
  |<-- mailbox_A_pk --------|                         |
  |                         |                         |
  |-- Bootstrap (sealed) -->| --> device_inbox_B      |
  |   [mailbox_A_pk]        |                         |
  |                         |                         |
  |                         |<-- Create mailbox ------|
  |                         |<-- mailbox_B_pk --------|
  |                         |                         |
  |    mailbox_A <----------|<-- Response ------------|
  |                [mailbox_B_pk]                     |
  |                         |                         |
  |-- Send msg ------------>| mailbox_B              |
  |                         |                         |
  |    mailbox_A <----------|<-- Receipt -------------|
```

Server only sees: `anonymous_mailbox_A ↔ anonymous_mailbox_B`

### Implementation Modules

| Module | Location | Purpose |
|--------|----------|---------|
| **Blind Signatures** | `mailbox/blind_signature.rs` | RFC 9474 RSABSSA for anonymous mailbox registration |
| **Token Manager** | `mailbox/token_manager.rs` | Quota-limited blind signature token acquisition |
| **DM Bootstrap** | `mailbox/dm/bootstrap.rs` | First-contact protocol, mailbox exchange |
| **DM Conversation** | `mailbox/dm/conversation.rs` | Per-conversation mailbox and key state |
| **Two-Way Anonymity** | `mailbox/dm/two_way.rs` | Bidirectional messaging via mailboxes |
| **Key Rolling** | `mailbox/key_rolling.rs` | Periodic mailbox rotation for forward secrecy |
| **Group Mailboxes** | `mailbox/group/member_mailbox.rs` | Per-member group mailboxes |
| **Group Fanout** | `mailbox/group/fanout.rs` | N-way message delivery |

### Traffic Analysis Resistance

Additional layers beyond basic ephemeral mailboxes:

| Feature | Module | Description |
|---------|--------|-------------|
| **Content Padding** | `mailbox/content.rs` | Log-bucketing pads messages to discrete size buckets |
| **Timing Obfuscation** | `mailbox/timing.rs` | Random delays (50-500ms) before send/poll operations |
| **Cover Traffic** | `mailbox/cover_traffic.rs` | Fake messages at random intervals (exponential distribution) |
| **Dummy Polling** | `mailbox/poller.rs` | 20% of polls go to decoy mailboxes (random Ed25519 keys) |

### Protocol Flow: DM Bootstrap

```rust
// 1. Alice gets blind-signed token
let token = token_manager.get_available_token()?;

// 2. Alice creates ephemeral mailbox (Ed25519 keypair)
let mailbox = DmMailbox {
    public_key,  // 32 bytes - mailbox address
    private_key, // 64 bytes - for challenge-response auth
    signature: token.signature,  // Unblinded RSA signature
    expires_at: now + 7_days,
};

// 3. Alice sends bootstrap via sealed sender to Bob's DEVICE inbox
let bootstrap_msg = [mailbox.public_key || conversation_ephemeral_pk];  // 64 bytes
signal_service.encrypt_message(bootstrap_msg).await?;  // Sealed sender

// 4. Bob receives, creates his own mailbox, sends response to Alice's MAILBOX
let response = [bob_mailbox.public_key || bob_ephemeral_pk];
two_way.send_to_peer(&alice_mailbox_pk, response, client).await?;
```

### Challenge-Response Authentication

When polling a mailbox, the client must prove ownership:

```rust
// Client requests challenge
let challenge = client.get_mailbox_challenge(mailbox.public_key).await?;

// Client signs challenge with mailbox private key
let signature = mailbox.sign_challenge(&challenge)?;

// Server verifies Ed25519 signature before returning messages
let messages = client.poll_mailbox(mailbox.public_key, signature).await?;
```

### Key Rotation Schedule

| Component | Default | Notes |
|-----------|---------|-------|
| Mailbox TTL | 7 days | Server deletes after expiry |
| Rotation threshold | 24 hours before expiry | Client initiates rotation |
| Overlap period | 1 hour | Old mailbox accepts messages during transition |
| Token quota | 10/day | Prevents DoS via unlimited mailbox creation |

### Security Properties

| Property | How Achieved |
|----------|--------------|
| **Sender Anonymity** | Sealed sender hides identity from server |
| **Receiver Anonymity** | Ephemeral mailbox hides recipient identity |
| **Unlinkability** | Blind signatures prevent linking token request to mailbox |
| **Forward Secrecy** | Key rolling rotates mailboxes periodically |
| **DoS Prevention** | Token quota limits mailbox creation rate |
| **Replay Protection** | Challenge-response with fresh nonces |
| **Traffic Analysis Resistance** | Padding, timing obfuscation, cover traffic, dummy polls |

### Configuration

```rust
// TimingConfig (mailbox/timing.rs)
TimingConfig {
    enabled: true,              // Enabled by default
    min_delay_ms: 50,           // Minimum pre-operation delay
    max_delay_ms: 500,          // Maximum pre-operation delay
    jitter_percent: 20,         // Polling interval jitter
}

// CoverTrafficConfig (mailbox/cover_traffic.rs)
CoverTrafficConfig {
    enabled: false,             // User opt-in (battery impact)
    base_interval: 60s,         // Mean time between fake messages
    min_interval: 10s,
    max_interval: 300s,
}

// PollerConfig (mailbox/poller.rs)
PollerConfig {
    poll_interval: 30s,         // Base polling interval (jittered)
    dummy_polling_enabled: true,
    dummy_poll_probability: 0.2, // 20% decoy polls
}
```

### NDSS21 Compliance Summary

| Requirement | Status | Implementation |
|-------------|--------|----------------|
| Ephemeral mailboxes | ✅ | `DmMailbox`, `GroupMailbox` |
| Blind signatures | ✅ | RFC 9474 RSABSSA |
| One-way sealed sender | ✅ | `DmBootstrap.initiate_dm()` |
| Two-way sealed sender | ✅ | `TwoWayAnonymity` after bootstrap |
| Key rolling | ✅ | `KeyRollingService` |
| Challenge-response auth | ✅ | Ed25519 signature on nonce |
| Content padding | ✅ | Log-bucketing in `ContentEncoder` |
| Timing obfuscation | ✅ | `TimingObfuscation` delays |
| Cover traffic | ✅ | `CoverTrafficGenerator` (opt-in) |
| Dummy polling | ✅ | `MailboxPoller` decoy polls |
