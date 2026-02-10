# Sealed Sender Feature Implementation

> **All content goes through sealed sender + mailbox. Server has no message storage.**

---

## Infrastructure Status: COMPLETE ✅

**All encryption/decryption infrastructure is built and working in `MessageGateway`:**

```
SEND (already working for text messages):
  chat.rs (DM)         → signal_service.encrypt_message() → encrypt_dm_bytes() → mailbox
  message_service.rs   → signal_service.encrypt_for_group() → device inbox

RECEIVE (already working):
  provider.rs → MessageGateway::process_dm_message()           → decrypts DM
  provider.rs → MessageGateway::route_encrypted_message()      → decrypts channel
  provider.rs → MessageGateway::process_sender_key_distribution() → stores SKDM
```

**To add new content types (typing, reactions, etc.):**
1. Use the SAME send path as text messages
2. Just change the `Content` proto variant
3. Receive side already decrypts - just parse Content and dispatch by type

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ Client A                                                        │
│                                                                 │
│  Content proto (text/typing/receipt/reaction/edit/delete/pin)   │
│      ↓                                                          │
│  Add padding (traffic analysis protection)                      │
│      ↓                                                          │
│  Sealed Sender encrypt (Signal Protocol)                        │
│      ↓                                                          │
│  Mailbox encrypt (XChaCha20-Poly1305)                          │
│      ↓                                                          │
│  Send to recipient's anonymous mailbox                          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Server (sees NOTHING - just encrypted blobs)                    │
│                                                                 │
│  - Routes to mailbox                                            │
│  - No message storage                                           │
│  - No content inspection                                        │
│  - No metadata about content type                               │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Client B                                                        │
│                                                                 │
│  Poll mailbox                                                   │
│      ↓                                                          │
│  Mailbox decrypt                                                │
│      ↓                                                          │
│  Sealed Sender decrypt (validates certificate)                  │
│      ↓                                                          │
│  Parse Content proto → route to handler by type                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## Feature Checklist: Register → Delete

### 1. Registration & Auth (Server API - OK)
- [x] Register account
- [x] Login
- [x] Logout
- [x] Upload PreKey bundle
- [x] Device management

### 2. Session Establishment (Local + Server API)
- [x] Fetch PreKey bundle from server
- [x] X3DH/PQXDH key agreement
- [x] Session storage in SQLCipher

### 3. DM Bootstrap (Sealed Sender + Mailbox)
- [x] Key exchange via mailbox
- [x] Shared secret derivation
- [x] Conversation storage

### 4. Send/Receive Messages (Sealed Sender + Mailbox)

| Content Type | Send | Receive | Notes |
|--------------|------|---------|-------|
| `DataMessage` (text) | ✅ | ✅ | Working |
| `DataMessage` (with attachments) | ⚠️ | ⚠️ | Attachment pointers, need verification |
| `DataMessage` (with quote) | ❌ | ❌ | Quote field not used |
| `DataMessage.delete` (delete for everyone) | ❌ | ❌ | Not implemented |
| `TypingMessage` | ❌ | ❌ | Currently uses broken server API |
| `ReceiptMessage` (read) | ❌ | ❌ | Currently uses broken server API |
| `ReceiptMessage` (delivery) | ❌ | ❌ | Not implemented |
| `Reaction` | ❌ | ❌ | Currently uses broken server API |
| `EditMessage` | ❌ | ❌ | Currently uses broken server API |
| `PinMessage` | ❌ | ❌ | Currently uses broken server API |
| `DeleteForMeSync` | ❌ | ⚠️ | Receive handler exists, send missing |
| `KeyExchange` | ✅ | ✅ | DM bootstrap |
| `AttachmentMessage` | ❌ | ❌ | Separate from DataMessage attachments |

### 5. Channel Messages (Sender Key + Device Inbox)

| Content Type | Send | Receive | Notes |
|--------------|------|---------|-------|
| `GroupEncrypted` (text) | ✅ | ✅ | Working |
| `SenderKeyDistribution` | ✅ | ✅ | Working |
| `SenderKeyRequest` | ⚠️ | ⚠️ | Partial - needs full flow |
| `GroupMailboxAddress` | ❌ | ❌ | Not implemented |
| All other content types | ❌ | ❌ | Same as DM - need sender key path |

### 6. Attachments
- [x] Encrypt file with AES-GCM
- [x] Upload encrypted blob to CDN
- [x] Download and decrypt
- [ ] Send `AttachmentPointer` in `DataMessage` via sealed sender
- [ ] Verify full flow works

---

## Implementation Tasks

### Phase 1: Core Content Envelope

**Goal**: All content wrapped in `Content` proto with padding

```rust
// src/services/content_service.rs (NEW)

pub struct ContentService {
    signal_service: Arc<SignalService>,
}

impl ContentService {
    /// Wrap any content in padded Content envelope
    pub fn wrap_content(&self, inner: content::Content) -> Content {
        let timestamp = chrono::Utc::now().timestamp_millis() as u64;
        let mut content = Content {
            timestamp,
            content: Some(inner),
            ephemeral_reply_to: None,
            padding: vec![],
        };

        // Add padding to bucket size
        let size = content.encoded_len();
        let bucket = self.padding_bucket(size);
        let padding_needed = bucket - size;
        content.padding = self.random_padding(padding_needed);

        content
    }

    /// Encrypt Content for DM via sealed sender + mailbox
    pub async fn encrypt_for_dm(
        &self,
        content: Content,
        recipient_id: &UserId,
        shared_secret: &[u8],
    ) -> Result<Vec<u8>, ServiceError> {
        let plaintext = content.encode_to_vec();

        // Layer 1: Sealed sender
        let sealed = self.signal_service
            .encrypt_message(&mut auth_client, recipient_id, device_id, &plaintext)
            .await?;

        // Layer 2: Mailbox
        let mailbox_encrypted = encrypt_dm_bytes(&sealed, shared_secret)?;

        Ok(mailbox_encrypted)
    }

    /// Encrypt Content for channel via sender key
    pub async fn encrypt_for_channel(
        &self,
        content: Content,
        channel_id: &ChannelId,
    ) -> Result<(Vec<u8>, Option<SenderKeyDistributionMessage>), ServiceError> {
        let plaintext = content.encode_to_vec();
        self.signal_service.encrypt_for_group(channel_id, &plaintext).await
    }
}
```

### Phase 2: DM Content Senders

| Function | Content Type | Priority |
|----------|--------------|----------|
| `send_dm_text()` | `DataMessage` | ✅ Done (refactor to use ContentService) |
| `send_dm_typing()` | `TypingMessage` | P0 |
| `send_dm_read_receipt()` | `ReceiptMessage` | P0 |
| `send_dm_delivery_receipt()` | `ReceiptMessage` | P1 |
| `send_dm_reaction()` | `Reaction` | P1 |
| `send_dm_edit()` | `EditMessage` | P1 |
| `send_dm_delete()` | `DataMessage.delete` | P1 |
| `send_dm_pin()` | `PinMessage` | P2 |
| `send_delete_for_me_sync()` | `DeleteForMeSync` | P2 |

### Phase 3: Content Handlers (Receive Side)

Most handlers exist but need verification/completion:

| Handler | Content Type | Status |
|---------|--------------|--------|
| `chat_handler.rs` | `DataMessage` | ✅ Working |
| `typing_handler.rs` | `TypingMessage` | ❌ Uses server stream |
| `receipt_handler.rs` | `ReceiptMessage` | ❌ Uses server stream |
| `reaction_handler.rs` | `Reaction` | ❌ Uses server stream |
| `edit_handler.rs` | `EditMessage` | ❌ Needs implementation |
| `pin_handler.rs` | `PinMessage` | ❌ Needs implementation |
| `sync_handler.rs` | `DeleteForMeSync` | ⚠️ Partial |

### Phase 4: Channel Content (Same as DM but sender key)

Same content types, but:
- Use `encrypt_for_channel()` instead of `encrypt_for_dm()`
- Send via device inbox instead of mailbox
- Wrap in `GroupEncrypted` or send SKDM as needed

---

## Current Broken Features

These features currently call server API but **cannot work** because server doesn't store messages:

1. **Typing indicators** - `typing_service.rs` calls `message_client.send_typing()`
2. **Read receipts** - calls `message_client.send_read_receipt()`
3. **Reactions** - `reaction_service.rs` calls server API
4. **Edit message** - `message_client.edit_message()`
5. **Delete message** - `message_client.delete_message()`
6. **Pin message** - server API calls

These need to be reimplemented to send `Content` proto via sealed sender.

---

## Local Storage for Features

Since server doesn't store messages, client needs local storage for:

| Feature | Local Storage | Notes |
|---------|---------------|-------|
| Messages | `messages` table | ✅ Working |
| Reactions | `reactions` table | ✅ Exists |
| Read status | `messages.read_at` | ❌ Need column |
| Delivery status | `messages.delivered_at` | ❌ Need column |
| Pins | `messages.is_pinned` | ✅ Exists |
| Edits | `messages.content`, `edited_at` | ✅ Exists |
| Typing | In-memory only | Ephemeral |

---

## Summary

**Working via Sealed Sender:**
- DM text send/receive
- Channel text send/receive (sender key)
- Sender key distribution
- DM bootstrap

**Broken (using non-functional server API):**
- Typing indicators
- Read receipts
- Reactions
- Edit message
- Delete message
- Pin message

**Not Implemented:**
- Delivery receipts
- Delete for me sync (send)
- Quotes in messages
- Full attachment flow verification

**Next Steps:**
1. Create `ContentService` for unified content wrapping
2. Implement `send_dm_typing()` as template
3. Implement remaining content senders
4. Fix/create content handlers
5. Add missing local storage columns
