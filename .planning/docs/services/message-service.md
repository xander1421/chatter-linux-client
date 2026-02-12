# MessageService

**File**: `src/services/message_service.rs`

Orchestrates all message sending - both channels and DMs.

## Purpose

MessageService is the **single entry point** for sending messages. It handles:
- Channel messages (with optional sender key encryption)
- DM messages (with two-layer encryption)
- Saved messages (Note to Self)
- Proto building, encryption, and delivery

## Methods

### Channel Message

```rust
/// Send message to a channel
pub async fn send_message(
    &self,
    message_client: &mut MessageClient,
    auth_client: &mut AuthClient,
    channel_client: &mut ChannelClient,
    channel_id: &ChannelId,
    text: &str,
    attachment_ids: Vec<AttachmentId>,
    reply_to: Option<MessageId>,
    mentioned_user_ids: Vec<UserId>,
    targets_ai_bot: bool,
) -> Result<Message, ServiceError>
```

What it does:
1. Checks `channel.config.require_encryption`
2. If encrypted: calls `SignalService::encrypt_for_group()`
3. Distributes SKDM to new members if needed
4. Sends via gRPC
5. Returns stored message

### DM Message

```rust
/// Send text message to DM peer (two-layer encryption)
pub async fn send_dm_text(
    &self,
    auth_client: &mut AuthClient,
    peer_id: &UserId,
    text: &str,
    database: Arc<Database>,
    mailbox_manager: &MailboxManager,
) -> Result<(), ServiceError>
```

What it does:
1. Builds DataMessage proto
2. Layer 1: `SignalService::encrypt_message()` (sealed sender)
3. Layer 2: `DmCrypto::encrypt_dm_bytes()` (mailbox encryption)
4. Sends to peer's anonymous mailbox

### DM Content (Generic)

```rust
/// Send any Content to DM peer
pub async fn send_dm_content(
    &self,
    auth_client: &mut AuthClient,
    peer_id: &UserId,
    content: Content,
    database: Arc<Database>,
    mailbox_manager: &MailboxManager,
) -> Result<(), ServiceError>
```

Used for: typing indicators, reactions, read receipts, etc.

### Saved Messages

```rust
/// Send message to Saved Messages (Note to Self)
pub async fn send_saved_message(
    &self,
    auth_client: &mut AuthClient,
    message_client: &mut MessageClient,
    text: &str,
    attachment_ids: Vec<AttachmentId>,
) -> Result<Message, ServiceError>
```

## Content Builders

MessageService re-exports content builders for convenience:

```rust
pub use message_gateway::{
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

## Usage Examples

### Send Channel Message (from UI)

```rust
// In send_message.rs
let message_service = ctx.message_service();

match message_service.send_message(
    &mut message_client,
    &mut auth_client,
    &mut channel_client,
    &channel_id,
    &text,
    pending_attachment_ids,
    None,  // reply_to
    vec![], // mentions
    false,  // targets_ai_bot
).await {
    Ok(message) => { /* update UI */ }
    Err(e) => tracing::error!("Send failed: {}", e),
}
```

### Send DM (from UI)

```rust
// In send_message.rs
match message_service.send_dm_text(
    &mut auth_client,
    &peer_id,
    &text,
    database,
    &mailbox_manager,
).await {
    Ok(()) => { /* update UI */ }
    Err(e) => tracing::error!("DM send failed: {}", e),
}
```

### Send Typing Indicator (from UI)

```rust
// In typing.rs - using helper
let content = build_typing_content(is_typing);
send_dm_content(content, &peer_id, ...).await?;
```

## Flow Diagrams

### Channel Send Flow

```
UI: on_send()
    │
    ▼
MessageService::send_message()
    │
    ├─→ Check require_encryption
    │
    ├─→ If encrypted:
    │       SignalService::encrypt_for_group()
    │       │
    │       └─→ Distribute SKDM if new
    │
    └─→ MessageClient::send_message()
```

### DM Send Flow

```
UI: on_send()
    │
    ▼
MessageService::send_dm_text()
    │
    ├─→ Build DataMessage proto
    │
    ├─→ Layer 1: SignalService::encrypt_message()
    │   (Sealed sender - forward secrecy)
    │
    ├─→ Layer 2: DmCrypto::encrypt_dm_bytes()
    │   (Mailbox shared secret - sender anonymity)
    │
    └─→ MailboxManager::send_to_mailbox()
```

## What NOT to Do

```rust
// ❌ WRONG - Building proto in UI
let data_message = DataMessage {
    body: text.to_string(),
    timestamp: now_millis(),
    ..Default::default()
};
let plaintext = data_message.encode_to_vec();

// ✅ CORRECT - Use MessageService
message_service.send_dm_text(&mut auth_client, &peer_id, &text, db, &mailbox).await?;
```

## Dependencies

- `SignalService` - Encryption operations
- `MessageRepository` - Message storage
- `ChannelRepository` - Channel config lookup
- `SettingsRepository` - User settings
