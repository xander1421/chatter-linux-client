# Phase 1: Scaffolding

## Objective

Create the complete directory structure and stub files from ARCHITECTURE.md, establishing the foundation for all subsequent development phases.

## Execution Context

- ARCHITECTURE.md defines 17 top-level src/ modules and 10 ui/ directories
- Current state: Only `src/main.rs`, `src/lib.rs`, and `src/proto/` exist
- All new files should be minimal stubs with module declarations
- Follow Rust conventions: mod.rs for directories, snake_case for files

## Context

**Why this matters:** Every subsequent phase depends on the module structure existing. Creating stubs now:
1. Validates the architecture has no circular dependencies
2. Allows `cargo check` to verify module declarations
3. Provides clear navigation for developers
4. Enables parallel development on different modules

**Key patterns:**
- Each directory gets a `mod.rs` that re-exports submodules
- Stub files contain `//! Module documentation` and minimal type/trait placeholders
- No actual implementation - just enough to compile

## Tasks

### Task 1: Create src/ module directories
Create all top-level module directories under src/:
- `app/` - Application orchestration
- `domain/` - Core business entities
- `crypto/` - Signal Protocol and encryption
- `storage/` - SQLCipher persistence
- `network/` - gRPC clients and streaming
- `services/` - Business logic
- `sync/` - Synchronization engine
- `inbox/` - Device inbox (Signal-style)
- `mailbox/` - Ephemeral mailbox system
- `media/` - Media handling
- `system/` - OS integration
- `state/` - Application state
- `ui/` - Slint bridge and adapters
- `local_ai/` - Local AI infrastructure
- `events/` - Event system
- `error/` - Error handling
- `util/` - Shared utilities

**Checkpoint:** All 17 directories exist under src/

### Task 2: Create nested module directories
Create subdirectories as specified in ARCHITECTURE.md:
- `crypto/signal/`
- `storage/migrations/`, `storage/repositories/`
- `network/grpc/`, `network/streaming/`, `network/validation/`
- `inbox/handlers/`
- `mailbox/dm/`, `mailbox/group/`
- `media/gif/`, `media/embed/`
- `ui/adapters/`, `ui/commands/`, `ui/handlers/`, `ui/widgets/`

**Checkpoint:** All 14 nested directories exist

### Task 3: Create domain layer stubs
Create stub files in `src/domain/`:
- `mod.rs` - Module exports
- `ids.rs` - Strongly-typed ID wrappers (UserId, ChannelId, MessageId, etc.)
- `user.rs` - User, Profile, Presence
- `message.rs` - Message, Thread, Draft
- `channel.rs` - Channel, Membership, Permissions
- `attachment.rs` - Attachment, FileMetadata
- `reaction.rs` - Reaction, Emoji
- `call.rs` - Call, Participant
- `notification.rs` - Notification types
- `ai.rs` - AI Integration types
- `moderation.rs` - Ban, Mute, Role
- `saved_message.rs` - SavedMessage
- `draft.rs` - Draft
- `ephemeral.rs` - EphemeralMessage
- `mention.rs` - Mention types
- `embed.rs` - LinkEmbed, YouTubeEmbed

**Checkpoint:** `cargo check` passes for domain module

### Task 4: Create crypto layer stubs
Create stub files in `src/crypto/`:
- `mod.rs` - Module exports
- `signal/mod.rs` - Signal submodule exports
- `signal/store.rs` - SignalProtocolStore trait
- `signal/session.rs` - Session management
- `signal/identity.rs` - Identity key operations
- `signal/prekey.rs` - PreKey bundle management
- `signal/sender_key.rs` - Sender key for groups
- `sealed_sender.rs` - Anonymous messaging
- `group_encryption.rs` - Group E2EE
- `attachment_crypto.rs` - File encryption
- `key_derivation.rs` - HKDF, key stretching
- `device_credentials.rs` - Device key management
- `rng.rs` - Secure random generation

**Checkpoint:** `cargo check` passes for crypto module

### Task 5: Create storage layer stubs
Create stub files in `src/storage/`:
- `mod.rs` - Module exports
- `database.rs` - SQLCipher connection pool
- `migrations/mod.rs` - Migration infrastructure
- `migrations/v001_initial.rs` - Initial schema migration
- `repositories/mod.rs` - Repository exports
- `repositories/user_repo.rs`
- `repositories/message_repo.rs`
- `repositories/channel_repo.rs`
- `repositories/attachment_repo.rs`
- `repositories/draft_repo.rs`
- `repositories/reaction_repo.rs`
- `repositories/call_repo.rs`
- `repositories/settings_repo.rs`
- `repositories/search_index.rs` - FTS5 search
- `key_store.rs` - Signal key storage
- `cache.rs` - In-memory LRU caches
- `blob_store.rs` - File/attachment storage

**Checkpoint:** `cargo check` passes for storage module

### Task 6: Create network layer stubs
Create stub files in `src/network/`:
- `mod.rs` - Module exports
- `grpc/mod.rs` - gRPC client exports
- `grpc/client_pool.rs` - Connection pooling
- `grpc/interceptors.rs` - Auth, retry, logging
- `grpc/auth_client.rs`
- `grpc/message_client.rs`
- `grpc/channel_client.rs`
- `grpc/user_client.rs`
- `grpc/file_client.rs`
- `grpc/call_client.rs`
- `grpc/notification_client.rs`
- `grpc/integration_client.rs`
- `grpc/mailbox_client.rs`
- `streaming/mod.rs` - Streaming exports
- `streaming/stream_manager.rs`
- `streaming/message_stream.rs`
- `streaming/presence_stream.rs`
- `streaming/typing_stream.rs`
- `streaming/reaction_stream.rs`
- `streaming/read_receipt_stream.rs`
- `connection.rs` - Connection state machine
- `recovery.rs` - Exponential backoff
- `retry.rs` - Retry logic
- `offline_queue.rs` - Queued operations
- `validation/mod.rs` - Validation exports
- `validation/orderer.rs` - Message ordering
- `validation/deduplicator.rs` - Duplicate prevention
- `validation/validator.rs` - Content validation

**Checkpoint:** `cargo check` passes for network module

### Task 7: Create services layer stubs
Create stub files in `src/services/`:
- `mod.rs` and all 28 service files from ARCHITECTURE.md

**Checkpoint:** `cargo check` passes for services module

### Task 8: Create remaining src/ module stubs
Create stub files for:
- `sync/` (6 files)
- `inbox/` (11 files including handlers/)
- `mailbox/` (13 files including dm/ and group/)
- `media/` (15 files including gif/ and embed/)
- `system/` (9 files)
- `state/` (13 files)
- `ui/` (20 files including adapters/, commands/, handlers/, widgets/)
- `local_ai/` (10 files)
- `events/` (5 files)
- `error/` (6 files)
- `util/` (7 files)

**Checkpoint:** `cargo check` passes for all src/ modules

### Task 9: Create app orchestration stubs
Create stub files in `src/app/`:
- `mod.rs`
- `bootstrap.rs` - App initialization
- `shutdown.rs` - Graceful shutdown
- `config.rs` - Runtime configuration

**Checkpoint:** `cargo check` passes for app module

### Task 10: Update lib.rs with all module declarations
Update `src/lib.rs` to declare all top-level modules:
```rust
pub mod app;
pub mod domain;
pub mod crypto;
pub mod storage;
pub mod network;
pub mod services;
pub mod sync;
pub mod inbox;
pub mod mailbox;
pub mod media;
pub mod system;
pub mod state;
pub mod ui;
pub mod local_ai;
pub mod events;
pub mod error;
pub mod util;
pub mod proto;
```

**Checkpoint:** `cargo check` passes at crate root

### Task 11: Create ui/ Slint directory structure
Create Slint UI directories (separate from src/ui/):
- `ui/theme/`
- `ui/components/`
- `ui/chat/`
- `ui/sidebar/`
- `ui/modals/`
- `ui/calls/`
- `ui/settings/`
- `ui/pages/`

**Checkpoint:** All 8 ui/ directories exist

### Task 12: Create ui/ Slint stub files
Create minimal .slint stub files:
- `ui/main.slint` - Root component
- `ui/theme/colors.slint`, `typography.slint`, `spacing.slint`, `icons.slint`
- Key component stubs (avatar, button, input, modal)

**Checkpoint:** Slint files exist and can be referenced

### Task 13: Final verification
Run `cargo check` to verify:
1. All modules compile
2. No circular dependencies
3. No missing module declarations

**Checkpoint:** `cargo check` succeeds with no errors

## Verification

After completing all tasks:
1. Run `cargo check` - must pass with no errors
2. Run `find src -name "*.rs" | wc -l` - should show ~150+ files
3. Run `find ui -name "*.slint" | wc -l` - should show ~40+ files
4. Verify directory structure matches ARCHITECTURE.md

## Success Criteria

- [ ] All 17 top-level src/ modules created with mod.rs
- [ ] All nested directories created per ARCHITECTURE.md
- [ ] All stub files created with documentation comments
- [ ] `cargo check` passes with no errors
- [ ] Slint ui/ directory structure created
- [ ] lib.rs exports all modules

## Output

**Files created:** ~180 Rust source files, ~40 Slint UI files
**Result:** Complete scaffolding ready for implementation phases
