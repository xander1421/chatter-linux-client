# DM Receive Flow

Two-layer decryption for incoming DM messages.

## Overview

DM messages have two encryption layers:
1. **Outer (Mailbox)**: XChaCha20-Poly1305 with conversation shared secret
2. **Inner (Signal)**: Sealed Sender with Double Ratchet

## Flow Diagram

```
DM arrives in mailbox
    │
    ▼
DeviceInboxProvider::poll_dm_mailboxes()
    │
    ├─→ Get active DM conversations
    │
    └─→ For each conversation:
        │
        ├─→ Poll peer's send mailbox
        │
        └─→ For each message:
            │
            ▼
        MessageGateway::process_dm_message()
            │
            ├─→ Layer 1: Decrypt mailbox
            │   (XChaCha20-Poly1305)
            │
            ├─→ Layer 2: Decrypt Signal
            │   (Sealed sender validation)
            │
            ├─→ Parse Content proto
            │
            ├─→ Dispatch by content type
            │
            └─→ Store + emit event
```

## Detailed Steps

### 1. Poll DM Mailboxes

```rust
// In DeviceInboxProvider
async fn poll_dm_mailboxes(&self) -> Result<(), InboxError> {
    // Get all active DM conversations
    let conversations = dm_conversation_repo.get_active()?;

    for conversation in conversations {
        // Get the mailbox we poll (peer's send mailbox)
        let mailbox_id = conversation.peer_send_mailbox_id;

        // Poll for messages
        let messages = mailbox_client.poll_mailbox(&mailbox_id).await?;

        for msg in messages {
            // Get shared secret for this conversation
            let shared_secret = conversation.shared_secret();

            // Process through gateway
            self.message_gateway.process_dm_message(
                &msg.ciphertext,
                &shared_secret,
                &conversation,
            ).await?;
        }
    }
}
```

### 2. Decrypt Mailbox Layer

```rust
// In MessageGateway::process_dm_message()

// Layer 1: Remove mailbox encryption
let signal_ciphertext = decrypt_dm_bytes(&mailbox_ciphertext, &shared_secret)?;

// decrypt_dm_bytes() does:
// 1. Extract nonce (first 24 bytes)
// 2. XChaCha20-Poly1305 decrypt with shared secret
// 3. Verify authentication tag
// 4. Return inner ciphertext
```

### 3. Decrypt Signal Layer

```rust
// Layer 2: Signal sealed sender decryption
let (sender_address, plaintext) = self.signal_service
    .decrypt_message(&signal_ciphertext)
    .await?;

// decrypt_message() does:
// 1. Unwrap sealed sender envelope
// 2. Extract sender certificate
// 3. Validate certificate against TRUST_ROOT
// 4. Check certificate expiration
// 5. Double Ratchet decrypt
// 6. Return sender identity + plaintext
```

### 4. Parse Content

```rust
// Parse the proto
let content = Content::decode(&plaintext)?;

// Dispatch by type
match content.content {
    Some(content::Content::DataMessage(dm)) => {
        handle_data_message(dm, &sender_address, &conversation).await?;
    }
    Some(content::Content::Typing(typing)) => {
        handle_typing_indicator(typing, &sender_address).await?;
    }
    Some(content::Content::Receipt(receipt)) => {
        handle_receipt(receipt, &sender_address).await?;
    }
    Some(content::Content::Reaction(reaction)) => {
        handle_reaction(reaction, &sender_address).await?;
    }
    // ... other types
}
```

### 5. Handle DataMessage

```rust
async fn handle_data_message(
    dm: DataMessage,
    sender: &ProtocolAddress,
    conversation: &DmConversation,
) -> Result<(), InboxError> {
    // Derive DM channel ID (deterministic from participants)
    let channel_id = ChannelId::from_dm_participants(
        &our_user_id,
        &sender.user_id(),
    );

    // Create message domain object
    let message = Message {
        id: MessageId::new(),
        channel_id,
        sender_id: Some(sender.user_id()),
        content: MessageContent::Text(dm.body),
        created_at: DateTime::from_timestamp_millis(dm.timestamp),
        // ...
    };

    // Store with deduplication
    let is_new = message_repo.insert_or_ignore(&message)?;

    if is_new {
        // Update unread count
        channel_repo.increment_unread_count(&channel_id)?;

        // Emit event
        self.emit(InboxEvent::DmReceived {
            peer_id: sender.user_id(),
            text: dm.body,
            timestamp: message.created_at,
            message_id: message.id,
        });

        self.emit(InboxEvent::DmUnreadCountUpdated {
            channel_id,
            peer_id: sender.user_id(),
            unread_count: channel_repo.get_unread_count(&channel_id)?,
        });
    }

    Ok(())
}
```

## Error Handling

```rust
match self.message_gateway.process_dm_message(...).await {
    Ok(()) => {
        tracing::debug!("DM processed successfully");
    }
    Err(InboxError::Crypto(e)) => {
        // Decryption failed
        self.emit(InboxEvent::DmDecryptionFailed {
            peer_id,
            error: e.to_string(),
        });
        tracing::warn!("DM decryption failed: {}", e);
    }
    Err(e) => {
        tracing::error!("DM processing error: {}", e);
    }
}
```

## Deduplication

```rust
// Always use insert_or_ignore to handle duplicates
match message_repo.insert_or_ignore(&message)? {
    true => {
        // New message - process normally
    }
    false => {
        // Duplicate - already have it, skip
        tracing::trace!("Duplicate DM ignored: {}", message.id);
    }
}
```

## Security Validations

| Check | Where | What Happens on Failure |
|-------|-------|------------------------|
| Mailbox auth tag | decrypt_dm_bytes | CryptoError::DecryptionFailed |
| Certificate signature | decrypt_message | CryptoError::InvalidSenderCertificate |
| Certificate expiration | decrypt_message | CryptoError::CertificateExpired |
| Session exists | decrypt_message | Automatic session creation |

## Related Files

- `src/inbox/provider.rs` - poll_dm_mailboxes()
- `src/services/message_gateway.rs` - process_dm_message()
- `src/crypto/dm_crypto.rs` - decrypt_dm_bytes()
- `src/services/signal_service.rs` - decrypt_message()
