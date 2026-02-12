# Auth Callbacks

**Location**: `src/bin_ui/features/auth/`

Login, register, logout, and inbox event handling.

## Files

| File | Purpose |
|------|---------|
| `login.rs` | Login form submission |
| `register.rs` | Registration form submission |
| `logout.rs` | Logout action |
| `initial_data.rs` | Load initial data after login |
| `mod.rs` | Inbox event handling, polling setup |

## login.rs

### on_login_submit

```rust
window.on_login_submit(move |identifier, password| {
    let auth_service = ctx.auth_service();

    tokio::spawn(async move {
        match auth_service.login(&mut auth_client, &identifier, &password, "Linux").await {
            Ok(response) => {
                // Update auth state
                auth_state.set_logged_in(response.user_id, response.device_id);

                // Initialize inbox provider
                ctx.init_device_inbox_provider();

                // Load initial data
                load_initial_data(&ctx).await;

                // Navigate to main view
                window.set_current_view("main");
            }
            Err(e) => {
                window.set_login_error(e.to_string());
            }
        }
    });
});
```

## register.rs

### on_register_submit

```rust
window.on_register_submit(move |username, password, display_name| {
    let auth_service = ctx.auth_service();

    tokio::spawn(async move {
        match auth_service.register(
            &mut auth_client,
            &username,
            &password,
            &display_name,
            "Linux Desktop",
        ).await {
            Ok(response) => {
                // Update auth state
                auth_state.set_logged_in(response.user_id, response.device_id);

                // Initialize inbox
                ctx.init_device_inbox_provider();

                // Navigate
                window.set_current_view("main");
            }
            Err(e) => {
                window.set_register_error(e.to_string());
            }
        }
    });
});
```

## logout.rs

### on_logout

```rust
window.on_logout(move || {
    let auth_service = ctx.auth_service();

    tokio::spawn(async move {
        auth_service.logout().await.ok();

        // Clear state
        auth_state.clear();
        chat_state.clear();

        // Navigate to login
        window.set_current_view("login");
    });
});
```

## mod.rs - Inbox Event Handling

After login, subscribe to inbox events:

```rust
pub fn wire(window: &MainWindow, ctx: &AppContext) {
    // Wire login/register/logout callbacks
    login::wire(window, ctx);
    register::wire(window, ctx);
    logout::wire(window, ctx);
    initial_data::wire(window, ctx);
}

/// Called after successful login
pub fn start_inbox_polling(ctx: &AppContext, window: slint::Weak<MainWindow>) {
    if let Some(provider) = ctx.get_device_inbox_provider() {
        // Start polling loop
        let provider_clone = provider.clone();
        tokio::spawn(async move {
            if let Err(e) = provider_clone.run_loop().await {
                tracing::error!("Inbox polling stopped: {}", e);
            }
        });

        // Subscribe to events
        subscribe_to_inbox_events(provider, window);
    }
}

fn subscribe_to_inbox_events(provider: Arc<DeviceInboxProvider>, window: slint::Weak<MainWindow>) {
    let mut rx = provider.subscribe();

    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if let Some(w) = window.upgrade() {
                slint::invoke_from_event_loop(move || {
                    handle_inbox_event(event, &w);
                }).unwrap();
            }
        }
    });
}
```

## handle_inbox_event

Central event handler for all inbox events:

```rust
fn handle_inbox_event(event: InboxEvent, window: &MainWindow) {
    match event {
        // DM received
        InboxEvent::DmReceived { peer_id, text, timestamp, message_id } => {
            let selected = window.get_selected_dm_id();
            if selected.as_str() == peer_id.to_string() {
                // Add to current chat view
                add_message_to_chat(window, message_id, text, timestamp);
            }
            // Update sidebar with preview
            update_dm_preview(window, peer_id, text);
        }

        // DM unread count
        InboxEvent::DmUnreadCountUpdated { peer_id, unread_count, .. } => {
            update_dm_badge(window, peer_id, unread_count);
        }

        // Channel message
        InboxEvent::MessageProcessed { channel_id, .. } => {
            let selected = window.get_selected_channel_id();
            if selected.as_str() == channel_id.to_string() {
                refresh_channel_messages(window, channel_id);
            }
        }

        // Poll errors
        InboxEvent::PollFailed { error } => {
            tracing::warn!("Inbox poll failed: {}", error);
            // Maybe show connection indicator
        }

        _ => {}
    }
}
```

## initial_data.rs

Loads channels, DMs, friends after login:

```rust
pub async fn load_initial_data(ctx: &AppContext) {
    // Load channels
    let channels = channel_client.get_user_channels().await?;
    for channel in channels {
        channel_repo.upsert(&channel)?;
    }

    // Load DM conversations
    let conversations = dm_conversation_repo.get_all()?;

    // Load friends
    let friends = user_client.get_friends().await?;

    // Update UI
    slint::invoke_from_event_loop(move || {
        window.set_channels(channels_model);
        window.set_dm_conversations(dms_model);
        window.set_friends(friends_model);
    });
}
```

## Flow Diagram

```
Login Form Submit
    │
    ▼
AuthService::login()
    │
    ├─→ Set auth token
    │
    ├─→ ctx.init_device_inbox_provider()
    │
    ├─→ load_initial_data()
    │   └─→ Load channels, DMs, friends
    │
    ├─→ start_inbox_polling()
    │   ├─→ provider.run_loop() (background)
    │   └─→ subscribe_to_inbox_events()
    │
    └─→ Navigate to main view
```

## What NOT to Do

```rust
// ❌ WRONG - Creating polling loop in callback
window.on_login_submit(move || {
    tokio::spawn(async move {
        loop {
            let messages = poll_messages().await;
            // ...
        }
    });
});

// ✅ CORRECT - Use DeviceInboxProvider
ctx.init_device_inbox_provider();
start_inbox_polling(&ctx, window.as_weak());
```

```rust
// ❌ WRONG - Doing crypto in callback
window.on_login_submit(move || {
    // Generate keys manually...
    let identity = IdentityKeyPair::generate();
    let prekeys = generate_prekeys(100);
});

// ✅ CORRECT - Delegate to AuthService
auth_service.login(&mut auth_client, &identifier, &password, device_info).await?;
```
