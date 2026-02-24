# UI Layer Separation of Concerns Refactor

## Problem Summary

The `bin_ui/` layer has accumulated significant business logic that should be in the services layer. This violates separation of concerns and makes the codebase harder to maintain and test.

**Statistics:**
- 60+ direct gRPC client instantiations in UI
- 73 tokio::spawn/spawn_blocking calls in UI
- 119 .await calls in UI
- 245 grpc_pool references in UI

## Architecture Goals

```
┌─────────────────────────────────────────────────────────────┐
│                        UI Layer (bin_ui/)                    │
│  - Bind Slint callbacks                                      │
│  - Convert domain types to Slint types                       │
│  - Update Slint properties                                   │
│  - NO network calls, NO business logic, NO async operations  │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                     Services Layer (services/)               │
│  - Business logic                                            │
│  - Orchestrate network calls                                 │
│  - Handle errors and retries                                 │
│  - Emit events for UI updates                                │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                    Network Layer (network/)                  │
│  - gRPC clients                                              │
│  - HTTP clients                                              │
│  - Connection pooling                                        │
└─────────────────────────────────────────────────────────────┘
```

---

## Culprits by File

### CRITICAL: `modals/friends.rs` (14 client creations)

**Current violations:**
```rust
// Line 58: Direct client creation
let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());

// Line 140: Search users
let mut user_client = UserClient::new(...);
user_client.search_users(&query).await

// Line 183: Send friend request
let mut user_client = UserClient::new(...);
user_client.send_friend_request(&user_id).await

// Line 320: Get DM channel after friend accept
let mut channel_client = ChannelClient::new(...);
channel_client.get_dm_channel(&friend_id).await

// Line 422-669: Multiple friend list operations
```

**Fix:** Create `FriendService`
```rust
// services/friend_service.rs
pub struct FriendService {
    grpc_pool: Arc<GrpcClientPool>,
    user_repo: Arc<UserRepository>,
    channel_repo: Arc<ChannelRepository>,
}

impl FriendService {
    pub async fn search_users(&self, query: &str) -> Result<Vec<UserSearchResult>, FriendError>;
    pub async fn send_friend_request(&self, user_id: &UserId) -> Result<(), FriendError>;
    pub async fn accept_friend_request(&self, user_id: &UserId) -> Result<ChannelId, FriendError>;
    pub async fn reject_friend_request(&self, user_id: &UserId) -> Result<(), FriendError>;
    pub async fn cancel_friend_request(&self, user_id: &UserId) -> Result<(), FriendError>;
    pub async fn remove_friend(&self, user_id: &UserId) -> Result<(), FriendError>;
    pub async fn get_friends(&self) -> Result<Vec<Friend>, FriendError>;
    pub async fn get_friend_requests(&self) -> Result<FriendRequests, FriendError>;
}
```

---

### CRITICAL: `modals/profile.rs` (5 client creations)

**Current violations:**
```rust
// Line 59: Fetch own profile
let mut user_client = UserClient::new(...);
user_client.get_user_profile(&user_id).await

// Line 162: Update display name
user_client.update_profile(...).await

// Line 232: Update avatar
user_client.upload_avatar(...).await

// Line 303: Update status
user_client.update_status(...).await
```

**Fix:** Extend `UserService` or create `ProfileService`
```rust
// services/profile_service.rs (or add to user_service.rs)
impl ProfileService {
    pub async fn get_profile(&self, user_id: &UserId) -> Result<Profile, ProfileError>;
    pub async fn update_display_name(&self, name: &str) -> Result<(), ProfileError>;
    pub async fn update_avatar(&self, image_path: &Path) -> Result<String, ProfileError>;
    pub async fn update_status(&self, status: &str) -> Result<(), ProfileError>;
}
```

---

### CRITICAL: `modals/friend_requests.rs` (5 client creations)

**Current violations:**
```rust
// Line 75: Accept friend request
let mut user_client = UserClient::new(...);
user_client.accept_friend_request(&user_id).await

// Line 152: Reject friend request
user_client.reject_friend_request(&user_id).await

// Line 216: Cancel sent request
user_client.cancel_friend_request(&user_id).await

// Line 272: Get DM channel
let mut channel_client = ChannelClient::new(...);
```

**Fix:** Use `FriendService` (same as friends.rs)

---

### CRITICAL: `modals/join_channel.rs` (5 client creations)

**Current violations:**
```rust
// Line 85: Search channels
let mut channel_client = ChannelClient::new(...);
channel_client.search_channels(&query).await

// Line 206-225: Join channel + sender key distribution
let mut channel_client = ChannelClient::new(...);
let mut mailbox_client = MailboxClient::new(...);
let mut message_client = MessageClient::new(...);
let mut auth_client = AuthClient::new(...);
```

**Fix:** Extend `ChannelService`
```rust
// services/channel/service.rs - add methods
impl ChannelService {
    pub async fn search_channels(&self, query: &str) -> Result<Vec<ChannelSearchResult>, ChannelError>;
    pub async fn join_channel(&self, channel_id: &ChannelId) -> Result<Channel, ChannelError>;
    // join_channel internally handles sender key distribution
}
```

---

### CRITICAL: `modals/create_channel.rs` (4 client creations)

**Current violations:**
```rust
// Line 78: Create channel
let mut channel_client = ChannelClient::new(...);
channel_client.create_channel(...).await

// Line 222, 371: AI integration setup
let mut integration_client = IntegrationClient::new(...);

// Line 477: Group mailbox creation
let mut mailbox_client = MailboxClient::new(...);
```

**Fix:** Extend `ChannelService`
```rust
impl ChannelService {
    pub async fn create_channel(&self, params: CreateChannelParams) -> Result<Channel, ChannelError>;
    pub async fn setup_ai_integration(&self, channel_id: &ChannelId, config: AiConfig) -> Result<(), ChannelError>;
}
```

---

### HIGH: `chat/send_message.rs` (4 client creations)

**Current violations:**
```rust
// Line 162: Auth client for key fetching
let mut auth_client = AuthClient::new(...);

// Line 393-402: Message sending with multiple clients
let mut message_client = MessageClient::new(...);
let mut auth_client = AuthClient::new(...);
let mut channel_client = ChannelClient::new(...);
```

**Fix:** Use existing `MessageService` properly
```rust
// The MessageService already exists! Just use it instead of raw clients
// services/message_service.rs already has send_dm_content, send_channel_message
```

---

### HIGH: `chat/helpers.rs` (4 client creations)

**Current violations:**
```rust
// Line 90: Ensure DM session
let mut auth_client = AuthClient::new(...);

// Line 162-164: Load DM messages
let mut message_client = MessageClient::new(...);
let mut auth_client = AuthClient::new(...);
let mut channel_client = ChannelClient::new(...);
```

**Fix:** Use `MessageService` and `ChannelService`
```rust
// Move ensure_dm_session to MessageService
// Move load_dm_messages to MessageService or create DmService
```

---

### HIGH: `auth/inbox_events.rs` (4 client creations)

**Current violations:**
```rust
// Line 604, 667: Session recovery
let mut auth_client = AuthClient::new(...);

// Line 723 (new): Auto-download attachments
let attachment_service = AttachmentService::new(...);
attachment_service.download_from_cdn_encrypted(...).await

// Line 1979, 2032: Friend acceptance handling
let mut channel_client = ChannelClient::new(...);
let mut user_client = UserClient::new(...);
```

**Fix:** Create `InboxEventProcessor` service
```rust
// services/inbox_event_processor.rs
pub struct InboxEventProcessor {
    message_service: Arc<MessageService>,
    attachment_service: Arc<AttachmentService>,
    friend_service: Arc<FriendService>,
    signal_service: Arc<SignalService>,
}

impl InboxEventProcessor {
    /// Process inbox event and return UI updates
    pub async fn process(&self, event: InboxEvent) -> Result<UiUpdate, ProcessError>;

    // Internal methods
    async fn handle_dm_received(&self, ...) -> Result<DmReceivedUpdate, ProcessError>;
    async fn handle_session_recovery(&self, ...) -> Result<(), ProcessError>;
    async fn auto_download_attachments(&self, ...) -> Result<Vec<AttachmentUpdate>, ProcessError>;
}

// The UI layer then just applies the UiUpdate to Slint
```

---

### MEDIUM: `features/thread.rs` (3 client creations)

**Current violations:**
```rust
// Line 95-98: Load thread messages
let mut message_client = MessageClient::new(...);
let mut auth_client = AuthClient::new(...);
let mut channel_client = ChannelClient::new(...);
```

**Fix:** Add to `MessageService`
```rust
impl MessageService {
    pub async fn load_thread(&self, channel_id: &ChannelId, parent_id: &MessageId) -> Result<Vec<Message>, MessageError>;
}
```

---

### MEDIUM: `modals/sessions.rs` (3 client creations)

**Current violations:**
```rust
// Line 35, 97, 179: Session management
let mut auth_client = AuthClient::new(...);
auth_client.list_sessions().await
auth_client.revoke_session(...).await
auth_client.revoke_all_other_sessions().await
```

**Fix:** Create `SessionService` or add to `AuthService`
```rust
impl SessionService {
    pub async fn list_sessions(&self) -> Result<Vec<Session>, SessionError>;
    pub async fn revoke_session(&self, session_id: &str) -> Result<(), SessionError>;
    pub async fn revoke_all_other(&self) -> Result<(), SessionError>;
}
```

---

### MEDIUM: `modals/settings.rs` (2 client creations)

**Current violations:**
```rust
// Line 41, 154: Password change, account deletion
let mut auth_client = AuthClient::new(...);
```

**Fix:** Add to `AuthService`

---

### LOW: `features/composer.rs` (2 client creations)

**Current violations:**
```rust
// Line 794, 802: Mention autocomplete
let mut channel_client = ChannelClient::new(...);
let mut user_client = UserClient::new(...);
```

**Fix:** Create `MentionService` or add to `ChannelService`

---

### LOW: `auth/initial_data.rs` (2 client creations)

**Current violations:**
```rust
// Line 38, 153: Load initial channels and friends
let mut channel_client = ChannelClient::new(...);
let mut user_client = UserClient::new(...);
```

**Fix:** Create `BootstrapService` for initial data loading

---

## Refactoring Priority

### Phase 1: Create Core Services (HIGH IMPACT)
1. **`FriendService`** - Consolidates friends.rs + friend_requests.rs (19 client creates)
2. **`InboxEventProcessor`** - Handles all inbox event business logic
3. **Extend `ChannelService`** - Add search, join, create with sender keys

### Phase 2: Move Remaining Logic (MEDIUM IMPACT)
4. **`ProfileService`** - Profile operations
5. **`SessionService`** - Session management
6. Use existing `MessageService` properly in send_message.rs, helpers.rs

### Phase 3: Cleanup (LOW IMPACT)
7. **`MentionService`** - Mention autocomplete
8. **`BootstrapService`** - Initial data loading
9. Extend `AuthService` - Settings operations

---

## Implementation Pattern

### Before (UI doing network calls):
```rust
// bin_ui/features/modals/friends.rs
fn wire_send_friend_request(window: &MainWindow, ctx: &AppContext) {
    let grpc_pool = ctx.grpc_pool.clone();

    window.on_send_friend_request(move |user_id| {
        let grpc_pool = grpc_pool.clone();
        tokio::spawn(async move {
            let mut user_client = UserClient::new(grpc_pool.channel(), grpc_pool.auth_clone());
            match user_client.send_friend_request(&user_id).await {
                Ok(_) => { /* update UI */ }
                Err(e) => { /* handle error */ }
            }
        });
    });
}
```

### After (UI calls service, service does network):
```rust
// services/friend_service.rs
impl FriendService {
    pub async fn send_friend_request(&self, user_id: &UserId) -> Result<(), FriendError> {
        let mut client = UserClient::new(self.grpc_pool.channel(), self.grpc_pool.auth_clone());
        client.send_friend_request(&user_id.to_string()).await
            .map_err(FriendError::Network)?;
        Ok(())
    }
}

// bin_ui/features/modals/friends.rs
fn wire_send_friend_request(window: &MainWindow, ctx: &AppContext) {
    let friend_service = ctx.friend_service.clone();
    let weak = window.as_weak();

    window.on_send_friend_request(move |user_id| {
        let friend_service = friend_service.clone();
        let weak = weak.clone();
        tokio::spawn(async move {
            match friend_service.send_friend_request(&user_id.parse().unwrap()).await {
                Ok(_) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() {
                            w.set_friend_request_sent(true);
                        }
                    });
                }
                Err(e) => { /* show error toast */ }
            }
        });
    });
}
```

---

## Files to Create

```
src/services/
├── friend_service.rs          # NEW - Friend operations
├── profile_service.rs         # NEW - Profile operations
├── session_service.rs         # NEW - Session management
├── inbox_event_processor.rs   # NEW - Inbox event handling
├── mention_service.rs         # NEW - Mention autocomplete
├── bootstrap_service.rs       # NEW - Initial data loading
├── channel/
│   └── service.rs             # EXTEND - Add search, join, create
├── message_service.rs         # EXTEND - Add thread loading, DM helpers
└── auth_service.rs            # EXTEND - Add settings operations
```

---

## Success Criteria

After refactoring:
- [ ] Zero `::new(grpc_pool` in bin_ui/
- [ ] Zero direct `Client::new(` in bin_ui/
- [ ] All `.await` in bin_ui/ are only for `slint::invoke_from_event_loop`
- [ ] All `tokio::spawn` in bin_ui/ only wrap service calls, not business logic
- [ ] Services are injected via `AppContext`, not created inline

---

## Progress Log

### Phase 1 Progress (2026-02-19)

**Services updated to own grpc_pool:**
- [x] `SessionService` - Now creates AuthClient internally
- [x] `UserService` - Now creates UserClient internally

**UI files refactored:**
- [x] `modals/sessions.rs` - Uses SessionService, no direct client creation
- [x] `modals/profile.rs` - Uses UserService, no direct client creation
- [x] `modals/friend_requests.rs` - Uses UserService (helper still has direct calls)
- [x] `modals/friends.rs` - Uses UserService (13 → 2 client creations, remaining are in add_dm_to_sidebar helper)

**Current client creation count:** 28 (down from 60+)

### Phase 2 Progress (2026-02-19)

**Services updated to own grpc_pool:**
- [x] `ChannelService` - Now creates ChannelClient/MailboxClient internally
- [x] `MessageService` - Now creates MessageClient/AuthClient/ChannelClient internally
  - Added `send_control()` - simplified send_control_content (no client params)
  - Added `send_dm()` - simplified send_dm_content (no client params)
  - Added `announce_group_mailbox()` - simplified send_group_mailbox_announcement

**UI files refactored:**
- [x] `modals/join_channel.rs` - Uses ChannelService (5 → 3 client creations, remaining are for SKDM distribution via MessageService)
- [x] `modals/create_channel.rs` - Uses ChannelService (4 → 2 client creations for AI integration)
- [x] `auth/initial_data.rs` - Uses ChannelService and UserService (0 client creations)
- [x] `chat/helpers.rs` - Uses MessageService simplified methods (4 → 0 client creations)
- [x] `chat/delete.rs` - Updated to use new helper signatures
- [x] `chat/edit.rs` - Updated to use new helper signatures
- [x] `chat/pin.rs` - Updated to use new helper signatures
- [x] `chat/reactions.rs` - Updated to use new helper signatures
- [x] `chat/typing.rs` - Updated to use new helper signatures
- [x] `features/sidebar.rs` - Updated to use new helper signatures

**Remaining files:**
- `chat/send_message.rs` - 4 client creations (should use MessageService methods)
- `auth/inbox_events.rs` - 4 client creations (needs InboxEventProcessor or MessageService methods)
- `features/thread.rs` - 3 client creations (should use MessageService methods)
- `modals/settings.rs` - 2 client creations (should extend SessionService)
- `features/composer.rs` - 2 client creations (mention autocomplete)
- `modals/friends.rs` - 2 client creations (add_dm_to_sidebar helper)
- `modals/friend_requests.rs` - 2 client creations (same helper pattern)
- `modals/create_channel.rs` - 2 client creations (AI integration)
- `modals/join_channel.rs` - 3 client creations (SKDM distribution)
