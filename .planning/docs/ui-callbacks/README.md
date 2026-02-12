# UI Callbacks

Slint callback wiring in `src/bin_ui/features/`.

## Structure

```
features/
├── mod.rs              # wire_all() entry point
├── auth/               # Login, register, logout
│   ├── login.rs
│   ├── register.rs
│   ├── logout.rs
│   └── initial_data.rs
├── chat/               # Message sending and actions
│   ├── send_message.rs
│   ├── typing.rs
│   ├── reactions.rs
│   ├── pin.rs
│   ├── edit.rs
│   ├── delete.rs
│   └── helpers.rs
├── composer.rs         # Attachments, emoji, mentions
├── sidebar.rs          # Channel/DM list
├── header.rs           # Header actions
├── navigation.rs       # View switching
├── thread.rs           # Thread view
└── modals/             # Modal dialogs
    ├── settings.rs
    ├── create_channel.rs
    ├── join_channel.rs
    ├── friends.rs
    ├── friend_requests.rs
    ├── profile.rs
    └── sessions.rs
```

## Documentation

- [Auth Callbacks](auth.md) - Login, register, logout, inbox events
- [Chat Callbacks](chat.md) - Message sending, typing, reactions
- [Composer Callbacks](composer.md) - Attachments, emoji, mentions
- [Modal Callbacks](modals.md) - Channels, settings, friends

## Core Principle

**UI callbacks delegate to services. Never do business logic in callbacks.**

```rust
// ✅ CORRECT - Delegate to service
window.on_send(move |text| {
    let message_service = ctx.message_service();
    tokio::spawn(async move {
        message_service.send_message(...).await?;
    });
});

// ❌ WRONG - Business logic in callback
window.on_send(move |text| {
    let data_message = DataMessage { body: text.to_string(), ... };
    let encrypted = signal_service.encrypt_for_group(...).await?;
    // 100 more lines of logic...
});
```

## Wiring Order

All callbacks wired in `features/mod.rs`:

```rust
pub fn wire_all(window: &MainWindow, ctx: &AppContext) {
    // Auth first (handles login state)
    auth::wire(window, ctx);

    // Navigation
    navigation::wire(window);

    // Core functionality
    chat::wire(window, ctx);
    sidebar::wire(window, ctx);
    composer::wire(window, ctx);
    header::wire(window, ctx);
    thread::wire(window, ctx);

    // Modals
    modals::wire(window, ctx);
}
```

## Async Patterns

### spawn_blocking (for libsignal)

```rust
window.on_some_action(move || {
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create runtime");

        rt.block_on(async {
            // Async work with libsignal stores
        });
    });
});
```

### invoke_from_event_loop (for UI updates)

```rust
// From async context
slint::invoke_from_event_loop(move || {
    if let Some(w) = weak.upgrade() {
        w.set_some_property(value);
    }
}).unwrap();
```

### slint::spawn_local (for simple async)

```rust
window.on_some_action(move || {
    slint::spawn_local(async move {
        let result = some_async_operation().await;
        // UI updates happen here (already on UI thread)
    }).unwrap();
});
```

## Common Patterns

### Get Service, Clone State, Spawn

```rust
pub fn wire_some_feature(window: &MainWindow, ctx: &AppContext) {
    let message_service = ctx.message_service();
    let chat_state = ctx.chat_state.clone();
    let weak_window = window.as_weak();

    window.on_some_action(move |arg| {
        let message_service = message_service.clone();
        let chat_state = chat_state.clone();
        let weak = weak_window.clone();

        tokio::spawn(async move {
            // Use cloned values
            match message_service.do_something(&arg).await {
                Ok(result) => {
                    chat_state.update(|s| s.some_field = result);

                    slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_result(result.into());
                        }
                    }).unwrap();
                }
                Err(e) => tracing::error!("Failed: {}", e),
            }
        });
    });
}
```

### Helper Function Pattern

```rust
// In helpers.rs
pub async fn send_dm_content(
    content: Content,
    peer_id: &UserId,
    database: Arc<Database>,
    grpc_pool: Arc<GrpcClientPool>,
    message_service: Arc<MessageService>,
    mailbox_manager: Arc<MailboxManager>,
) -> Result<(), ServiceError> {
    let mut auth_client = AuthClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
    message_service.send_dm_content(&mut auth_client, peer_id, content, database, &mailbox_manager).await
}

// Usage
let content = build_typing_content(is_typing);
send_dm_content(content, &peer_id, database, grpc_pool, message_service, mailbox_manager).await?;
```
