# Storage Layer

Encrypted local storage with SQLCipher.

## Overview

All data stored in encrypted SQLCipher database:
- Passphrase from system keyring
- AES-256 encryption
- Transparent to application code

## Documentation

- [Key Store](key-store.md) - Signal Protocol key storage
- [Repositories](repositories.md) - Data access patterns

## Database Location

```
~/.local/share/chatter/chatter.db
```

XDG-compliant, managed by `Directories` struct.

## Passphrase Management

```rust
// In AppContext::new()

// Try to get existing passphrase from keyring
if let Ok(passphrase) = keyring.get_passphrase() {
    return Ok(passphrase);
}

// Generate new passphrase (32 random bytes as hex)
let mut bytes = [0u8; 32];
rand::RngCore::fill_bytes(&mut rand::rng(), &mut bytes);
let passphrase = hex::encode(bytes);

// Store in system keyring
keyring.set_passphrase(&passphrase)?;
```

## Schema Migrations

Migrations in `src/storage/migrations/`:

| Version | File | Description |
|---------|------|-------------|
| v001 | `v001_initial.rs` | Users, channels, basic tables |
| v004 | `v004_messages.rs` | Message storage |
| v010 | `v010_signal_keys.rs` | Signal Protocol keys |
| v011 | `v011_dm_conversations.rs` | DM conversation state |
| v012 | `v012_dm_bootstrap.rs` | DM bootstrap mailboxes |
| v013 | `v013_group_mailboxes.rs` | Group mailbox state |

## Key Tables

### Signal Protocol Keys

```sql
-- Our identity (single row)
CREATE TABLE local_identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    registration_id INTEGER NOT NULL,
    identity_key_pair BLOB NOT NULL,
    created_at TEXT NOT NULL
);

-- Remote identities (TOFU)
CREATE TABLE remote_identities (
    address TEXT PRIMARY KEY,  -- "user_id:device_id"
    identity_key BLOB NOT NULL,
    first_seen_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Double Ratchet sessions
CREATE TABLE sessions (
    address TEXT PRIMARY KEY,
    record BLOB NOT NULL,
    updated_at TEXT NOT NULL
);

-- Group sender keys
CREATE TABLE sender_keys (
    sender TEXT NOT NULL,
    distribution_id TEXT NOT NULL,
    record BLOB NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (sender, distribution_id)
);
```

### Application Data

```sql
-- Messages
CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL,
    sender_id TEXT,
    content_type INTEGER NOT NULL,
    content_json TEXT NOT NULL,
    created_at TEXT NOT NULL,
    -- ... more fields
);

-- Channels
CREATE TABLE channels (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    channel_type INTEGER NOT NULL,
    -- ...
);

-- Users
CREATE TABLE users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    -- ...
);
```

## Repository Pattern

Each entity has a repository:

```rust
pub struct MessageRepository {
    db: Arc<Database>,
}

impl MessageRepository {
    pub fn new(db: Arc<Database>) -> Self { ... }
    pub fn insert(&self, message: &Message) -> Result<(), StorageError> { ... }
    pub fn insert_or_ignore(&self, message: &Message) -> Result<bool, StorageError> { ... }
    pub fn get(&self, id: &MessageId) -> Result<Option<Message>, StorageError> { ... }
    pub fn get_by_channel(&self, channel_id: &ChannelId, limit: u32) -> Result<Vec<Message>, StorageError> { ... }
}
```

## Files

```
storage/
├── mod.rs
├── database.rs         # SQLCipher wrapper
├── key_store.rs        # Signal key storage
├── repositories/
│   ├── mod.rs
│   ├── message_repo.rs
│   ├── channel_repo.rs
│   ├── user_repo.rs
│   ├── settings_repo.rs
│   ├── attachment_repo.rs
│   ├── reaction_repo.rs
│   └── draft_repo.rs
├── migrations/
│   ├── mod.rs
│   ├── v001_initial.rs
│   └── ...
└── search_index.rs     # FTS5 full-text search
```

## Thread Safety

Database wrapped in Arc for sharing:

```rust
let database = Arc::new(Database::open(&path, &passphrase)?);

// Repositories share the database
let message_repo = Arc::new(MessageRepository::new(database.clone()));
let channel_repo = Arc::new(ChannelRepository::new(database.clone()));
```

SQLCipher handles concurrent access internally.
