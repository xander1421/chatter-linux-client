# Chat Callbacks

**Location**: `src/bin_ui/features/chat/`

UI callbacks for message sending, typing, reactions, and other chat actions.

## Files

| File | Purpose |
|------|---------|
| `send_message.rs` | Main send callback for DMs and channels |
| `typing.rs` | Typing indicator callbacks |
| `reactions.rs` | Reaction add/remove callbacks |
| `pin.rs` | Pin/unpin message callbacks |
| `edit.rs` | Message edit callbacks |
| `delete.rs` | Message delete callbacks |
| `helpers.rs` | Shared helper functions |

## send_message.rs

### on_send

Main callback for sending messages.

```rust
window.on_send(move |text: SharedString| {
    // Determines DM vs channel based on selected_dm_id
    // Delegates to MessageService for both
});
```

**Flow**:
```
on_send(text)
    │
    ├─→ Check: is DM or channel?
    │   (based on selected_dm_id)
    │
    ├─→ If DM:
    │   └─→ MessageService::send_dm_text()
    │
    └─→ If Channel:
        ├─→ Check SavedMessages type
        │   └─→ MessageService::send_saved_message()
        │
        └─→ Regular channel
            └─→ MessageService::send_message()
```

**Correct Pattern**:
```rust
// Delegates entirely to MessageService
let message_service = ctx.message_service();

// DM
message_service.send_dm_text(&mut auth_client, &peer_id, &text, database, &mailbox_manager).await?;

// Channel
message_service.send_message(&mut message_client, &mut auth_client, &mut channel_client, ...).await?;
```

## typing.rs

### on_typing_changed

Sends typing indicators to DM or channel.

```rust
window.on_typing_changed(move |is_typing: bool| {
    // Uses build_typing_content() helper
    // Delegates to send_dm_content() or send_channel_content()
});
```

**Correct Pattern**:
```rust
// Build content using helper (NOT building proto directly)
let content = build_typing_content(is_typing);

// Send via helper
send_dm_content(content, &peer_id, database, grpc_pool, message_service, mailbox_manager).await?;
```

## reactions.rs

### on_reaction_selected

Adds reaction to message.

```rust
window.on_reaction_selected(move |message_id: SharedString, emoji: SharedString| {
    // Looks up message for author/timestamp
    // Uses build_reaction_content() helper
    // Delegates to send_dm_content() or send_channel_content()
});
```

**Correct Pattern**:
```rust
// Look up message for reaction target info
let message = message_repo.get(&msg_id)?;
let target_author_aci = message.sender_id.map(|id| id.to_string()).unwrap_or_default();
let target_sent_timestamp = message.created_at.timestamp_millis() as u64;

// Build content using helper
let content = build_reaction_content(&emoji_str, false, &target_author_aci, target_sent_timestamp);

// Send via helper
send_dm_content(content, &peer_id, ...).await?;
```

## helpers.rs

Shared helper functions that properly delegate to services.

### send_dm_content

```rust
pub async fn send_dm_content(
    content: Content,
    peer_id: &UserId,
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    message_service: Arc<MessageService>,
    mailbox_manager: Arc<MailboxManager>,
) -> Result<(), ServiceError>
```

What it does:
1. Creates AuthClient
2. Calls MessageService::send_dm_content()
3. Handles two-layer encryption internally

### send_channel_content

```rust
pub async fn send_channel_content(
    content: Content,
    channel_id: &ChannelId,
    message_service: Arc<MessageService>,
    grpc_pool: Arc<GrpcClientPool>,
) -> Result<(), ServiceError>
```

What it does:
1. Creates required clients
2. Calls MessageService for sender key encryption
3. Sends to channel

## What NOT to Do

```rust
// ❌ WRONG - Building proto in UI
let typing_message = TypingMessage {
    timestamp: now_millis(),
    action: if is_typing { 0 } else { 1 },
};
let content = Content {
    typing: Some(typing_message),
    ..Default::default()
};
let plaintext = content.encode_to_vec();
// ... then manually encrypting ...

// ✅ CORRECT - Use helper + delegate
let content = build_typing_content(is_typing);
send_dm_content(content, &peer_id, ...).await?;
```

```rust
// ❌ WRONG - Direct SignalService calls in UI
let ciphertext = signal_service.encrypt_message(&mut auth_client, ...).await?;
let mailbox_encrypted = encrypt_dm_bytes(&ciphertext, &shared_secret)?;

// ✅ CORRECT - Delegate to MessageService
message_service.send_dm_text(&mut auth_client, &peer_id, &text, db, &mailbox).await?;
```

## Content Builders

Always use these helpers (from `services/message_gateway.rs`):

| Builder | Purpose |
|---------|---------|
| `build_text_content()` | DataMessage with text |
| `build_typing_content()` | TypingMessage |
| `build_reaction_content()` | Reaction |
| `build_read_receipt_content()` | Read receipt |
| `build_pin_content()` | Pin/unpin message |
| `build_edit_content()` | Edit message |
| `build_delete_for_me_content()` | Delete for self |
| `build_delete_for_everyone_content()` | Delete for all |

## Thread Safety

All callbacks use proper async patterns:

```rust
window.on_some_action(move || {
    let ctx = ctx.clone();

    // Use spawn_blocking for libsignal (not Send)
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create runtime");

        rt.block_on(async {
            // Async work here
        });
    });
});
```

Use `slint::invoke_from_event_loop()` for UI updates from async context.
