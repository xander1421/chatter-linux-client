# Inbox Polling

**File**: `src/inbox/provider.rs`

The single message polling loop for the entire application.

## DeviceInboxProvider

**THE** polling loop. There is only one, and it handles:
- Device inbox (channel messages, sender key distributions)
- DM mailboxes (via TwoWayAnonymity)

```rust
pub struct DeviceInboxProvider {
    config: InboxConfig,
    message_client: Arc<Mutex<MessageClient>>,
    mailbox_client: Arc<Mutex<MailboxClient>>,
    validation_pipeline: Arc<MessageValidationPipeline>,
    message_gateway: Arc<MessageGateway>,
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    event_bus: Option<EventBus>,
    tx: broadcast::Sender<InboxEvent>,
}
```

## Initialization

Provider is lazily initialized after login:

```rust
// In login callback, after successful auth:
ctx.init_device_inbox_provider();

// Later, get the provider:
let provider = ctx.get_device_inbox_provider()?;
```

## The Run Loop

```rust
pub async fn run_loop(&self) -> Result<(), InboxError> {
    loop {
        // 1. Poll device inbox
        self.poll_device_inbox().await?;

        // 2. Poll DM mailboxes
        self.poll_dm_mailboxes().await?;

        // 3. Wait before next iteration
        tokio::time::sleep(self.config.poll_interval).await;
    }
}
```

### poll_device_inbox

```
poll_device_inbox()
    │
    ├─→ InboxPoller::run_once()
    │   (gRPC: GetMessages)
    │
    └─→ For each DeviceMessage:
        │
        ├─→ Validate envelope
        │
        ├─→ Try decode as EncryptedData
        │
        └─→ Route by associated_data marker:
            │
            ├─→ "sender_key_distribution"
            │   → process_sender_key_distribution()
            │
            ├─→ "sender_key"
            │   → process_group_encrypted_message()
            │   → Parse Content, dispatch by type
            │
            ├─→ "sender_key_request"
            │   → process_sender_key_request()
            │
            └─→ Other (sealed sender)
                → process_incoming_message()
```

### poll_dm_mailboxes

```
poll_dm_mailboxes()
    │
    ├─→ Get active DM conversations
    │
    └─→ For each conversation:
        │
        ├─→ Poll peer's send mailbox
        │
        └─→ For each message:
            │
            ├─→ Get shared secret
            │
            └─→ MessageGateway::process_dm_message()
                │
                ├─→ Layer 1: decrypt mailbox
                │   (XChaCha20-Poly1305)
                │
                ├─→ Layer 2: decrypt Signal
                │   (Sealed sender + Double Ratchet)
                │
                ├─→ Parse Content proto
                │
                ├─→ Store message
                │
                └─→ Emit InboxEvent::DmReceived
```

## Do NOT Create Another Loop

```rust
// ❌ WRONG - Creating separate polling
tokio::spawn(async move {
    loop {
        let messages = some_client.get_messages().await?;
        for msg in messages {
            // Process...
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
});

// ✅ CORRECT - Subscribe to events
let mut rx = provider.subscribe();
while let Ok(event) = rx.recv().await {
    handle_event(event);
}
```

## Event Subscription

```rust
// Get subscription
let mut rx = provider.subscribe();

// Handle events (in auth.rs)
tokio::spawn(async move {
    while let Ok(event) = rx.recv().await {
        if let Some(window) = window_weak.upgrade() {
            slint::invoke_from_event_loop(move || {
                handle_inbox_event(event, &window);
            }).unwrap();
        }
    }
});
```

## Configuration

```rust
pub struct InboxConfig {
    /// Polling interval
    pub poll_interval: Duration,  // Default: 5s

    /// Max messages per poll
    pub batch_size: u32,  // Default: 100

    /// Retry attempts on failure
    pub max_retries: u32,  // Default: 3
}
```

## Lifecycle

```
App Start
    │
    ▼
AppContext::new()
    │
    ├─→ device_inbox_provider = OnceLock::new()
    │   (not yet initialized)
    │
    ▼
User logs in
    │
    ├─→ ctx.init_device_inbox_provider()
    │   (creates provider with clients)
    │
    ├─→ Start run_loop in background task
    │
    └─→ Subscribe UI to events
```

## Related Files

- `src/inbox/poller.rs` - Low-level gRPC polling
- `src/inbox/events.rs` - InboxEvent types
- `src/services/message_gateway.rs` - Message routing/decryption
- `src/bin_ui/features/auth.rs` - Event handling
