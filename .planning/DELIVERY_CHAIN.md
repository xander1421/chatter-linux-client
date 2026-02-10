# Message Delivery Architecture

> **All message content is encrypted. No unencrypted fallback.**

---

## Implementation Status: COMPLETE ✅

All delivery paths are **fully implemented** in `MessageGateway` and related services:

| Path | Send | Receive | Location |
|------|------|---------|----------|
| DM (established) | ✅ | ✅ | `chat.rs` → `message_gateway.process_dm_message()` |
| DM (first contact) | ✅ | ✅ | `chat.rs` with session establishment |
| Channel (sender key) | ✅ | ✅ | `message_service.rs` → `message_gateway.route_encrypted_message()` |
| SKDM distribution | ✅ | ✅ | `message_service.distribute_sender_key()` |
| SKDM request/response | ✅ | ✅ | `message_gateway.process_sender_key_request()` |

**To send any content type, use the existing paths - just wrap in `Content` proto.**

---

## Encryption vs Delivery

These are **separate concerns**:

```
ENCRYPTION (what protects the content):
  - Signal Protocol (Double Ratchet) = E2E encryption
  - Sealed Sender = hides sender from server
  - Mailbox Layer = hides recipient from server
  - Sender Key = efficient group encryption

DELIVERY (how it reaches recipient):
  - Ephemeral Mailbox = anonymous delivery point
  - Device Inbox = direct to device
  - Server Routing = server forwards by user ID
```

---

## Delivery Paths by Scenario

### 1. DM - Established Conversation

**Prerequisite**: DM bootstrapped (session + mailbox addresses exchanged)

```
Content proto
    ↓
Sealed Sender encrypt (hides sender)
    ↓
Mailbox layer encrypt (hides content from server)
    ↓
Send to peer's ephemeral mailbox
```

**Privacy**: Server sees nothing - can't link sender or recipient

### 2. DM - First Contact

**Prerequisite**: None (initiating new conversation)

```
Fetch recipient's PreKey bundle from server
    ↓
Establish Signal session (X3DH/PQXDH)
    ↓
Create Content with KeyExchange (our mailbox address)
    ↓
Sealed Sender encrypt
    ↓
Send to recipient's device inbox
    ↓
(Recipient responds with their mailbox address)
    ↓
Future messages use Path 1 (ephemeral mailbox)
```

**Privacy**: Server knows we contacted recipient, but not content

### 3. Channel Message

**Prerequisite**: Channel member with sender key (or will create one)

```
Content proto
    ↓
Sender Key encrypt (efficient group encryption)
    ↓
Wrap in GroupEncrypted proto
    ↓
Send to each member's device inbox
    ↓
(If first message: also send SKDM to all members)
```

**Privacy**: Server sees channel membership, not content

### 4. Channel - New Member

**Prerequisite**: Just joined channel, no sender keys yet

```
Wait for SKDM from existing members
    ↓
(Or request SKDM via SenderKeyRequest)
    ↓
Once have sender keys, use Path 3
```

---

## What Server API Is For

Server API handles **public metadata only**:

| Operation | Server API | Encrypted |
|-----------|------------|-----------|
| Register/Login | ✅ | N/A |
| Fetch PreKey bundle | ✅ | N/A (keys are public) |
| Upload PreKeys | ✅ | N/A (keys are public) |
| Get sender certificate | ✅ | N/A |
| Create/join/leave channel | ✅ | N/A (membership is known) |
| Update profile | ✅ | N/A (public info) |
| **Send message content** | ❌ | Always encrypted |
| **Typing/receipts/reactions** | ❌ | Always encrypted |
| **Edit/delete messages** | ❌ | Always encrypted |

---

## Encryption Prerequisites

```
To send ANY content to a user:
  1. Must have Signal session (establish via X3DH if not)
  2. Must have sender certificate (fetch from server if expired)

To send via ephemeral mailbox (DMs):
  3. Must have peer's mailbox address (bootstrap if not)
  4. Must have shared secret for mailbox layer

To send via sender key (channels):
  3. Must have sender key for channel (create if not)
  4. Must distribute SKDM to new members
```

**If prerequisites not met → establish them first, don't fallback to unencrypted**

---

## DeliveryService Implementation

```rust
// src/services/delivery_service.rs

pub enum DeliveryPath {
    /// DM via ephemeral mailbox (best privacy)
    EphemeralMailbox {
        mailbox_id: String,
        shared_secret: Vec<u8>,
    },
    /// Direct to device inbox (channel or first DM)
    DeviceInbox {
        user_id: UserId,
        device_id: u32,
    },
}

pub struct DeliveryService {
    signal_service: Arc<SignalService>,
    mailbox_manager: Arc<MailboxManager>,
    dm_repo: Arc<DmConversationRepository>,
}

impl DeliveryService {
    /// Send DM content - uses mailbox if available, otherwise bootstraps
    pub async fn send_dm(
        &self,
        recipient_id: &UserId,
        content: Content,
    ) -> Result<(), ServiceError> {
        // Check if DM already bootstrapped
        if let Some(conv) = self.dm_repo.get(recipient_id)? {
            // Use ephemeral mailbox
            self.send_via_mailbox(recipient_id, &conv, content).await
        } else {
            // Need to bootstrap first
            self.bootstrap_and_send(recipient_id, content).await
        }
    }

    /// Send channel content - uses sender key
    pub async fn send_channel(
        &self,
        channel_id: &ChannelId,
        members: &[UserId],
        content: Content,
    ) -> Result<(), ServiceError> {
        let plaintext = content.encode_to_vec();

        // Encrypt with sender key (creates if needed)
        let (ciphertext, skdm) = self.signal_service
            .encrypt_for_group(channel_id, &plaintext)
            .await?;

        // Distribute SKDM if new key
        if let Some(skdm) = skdm {
            self.distribute_skdm(channel_id, members, skdm).await?;
        }

        // Send to each member's device inbox
        for member in members {
            self.send_to_device_inbox(member, &ciphertext).await?;
        }

        Ok(())
    }

    async fn send_via_mailbox(
        &self,
        recipient_id: &UserId,
        conv: &DmConversation,
        content: Content,
    ) -> Result<(), ServiceError> {
        let plaintext = content.encode_to_vec();

        // Layer 1: Sealed sender (hides our identity)
        let sealed = self.signal_service
            .encrypt_message(recipient_id, &plaintext)
            .await?;

        // Layer 2: Mailbox encryption (hides from server)
        let encrypted = encrypt_dm_bytes(&sealed, &conv.shared_secret)?;

        // Send to peer's mailbox
        self.mailbox_manager
            .send_to_mailbox(&conv.peer_mailbox_id, &encrypted)
            .await
    }

    async fn bootstrap_and_send(
        &self,
        recipient_id: &UserId,
        content: Content,
    ) -> Result<(), ServiceError> {
        // 1. Ensure we have a session
        self.signal_service.ensure_session(recipient_id).await?;

        // 2. Create our mailbox for this conversation
        let our_mailbox = self.mailbox_manager.create_mailbox().await?;

        // 3. Send KeyExchange with our mailbox address
        let key_exchange = Content {
            content: Some(content::Content::KeyExchange(KeyExchange {
                mailbox_public_key: our_mailbox.public_key.to_vec(),
                // ... other fields
            })),
            ..Default::default()
        };

        let sealed = self.signal_service
            .encrypt_message(recipient_id, &key_exchange.encode_to_vec())
            .await?;

        // Send via device inbox (no mailbox yet)
        self.send_to_device_inbox(recipient_id, &sealed).await?;

        // 4. Store pending conversation
        self.dm_repo.insert_pending(recipient_id, &our_mailbox)?;

        // 5. Queue the actual content to send after bootstrap completes
        self.queue_pending_message(recipient_id, content)?;

        Ok(())
    }

    async fn send_to_device_inbox(
        &self,
        recipient_id: &UserId,
        ciphertext: &[u8],
    ) -> Result<(), ServiceError> {
        // Get recipient's device(s)
        let devices = self.get_recipient_devices(recipient_id).await?;

        for device in devices {
            self.message_client
                .send_sealed_message(recipient_id, device.id, ciphertext)
                .await?;
        }

        Ok(())
    }
}
```

---

## Error Handling - No Silent Degradation

```rust
impl DeliveryService {
    pub async fn send_dm(&self, recipient_id: &UserId, content: Content) -> Result<(), ServiceError> {
        // Try to establish session if needed
        if !self.signal_service.has_session(recipient_id).await {
            self.signal_service.ensure_session(recipient_id).await
                .map_err(|e| ServiceError::SessionRequired {
                    recipient: recipient_id.clone(),
                    cause: e.to_string(),
                })?;
        }

        // Encrypt and send - if this fails, return error
        // DO NOT fallback to unencrypted
        self.send_encrypted(recipient_id, content).await
    }
}

// Errors that can occur
pub enum DeliveryError {
    /// Cannot establish session - recipient may not exist or be unreachable
    SessionRequired { recipient: UserId, cause: String },

    /// DM bootstrap failed
    BootstrapFailed { recipient: UserId, cause: String },

    /// Sender key not available and couldn't get SKDM
    SenderKeyRequired { channel: ChannelId, cause: String },

    /// Network error during delivery
    NetworkError(NetworkError),

    /// Encryption error
    CryptoError(CryptoError),
}
```

---

## Summary

| Scenario | Encryption | Delivery | Privacy Level |
|----------|------------|----------|---------------|
| DM (established) | Sealed Sender + Mailbox | Ephemeral mailbox | Best |
| DM (first contact) | Sealed Sender | Device inbox | Good |
| Channel | Sender Key | Device inbox | Good |
| Public metadata | None (TLS only) | Server API | N/A |

**Rules:**
1. All message content is encrypted - no exceptions
2. If can't encrypt, fail with error - don't send unencrypted
3. Server API only for public operations (auth, keys, metadata)
4. DMs upgrade to mailbox after bootstrap for best privacy
