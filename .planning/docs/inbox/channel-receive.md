# Channel Receive Flow

Sender key decryption for incoming channel messages.

## Overview

Channel messages use Sender Keys:
- One encryption, all members can decrypt
- Messages identified by `associated_data` marker
- SKDM must be received before decrypting messages

## Message Types

| associated_data | Type | Handler |
|-----------------|------|---------|
| `b"sender_key"` | Encrypted content | `process_group_encrypted_message()` |
| `b"sender_key_distribution"` | SKDM | `process_sender_key_distribution()` |
| `b"sender_key_request"` | Key request | `process_sender_key_request()` |

## Flow Diagram

```
Message arrives in device inbox
    │
    ▼
DeviceInboxProvider::poll_device_inbox()
    │
    └─→ InboxPoller::run_once()
        │
        └─→ For each DeviceMessage:
            │
            ▼
        process_message()
            │
            ├─→ Decode as EncryptedData
            │
            └─→ Check associated_data:
                │
                ├─→ "sender_key_distribution"
                │   └─→ process_sender_key_distribution()
                │
                ├─→ "sender_key"
                │   └─→ process_group_encrypted_message()
                │
                └─→ "sender_key_request"
                    └─→ process_sender_key_request()
```

## Detailed Steps

### 1. Poll Device Inbox

```rust
async fn poll_device_inbox(&self) -> Result<(), InboxError> {
    // Get messages from server
    let messages = self.poller.run_once().await?;

    for msg in messages {
        self.process_message(msg).await?;
    }

    self.emit(InboxEvent::PollCompleted {
        count: messages.len() as u32,
        has_more: false,
    });

    Ok(())
}
```

### 2. Route by Type

```rust
async fn process_message(&self, msg: DeviceMessage) -> Result<(), InboxError> {
    // Try to decode as EncryptedData
    let encrypted = match EncryptedData::decode(&msg.envelope) {
        Ok(e) => e,
        Err(_) => {
            // Not encrypted, might be legacy format
            return self.process_unencrypted(msg).await;
        }
    };

    // Route by associated_data marker
    match encrypted.associated_data.as_slice() {
        b"sender_key_distribution" => {
            self.process_sender_key_distribution(msg, encrypted).await
        }
        b"sender_key" => {
            self.process_group_encrypted_message(msg, encrypted).await
        }
        b"sender_key_request" => {
            self.process_sender_key_request(msg, encrypted).await
        }
        _ => {
            // Unknown marker, try sealed sender
            self.process_incoming_message(msg, encrypted).await
        }
    }
}
```

### 3. Process SKDM

```rust
async fn process_sender_key_distribution(
    &self,
    msg: DeviceMessage,
    encrypted: EncryptedData,
) -> Result<(), InboxError> {
    // Decode the GroupEncrypted wrapper
    let group_encrypted = GroupEncrypted::decode(&encrypted.ciphertext)?;

    // Parse channel ID
    let channel_id: ChannelId = group_encrypted.channel_id.parse()?;

    // Build sender address
    let sender = ProtocolAddress::new(
        msg.source_user_id.clone(),
        msg.source_device_id,
    );

    // Decode SKDM from the ciphertext
    let skdm = SenderKeyDistributionMessage::try_from(&group_encrypted.ciphertext)?;

    // Store the sender key
    self.signal_service.process_sender_key_distribution(
        &sender,
        &channel_id,
        skdm,
    ).await?;

    tracing::info!(
        "Stored sender key from {} for channel {}",
        sender, channel_id
    );

    self.emit(InboxEvent::SenderKeyReceived {
        channel_id,
        sender_id: sender.user_id(),
    });

    Ok(())
}
```

### 4. Process Encrypted Message

```rust
async fn process_group_encrypted_message(
    &self,
    msg: DeviceMessage,
    encrypted: EncryptedData,
) -> Result<(), InboxError> {
    // Decode wrapper
    let group_encrypted = GroupEncrypted::decode(&encrypted.ciphertext)?;
    let channel_id: ChannelId = group_encrypted.channel_id.parse()?;

    // Build sender address
    let sender = ProtocolAddress::new(
        msg.source_user_id.clone(),
        msg.source_device_id,
    );

    // Decrypt with sender key
    let plaintext = match self.signal_service.decrypt_from_group(
        &sender,
        &channel_id,
        &group_encrypted.ciphertext,
    ).await {
        Ok(pt) => pt,
        Err(e) => {
            // Missing sender key - request it
            self.request_sender_key(&channel_id, &sender).await?;
            return Err(InboxError::MissingSenderKey);
        }
    };

    // Parse Content proto
    let content = Content::decode(&plaintext)?;

    // Dispatch by content type
    self.dispatch_content(content, &sender, &channel_id, &msg).await
}
```

### 5. Process Sender Key Request

```rust
async fn process_sender_key_request(
    &self,
    msg: DeviceMessage,
    encrypted: EncryptedData,
) -> Result<(), InboxError> {
    let request = SenderKeyRequest::decode(&encrypted.ciphertext)?;
    let channel_id: ChannelId = request.channel_id.parse()?;

    // Requester address
    let requester = ProtocolAddress::new(
        msg.source_user_id.clone(),
        msg.source_device_id,
    );

    // Create fresh SKDM
    let skdm = self.signal_service.create_sender_key(&channel_id)?;

    // Send SKDM to requester
    self.send_skdm_to_member(&channel_id, &requester, skdm).await?;

    tracing::info!(
        "Sent SKDM to {} for channel {}",
        requester, channel_id
    );

    self.emit(InboxEvent::SenderKeyRequested {
        channel_id,
        requester_id: requester.user_id(),
    });

    Ok(())
}
```

### 6. Dispatch Content

```rust
async fn dispatch_content(
    &self,
    content: Content,
    sender: &ProtocolAddress,
    channel_id: &ChannelId,
    msg: &DeviceMessage,
) -> Result<(), InboxError> {
    match content.content {
        Some(content::Content::DataMessage(dm)) => {
            self.handle_channel_data_message(dm, sender, channel_id).await
        }
        Some(content::Content::Typing(typing)) => {
            self.handle_channel_typing(typing, sender, channel_id).await
        }
        Some(content::Content::Reaction(reaction)) => {
            self.handle_channel_reaction(reaction, sender, channel_id).await
        }
        Some(content::Content::Receipt(receipt)) => {
            self.handle_channel_receipt(receipt, sender, channel_id).await
        }
        // ... other types
        _ => {
            tracing::debug!("Unknown content type in channel message");
            Ok(())
        }
    }
}
```

### 7. Handle DataMessage

```rust
async fn handle_channel_data_message(
    &self,
    dm: DataMessage,
    sender: &ProtocolAddress,
    channel_id: &ChannelId,
) -> Result<(), InboxError> {
    // Create message
    let message = Message {
        id: MessageId::new(),
        channel_id: *channel_id,
        sender_id: Some(sender.user_id()),
        content: MessageContent::Text(dm.body.clone()),
        created_at: DateTime::from_timestamp_millis(dm.timestamp),
        // ...
    };

    // Store with deduplication
    let is_new = self.message_repo.insert_or_ignore(&message)?;

    if is_new {
        // Update unread
        self.channel_repo.increment_unread_count(channel_id)?;

        // Emit event
        self.emit(InboxEvent::MessageProcessed {
            message_id: message.id.to_string(),
            channel_id: *channel_id,
            content_type: "data_message".to_string(),
        });
    }

    Ok(())
}
```

## Missing Sender Key Handling

```rust
async fn request_sender_key(
    &self,
    channel_id: &ChannelId,
    sender: &ProtocolAddress,
) -> Result<(), InboxError> {
    // Build request
    let request = SenderKeyRequest {
        channel_id: channel_id.to_string(),
    };

    // Wrap in EncryptedData with marker
    let encrypted = EncryptedData {
        associated_data: b"sender_key_request".to_vec(),
        ciphertext: request.encode_to_vec(),
        // ...
    };

    // Send to the member who sent the message we couldn't decrypt
    self.message_client.send_to_device(
        &sender.user_id(),
        sender.device_id(),
        &encrypted.encode_to_vec(),
    ).await?;

    tracing::info!(
        "Requested sender key from {} for channel {}",
        sender, channel_id
    );

    Ok(())
}
```

## Related Files

- `src/inbox/provider.rs` - poll_device_inbox(), process_message()
- `src/services/message_gateway.rs` - route_encrypted_message()
- `src/services/signal_service.rs` - decrypt_from_group()
- `src/crypto/signal/sender_key.rs` - SenderKeyManager
