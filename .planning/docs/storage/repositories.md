# Repositories

Data access patterns for application data.

## Overview

Each entity has a dedicated repository for CRUD operations.

## Available Repositories

| Repository | Entity | Key Methods |
|------------|--------|-------------|
| `MessageRepository` | Messages | insert_or_ignore, get_by_channel |
| `ChannelRepository` | Channels | insert, get, increment_unread_count |
| `UserRepository` | Users, Profiles | get, get_profile, upsert |
| `SettingsRepository` | Key-value settings | get, set |
| `AttachmentRepository` | File metadata | insert, get |
| `ReactionRepository` | Message reactions | add, remove, get_for_message |
| `DraftRepository` | Message drafts | save, get, delete |

## MessageRepository

**File**: `src/storage/repositories/message_repo.rs`

```rust
pub struct MessageRepository {
    db: Arc<Database>,
}

impl MessageRepository {
    /// Insert new message (fails on duplicate)
    pub fn insert(&self, message: &Message) -> Result<(), StorageError>

    /// Insert with deduplication (returns false if exists)
    pub fn insert_or_ignore(&self, message: &Message) -> Result<bool, StorageError>

    /// Get by ID
    pub fn get(&self, id: &MessageId) -> Result<Option<Message>, StorageError>

    /// Get messages for channel (paginated)
    pub fn get_by_channel(
        &self,
        channel_id: &ChannelId,
        limit: u32,
        before: Option<&DateTime<Utc>>,
    ) -> Result<Vec<Message>, StorageError>

    /// Update message
    pub fn update(&self, message: &Message) -> Result<(), StorageError>

    /// Mark as deleted
    pub fn soft_delete(&self, id: &MessageId) -> Result<(), StorageError>
}
```

### Deduplication Pattern

```rust
// Always use insert_or_ignore for received messages
match message_repo.insert_or_ignore(&message)? {
    true => {
        // New message - update counts, emit events
        channel_repo.increment_unread_count(&channel_id)?;
    }
    false => {
        // Duplicate - skip silently
        tracing::trace!("Duplicate message ignored: {}", message.id);
    }
}
```

## ChannelRepository

**File**: `src/storage/repositories/channel_repo.rs`

```rust
pub struct ChannelRepository {
    db: Arc<Database>,
}

impl ChannelRepository {
    /// Insert new channel
    pub fn insert(&self, channel: &Channel) -> Result<(), StorageError>

    /// Upsert (insert or update)
    pub fn upsert(&self, channel: &Channel) -> Result<(), StorageError>

    /// Get by ID
    pub fn get(&self, id: &ChannelId) -> Result<Option<Channel>, StorageError>

    /// Get all channels for user
    pub fn get_all(&self) -> Result<Vec<Channel>, StorageError>

    /// Increment unread count
    pub fn increment_unread_count(&self, id: &ChannelId) -> Result<(), StorageError>

    /// Clear unread count (on channel open)
    pub fn clear_unread_count(&self, id: &ChannelId) -> Result<(), StorageError>

    /// Get unread count
    pub fn get_unread_count(&self, id: &ChannelId) -> Result<u32, StorageError>

    /// Delete channel
    pub fn delete(&self, id: &ChannelId) -> Result<(), StorageError>
}
```

### Unread Count Pattern

```rust
// When message received
channel_repo.increment_unread_count(&channel_id)?;

// When channel opened
channel_repo.clear_unread_count(&channel_id)?;
```

## UserRepository

**File**: `src/storage/repositories/user_repo.rs`

```rust
pub struct UserRepository {
    db: Arc<Database>,
}

impl UserRepository {
    /// Get user by ID
    pub fn get(&self, id: &UserId) -> Result<Option<User>, StorageError>

    /// Get user profile
    pub fn get_profile(&self, id: &UserId) -> Result<Option<UserProfile>, StorageError>

    /// Upsert user
    pub fn upsert(&self, user: &User) -> Result<(), StorageError>

    /// Upsert profile
    pub fn upsert_profile(&self, profile: &UserProfile) -> Result<(), StorageError>

    /// Get friends
    pub fn get_friends(&self) -> Result<Vec<User>, StorageError>

    /// Search users
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<User>, StorageError>
}
```

## SettingsRepository

**File**: `src/storage/repositories/settings_repo.rs`

Key-value storage for settings.

```rust
pub struct SettingsRepository {
    db: Arc<Database>,
}

impl SettingsRepository {
    /// Get typed setting
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StorageError>

    /// Set setting
    pub fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<(), StorageError>

    /// Get string setting
    pub fn get_string(&self, key: &str) -> Result<Option<String>, StorageError>

    /// Delete setting
    pub fn delete(&self, key: &str) -> Result<(), StorageError>
}
```

### Common Settings Keys

```rust
// In src/storage/settings_keys.rs
pub const USER_ID: &str = "auth.user_id";
pub const DEVICE_ID: &str = "auth.device_id";
pub const DISPLAY_NAME: &str = "user.display_name";
pub const SEND_READ_RECEIPTS: &str = "privacy.send_read_receipts";
pub const SHOW_TYPING_INDICATORS: &str = "privacy.show_typing_indicators";
pub const SENDER_CERTIFICATE: &str = "signal.sender_certificate";
pub const SENDER_CERTIFICATE_EXPIRY: &str = "signal.sender_certificate_expiry";
```

## AttachmentRepository

**File**: `src/storage/repositories/attachment_repo.rs`

```rust
pub struct AttachmentRepository {
    db: Arc<Database>,
}

impl AttachmentRepository {
    /// Insert attachment metadata
    pub fn insert(&self, attachment: &Attachment) -> Result<(), StorageError>

    /// Get by ID
    pub fn get(&self, id: &AttachmentId) -> Result<Option<Attachment>, StorageError>

    /// Update status
    pub fn update_status(
        &self,
        id: &AttachmentId,
        status: AttachmentStatus,
    ) -> Result<(), StorageError>

    /// Get attachments for message
    pub fn get_for_message(&self, message_id: &MessageId) -> Result<Vec<Attachment>, StorageError>
}
```

## ReactionRepository

**File**: `src/storage/repositories/reaction_repo.rs`

```rust
pub struct ReactionRepository {
    db: Arc<Database>,
}

impl ReactionRepository {
    /// Add reaction
    pub fn add(
        &self,
        message_id: &MessageId,
        user_id: &UserId,
        emoji: &str,
    ) -> Result<(), StorageError>

    /// Remove reaction
    pub fn remove(
        &self,
        message_id: &MessageId,
        user_id: &UserId,
        emoji: &str,
    ) -> Result<(), StorageError>

    /// Get reactions for message
    pub fn get_for_message(&self, message_id: &MessageId) -> Result<Vec<Reaction>, StorageError>

    /// Get reaction counts
    pub fn get_counts(&self, message_id: &MessageId) -> Result<Vec<ReactionCount>, StorageError>
}
```

## DraftRepository

**File**: `src/storage/repositories/draft_repo.rs`

```rust
pub struct DraftRepository {
    db: Arc<Database>,
}

impl DraftRepository {
    /// Save draft
    pub fn save(&self, channel_id: &ChannelId, text: &str) -> Result<(), StorageError>

    /// Get draft
    pub fn get(&self, channel_id: &ChannelId) -> Result<Option<String>, StorageError>

    /// Delete draft
    pub fn delete(&self, channel_id: &ChannelId) -> Result<(), StorageError>
}
```

## Transaction Pattern

For multi-step operations:

```rust
let tx = db.transaction()?;

// Multiple operations
message_repo.insert_with_tx(&tx, &message)?;
channel_repo.increment_unread_with_tx(&tx, &channel_id)?;

tx.commit()?;
```

## Error Handling

```rust
pub enum StorageError {
    /// Database error
    Database(rusqlite::Error),

    /// Entity not found
    NotFound,

    /// Invalid data
    InvalidData(String),

    /// Serialization error
    Serialization(String),
}
```

## Getting Repositories

From AppContext:

```rust
let message_repo = ctx.message_repo.clone();
let channel_repo = ctx.channel_repo.clone();
let user_repo = ctx.user_repo.clone();
let settings_repo = ctx.settings_repo.clone();
let attachment_repo = ctx.attachment_repo.clone();
let reaction_repo = ctx.reaction_repo.clone();
let draft_repo = ctx.draft_repo.clone();
```
