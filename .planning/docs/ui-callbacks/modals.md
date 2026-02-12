# Modal Callbacks

**Location**: `src/bin_ui/features/modals/`

Modal dialog callbacks for channels, settings, friends, etc.

## Files

| File | Purpose |
|------|---------|
| `settings.rs` | User settings and preferences |
| `create_channel.rs` | Create new channel |
| `join_channel.rs` | Join existing channel |
| `friends.rs` | Friends list management |
| `friend_requests.rs` | Incoming/outgoing friend requests |
| `profile.rs` | User profile view/edit |
| `sessions.rs` | Device sessions management |

## settings.rs

### on_settings_change_password

```rust
window.on_settings_change_password(move |current, new| {
    let account_service = AccountService::new(settings_repo.clone(), auth_interceptor);

    tokio::spawn(async move {
        match account_service.change_password(&mut auth_client, &current, &new).await {
            Ok(()) => {
                // Clear form
                window.set_settings_current_password("");
                window.set_settings_new_password("");
            }
            Err(e) => {
                window.set_settings_password_error(e.to_string());
            }
        }
    });
});
```

### on_settings_copy_fingerprint

```rust
window.on_settings_copy_fingerprint(move || {
    // Correctly delegates to SignalService
    match signal_service.get_identity_fingerprint() {
        Ok(fingerprint) => {
            clipboard.set_text(&fingerprint)?;
            tracing::info!("Fingerprint copied");
        }
        Err(e) => tracing::error!("Failed to get fingerprint: {}", e),
    }
});
```

### on_settings_refresh_keys

```rust
window.on_settings_refresh_keys(move || {
    tokio::spawn(async move {
        // Correctly delegates to SignalService
        match signal_service.refresh_prekeys(&mut auth_client, 100).await {
            Ok(count) => {
                // Update UI with new count
                let stats = auth_client.get_pre_key_stats(0).await?;
                window.set_settings_prekey_count(stats.available_count);
            }
            Err(e) => tracing::error!("Refresh failed: {}", e),
        }
    });
});
```

### on_settings_*_changed (Toggles)

```rust
// Read receipts, typing indicators, sealed sender
window.on_settings_read_receipts_changed(move |enabled| {
    settings_repo.set(SEND_READ_RECEIPTS, &enabled)?;
});

window.on_settings_typing_indicators_changed(move |enabled| {
    settings_repo.set(SHOW_TYPING_INDICATORS, &enabled)?;
});

window.on_settings_sealed_sender_changed(move |unrestricted| {
    settings_repo.set("privacy.sealed_sender_unrestricted", &unrestricted)?;
});
```

## create_channel.rs

### on_create_channel

```rust
window.on_create_channel(move |name, is_private| {
    let signal_service = ctx.signal_service.clone();

    tokio::spawn(async move {
        // Create channel via gRPC
        let channel = channel_client.create_channel(&name, is_private).await?;

        // Create sender key for the channel
        let skdm = signal_service.create_sender_key(&channel.id)?;

        // Store channel locally
        channel_repo.insert(&channel)?;

        // Close modal, navigate to channel
        window.set_active_modal("");
        window.set_selected_channel_id(channel.id.to_string());
    });
});
```

## join_channel.rs

### on_join_channel

```rust
window.on_join_channel(move |channel_id| {
    let signal_service = ctx.signal_service.clone();

    tokio::spawn(async move {
        // Join via gRPC
        let channel = channel_client.join_channel(&channel_id).await?;

        // Store locally
        channel_repo.insert(&channel)?;

        // Sender key will be received via SKDM from existing members

        // Close modal
        window.set_active_modal("");
    });
});
```

### on_leave_channel

```rust
window.on_leave_channel(move |channel_id| {
    let signal_service = ctx.signal_service.clone();

    tokio::spawn(async move {
        // Leave via gRPC
        channel_client.leave_channel(&channel_id).await?;

        // Clear sender keys (can't decrypt future messages)
        signal_service.clear_channel_sender_keys(&channel_id)?;

        // Remove from local storage
        channel_repo.delete(&channel_id)?;

        // Navigate away
        window.set_selected_channel_id("");
    });
});
```

## friends.rs

### on_send_friend_request

```rust
window.on_send_friend_request(move |username| {
    tokio::spawn(async move {
        match user_client.send_friend_request(&username).await {
            Ok(_) => {
                // Update pending list
                refresh_friend_requests(window);
            }
            Err(e) => {
                window.set_friends_error(e.to_string());
            }
        }
    });
});
```

### on_remove_friend

```rust
window.on_remove_friend(move |user_id| {
    tokio::spawn(async move {
        user_client.remove_friend(&user_id).await?;
        refresh_friends_list(window);
    });
});
```

## friend_requests.rs

### on_accept_friend_request

```rust
window.on_accept_friend_request(move |request_id| {
    tokio::spawn(async move {
        user_client.respond_to_friend_request(&request_id, true).await?;
        refresh_friend_requests(window);
        refresh_friends_list(window);
    });
});
```

### on_reject_friend_request

```rust
window.on_reject_friend_request(move |request_id| {
    tokio::spawn(async move {
        user_client.respond_to_friend_request(&request_id, false).await?;
        refresh_friend_requests(window);
    });
});
```

## sessions.rs

### on_revoke_session

```rust
window.on_revoke_session(move |device_id| {
    tokio::spawn(async move {
        // Revoke device on server
        auth_client.revoke_device(device_id).await?;

        // Refresh sessions list
        let sessions = auth_client.list_devices().await?;
        window.set_sessions(sessions_to_model(sessions));
    });
});
```

## Correct Patterns

### Settings - Use Services

```rust
// ✅ Fingerprint via SignalService
let fingerprint = signal_service.get_identity_fingerprint()?;

// ✅ Prekey refresh via SignalService
signal_service.refresh_prekeys(&mut auth_client, 100).await?;
```

### Channel - Sender Key Handling

```rust
// ✅ Create sender key when creating channel
let skdm = signal_service.create_sender_key(&channel.id)?;

// ✅ Clear keys when leaving
signal_service.clear_channel_sender_keys(&channel_id)?;
```

## What NOT to Do

```rust
// ❌ WRONG - Direct key store access
let fingerprint = ctx.key_store.get_fingerprint()?;

// ✅ CORRECT - Use SignalService
let fingerprint = signal_service.get_identity_fingerprint()?;
```

```rust
// ❌ WRONG - Manual prekey generation
let prekeys = ctx.prekey_manager.generate_batch(100)?;
auth_client.upload_prekeys(prekeys).await?;

// ✅ CORRECT - Use SignalService
signal_service.refresh_prekeys(&mut auth_client, 100).await?;
```
