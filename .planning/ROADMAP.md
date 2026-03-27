# Roadmap: Chatter Native Client

## Overview

Build a production-grade native Rust+Slint desktop client for Chatter from the ground up. Following bottom-up architecture: domain entities → storage → crypto → network → inbox/mailbox → services → state → system integration → UI. Each layer builds on the previous, culminating in a full-featured encrypted chat client.

## Domain Expertise

None (custom application, no pre-existing domain skills)

## Phases

**Phase Numbering:**
- Integer phases (1, 2, 3...): Planned milestone work
- Decimal phases (X.1, X.2): Urgent insertions (marked with INSERTED)

### Foundation
- [x] **Phase 1: Scaffolding** - Create all directories and stub files from ARCHITECTURE.md

### Domain Layer
- [x] **Phase 2: Domain Core Entities** - User, Message, Channel, Attachment, IDs
- [x] **Phase 3: Domain Supporting Entities** - Reaction, Notification, Call, Moderation
- [x] **Phase 4: Domain Privacy Entities** - Draft, Ephemeral, SavedMessage, Mention, Embed

### Storage Layer
- [x] **Phase 5: Storage Database Setup** - SQLCipher connection, migrations infrastructure
- [x] **Phase 6: Storage Core Repositories** - UserRepo, MessageRepo, ChannelRepo
- [x] **Phase 7: Storage Supporting Repositories** - AttachmentRepo, ReactionRepo, DraftRepo, SettingsRepo
- [x] **Phase 8: Storage Search Index** - FTS5 full-text search
- [x] **Phase 9: Storage Key Store** - Signal protocol key storage

### Crypto Layer
- [x] **Phase 10: Crypto Signal Store** - SignalProtocolStore trait implementation
- [x] **Phase 11: Crypto Identity & Sessions** - Identity keys, session management
- [x] **Phase 12: Crypto Prekeys** - PreKey bundles, signed prekeys, OTPKs
- [x] **Phase 13: Crypto Sender Keys** - Group sender key distribution
- [x] **Phase 14: Crypto Sealed Sender** - Anonymous messaging, certificates
- [x] **Phase 15: Crypto Attachments** - File encryption/decryption

### Network Layer
- [ ] **Phase 16: Network gRPC Foundation** - Client pool, interceptors, connection
- [ ] **Phase 17: Network Auth & User Clients** - AuthClient, UserClient
- [ ] **Phase 18: Network Channel & Message Clients** - ChannelClient, MessageClient
- [x] **Phase 19: Network Supporting Clients** - FileClient, NotificationClient, IntegrationClient, MailboxClient
- [x] **Phase 20: Network Streaming** - StreamManager, real-time streams
- [x] **Phase 21: Network Resilience** - Connection recovery, retry, offline queue
- [x] **Phase 22: Network Validation** - Orderer, deduplicator, validator pipeline

### Inbox & Mailbox
- [x] **Phase 23: Inbox Provider & Poller** - DeviceInboxProvider, polling loop
- [x] **Phase 24: Inbox Message Handlers** - Chat, attachment, reaction, receipt, typing
- [x] **Phase 25: Inbox Sync Handlers** - Key exchange, sender key, pin, sync
- [ ] **Phase 26: Mailbox Token Manager** - Blind signature tokens, daily quota
- [ ] **Phase 27: Mailbox DM System** - Bootstrap, conversation keys, two-way anonymity
- [ ] **Phase 28: Mailbox Group System** - Member mailboxes, N-way fanout
- [x] **Phase 29: Mailbox Privacy Features** - Cover traffic, key rolling

### Services Layer
- [x] **Phase 30: Services Auth** - Login, logout, device auth
- [x] **Phase 31: Services Account** - Password change, deletion, session management
- [x] **Phase 32: Services Message Core** - Send, receive, edit, delete
- [x] **Phase 33: Services Message Advanced** - Forward, quote, pin, thread, draft
- [x] **Phase 34: Services Ephemeral Messages** - TTL support, countdown timers
- [ ] **Phase 35: Services Channel** - Create, join, leave, archive, templates
- [ ] **Phase 36: Services User** - Profile, status, friends, blocking
- [ ] **Phase 37: Services Presence** - Online/offline/away/DND
- [ ] **Phase 38: Services Real-time** - Typing indicators, read receipts
- [ ] **Phase 39: Services Reactions** - Add/remove, streaming
- [ ] **Phase 40: Services Search** - Global search, semantic search
- [ ] **Phase 41: Services Moderation** - Ban, mute, kick, permissions
- [x] **Phase 42: Services AI Integration** - Server-side AI, summarization
- [ ] **Phase 43: Services Saved Messages** - Personal vault

### State & Events
- [ ] **Phase 44: State Core Stores** - AppState, AuthState, UIState
- [ ] **Phase 45: State Chat Stores** - ChatState, MessageState, PinnedState
- [ ] **Phase 46: State User Stores** - UserState, PresenceState, PrivacyState
- [ ] **Phase 47: State Streaming & Offline** - StreamingState, OfflineState
- [x] **Phase 48: Events Bus & Handlers** - EventBus, domain/UI events

### System Integration
- [x] **Phase 49: System Keyring & Directories** - Secret Service, XDG
- [x] **Phase 50: System Tray & Notifications** - StatusNotifierItem, notify-rust
- [x] **Phase 51: System Utilities** - Clipboard, open URLs, autostart

### Media
- [x] **Phase 52: Media File Handling** - File picker, MIME, thumbnails
- [x] **Phase 53: Media Playback** - Audio (rodio), video player
- [x] **Phase 54: Media Recording** - Voice, video recorder
- [x] **Phase 55: Media Previews** - PDF viewer, code preview
- [ ] **Phase 56: Media GIF & Embeds** - GIF picker, YouTube/link embeds

### UI Layer
- [ ] **Phase 57: UI Foundation** - Slint setup, bridge, theme
- [ ] **Phase 58: UI Basic Components** - Avatar, button, input, modal, toast
- [ ] **Phase 59: UI Pickers** - Emoji, GIF, mention autocomplete
- [x] **Phase 60: UI Sidebar** - Channel list, DM list, user profile
- [x] **Phase 61: UI Chat Core** - Message list, message item, input
- [ ] **Phase 62: UI Chat Features** - Pinned bar, thread panel, reactions
- [x] **Phase 63: UI File Components** - Attachments, previews
- [x] **Phase 64: UI Modals** - Create channel, settings, profile, forward
- [x] **Phase 65: UI Settings Screens** - Account, privacy, encryption
- [x] **Phase 66: UI Auth Screens** - Login, register, loading

## Phase Details

### Phase 1: Scaffolding
**Goal**: Create complete directory structure and stub files from ARCHITECTURE.md
**Depends on**: Nothing (first phase)
**Research**: Unlikely (following documented architecture)
**Plans**: 1-1-PLAN.md (13 tasks)

### Phase 2: Domain Core Entities
**Goal**: Implement User, Message, Channel, Attachment structs and strongly-typed IDs
**Depends on**: Phase 1
**Research**: Unlikely (standard Rust structs)
**Plans**: 2/2 complete ✓

### Phase 3: Domain Supporting Entities
**Goal**: Implement Reaction, Notification, Call, Moderation domain types
**Depends on**: Phase 2
**Research**: Unlikely (standard Rust structs)
**Plans**: 2/2 complete ✓

### Phase 4: Domain Privacy Entities
**Goal**: Implement Draft, EphemeralMessage, SavedMessage, Mention, Embed types
**Depends on**: Phase 2
**Research**: Unlikely (standard Rust structs)
**Plans**: 2/2 complete ✓

### Phase 5: Storage Database Setup
**Goal**: SQLCipher database connection pool and migration infrastructure
**Depends on**: Phase 4
**Research**: Unlikely (rusqlite/SQLCipher documented)
**Plans**: 2/2 complete ✓

### Phase 6: Storage Core Repositories
**Goal**: UserRepo, MessageRepo, ChannelRepo with CRUD operations
**Depends on**: Phase 5
**Research**: Unlikely (standard repository pattern)
**Plans**: 3 (06-01: Users, 06-02: Channels, 06-03: Messages)

### Phase 7: Storage Supporting Repositories
**Goal**: AttachmentRepo, ReactionRepo, DraftRepo, SettingsRepo
**Depends on**: Phase 6
**Research**: Unlikely (standard repository pattern)
**Plans**: 4 (07-01: Attachments, 07-02: Reactions, 07-03: Drafts, 07-04: Settings)

### Phase 8: Storage Search Index
**Goal**: FTS5 full-text search index for messages
**Depends on**: Phase 6
**Research**: Unlikely (SQLite FTS5 documented)
**Plans**: 1/1 complete ✓

### Phase 9: Storage Key Store
**Goal**: Signal protocol key storage (identity, prekey, session, sender_key)
**Depends on**: Phase 5
**Research**: Unlikely (following libsignal storage traits)
**Plans**: TBD

### Phase 10: Crypto Signal Store
**Goal**: Implement SignalProtocolStore trait for libsignal-client
**Depends on**: Phase 9
**Research**: Likely (libsignal-client API)
**Research topics**: libsignal-client store traits, async requirements, error handling
**Plans**: TBD

### Phase 11: Crypto Identity & Sessions
**Goal**: Identity key generation/storage, session establishment
**Depends on**: Phase 10
**Research**: Likely (Signal Protocol specifics)
**Research topics**: X3DH flow, session creation, identity trust
**Plans**: TBD

### Phase 12: Crypto Prekeys
**Goal**: PreKey bundle generation, signed prekeys, one-time prekeys
**Depends on**: Phase 11
**Research**: Likely (Signal Protocol specifics)
**Research topics**: Prekey rotation, bundle format, server sync
**Plans**: TBD

### Phase 13: Crypto Sender Keys
**Goal**: Group sender key distribution and management
**Depends on**: Phase 12
**Research**: Likely (Signal Protocol specifics)
**Research topics**: SKDM format, distribution flow, ratcheting
**Plans**: TBD

### Phase 14: Crypto Sealed Sender
**Goal**: Anonymous message sending with sender certificates
**Depends on**: Phase 13
**Research**: Likely (Signal Protocol specifics)
**Research topics**: Certificate format, UAK derivation, envelope structure
**Plans**: TBD

### Phase 15: Crypto Attachments
**Goal**: File encryption/decryption with AES-GCM
**Depends on**: Phase 10
**Research**: Unlikely (standard crypto patterns)
**Plans**: TBD

### Phase 16: Network gRPC Foundation
**Goal**: tonic client pool, auth interceptors, connection state machine
**Depends on**: Phase 4
**Research**: Unlikely (tonic/prost documented)
**Plans**: TBD

### Phase 17: Network Auth & User Clients
**Goal**: AuthClient (login, register, device auth), UserClient
**Depends on**: Phase 16
**Research**: Unlikely (following proto definitions)
**Plans**: TBD

### Phase 18: Network Channel & Message Clients
**Goal**: ChannelClient, MessageClient with all operations
**Depends on**: Phase 17
**Research**: Unlikely (following proto definitions)
**Plans**: TBD

### Phase 19: Network Supporting Clients
**Goal**: FileClient, NotificationClient, IntegrationClient, MailboxClient
**Depends on**: Phase 17
**Research**: Unlikely (following proto definitions)
**Plans**: TBD

### Phase 20: Network Streaming
**Goal**: StreamManager for message, presence, typing, reaction streams
**Depends on**: Phase 18
**Research**: Unlikely (tonic streaming documented)
**Plans**: 4 (20-01: StreamManager, 20-02: MessageStream, 20-03: PresenceStream, 20-04: Specialized Handlers)

### Phase 21: Network Resilience
**Goal**: Connection recovery, exponential backoff, offline queue
**Depends on**: Phase 20
**Research**: Unlikely (standard patterns)
**Plans**: TBD

### Phase 22: Network Validation
**Goal**: Message orderer, deduplicator, validator pipeline
**Depends on**: Phase 20
**Research**: Unlikely (standard patterns)
**Plans**: TBD

### Phase 23: Inbox Provider & Poller
**Goal**: DeviceInboxProvider, polling loop with configurable interval
**Depends on**: Phase 19, Phase 14
**Research**: Unlikely (internal design)
**Plans**: TBD

### Phase 24: Inbox Message Handlers
**Goal**: Handlers for chat, attachment, reaction, receipt, typing messages
**Depends on**: Phase 23
**Research**: Unlikely (internal design)
**Plans**: TBD

### Phase 25: Inbox Sync Handlers
**Goal**: Handlers for key exchange, sender key, pin, sync messages
**Depends on**: Phase 24
**Research**: Unlikely (internal design)
**Plans**: TBD

### Phase 26: Mailbox Token Manager
**Goal**: Blind signature token acquisition and spending
**Depends on**: Phase 19
**Research**: Likely (NDSS '21 paper)
**Research topics**: Chaum blind signatures, RSA blinding, token lifecycle
**Plans**: TBD

### Phase 27: Mailbox DM System
**Goal**: DM bootstrap, conversation keys, two-way anonymity
**Depends on**: Phase 26, Phase 14
**Research**: Likely (NDSS '21 paper)
**Research topics**: Bootstrap flow, ephemeral key exchange, mailbox rotation
**Plans**: TBD

### Phase 28: Mailbox Group System
**Goal**: Per-member group mailboxes, N-way fanout delivery
**Depends on**: Phase 27
**Research**: Likely (NDSS '21 paper)
**Research topics**: Group mailbox coordination, fanout strategy
**Plans**: TBD

### Phase 29: Mailbox Privacy Features
**Goal**: Cover traffic generation, key rolling, content padding
**Depends on**: Phase 28
**Research**: Likely (NDSS '21 paper)
**Research topics**: Timing obfuscation, rotation intervals, padding schemes
**Plans**: TBD

### Phase 30: Services Auth
**Goal**: Login, logout, device authentication service
**Depends on**: Phase 17, Phase 10
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 31: Services Account
**Goal**: Password change, account deletion, session list/revoke
**Depends on**: Phase 30
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 32: Services Message Core
**Goal**: Send, receive, edit, delete message operations
**Depends on**: Phase 18, Phase 14, Phase 6
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 33: Services Message Advanced
**Goal**: Forward, quote, pin, thread, draft functionality
**Depends on**: Phase 32
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 34: Services Ephemeral Messages
**Goal**: Self-destructing messages with TTL (30s-24h)
**Depends on**: Phase 32
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 35: Services Channel
**Goal**: Create, join, leave, archive, settings, templates
**Depends on**: Phase 18, Phase 6
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 36: Services User
**Goal**: Profile management, status, friends, blocking
**Depends on**: Phase 17, Phase 6
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 37: Services Presence
**Goal**: Online/offline/away/DND status tracking
**Depends on**: Phase 36, Phase 20
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 38: Services Real-time
**Goal**: Typing indicators, read receipts
**Depends on**: Phase 20, Phase 24
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 39: Services Reactions
**Goal**: Add/remove reactions with streaming support
**Depends on**: Phase 32, Phase 20
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 40: Services Search
**Goal**: Global search using FTS5, semantic search prep
**Depends on**: Phase 8
**Research**: Unlikely (FTS5 documented)
**Plans**: TBD

### Phase 41: Services Moderation
**Goal**: Ban, mute, kick, permission management
**Depends on**: Phase 35
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 42: Services AI Integration
**Goal**: Server-side AI chat, conversation summarization
**Depends on**: Phase 19
**Research**: Unlikely (following existing backend API)
**Plans**: TBD

### Phase 43: Services Saved Messages
**Goal**: Personal message vault functionality
**Depends on**: Phase 32
**Research**: Unlikely (internal business logic)
**Plans**: TBD

### Phase 44: State Core Stores
**Goal**: AppState, AuthState, UIState reactive stores
**Depends on**: Phase 30
**Research**: Unlikely (standard state patterns)
**Plans**: TBD

### Phase 45: State Chat Stores
**Goal**: ChatState, MessageState, PinnedState
**Depends on**: Phase 44, Phase 32
**Research**: Unlikely (standard state patterns)
**Plans**: TBD

### Phase 46: State User Stores
**Goal**: UserState, PresenceState, PrivacyState
**Depends on**: Phase 44, Phase 36
**Research**: Unlikely (standard state patterns)
**Plans**: TBD

### Phase 47: State Streaming & Offline
**Goal**: StreamingState, OfflineState for real-time and offline support
**Depends on**: Phase 44, Phase 21
**Research**: Unlikely (standard state patterns)
**Plans**: TBD

### Phase 48: Events Bus & Handlers
**Goal**: EventBus with domain and UI events
**Depends on**: Phase 44
**Research**: Unlikely (standard event patterns)
**Plans**: TBD

### Phase 49: System Keyring & Directories
**Goal**: Secret Service keyring, XDG directory handling
**Depends on**: Phase 5
**Research**: Unlikely (keyring/directories crates documented)
**Plans**: TBD

### Phase 50: System Tray & Notifications
**Goal**: StatusNotifierItem tray, desktop notifications
**Depends on**: Phase 49
**Research**: Unlikely (ksni/notify-rust documented)
**Plans**: TBD

### Phase 51: System Utilities
**Goal**: Clipboard, open URLs/files, autostart
**Depends on**: Phase 49
**Research**: Unlikely (standard crates)
**Plans**: TBD

### Phase 52: Media File Handling
**Goal**: Native file picker, MIME detection, thumbnail cache
**Depends on**: Phase 7
**Research**: Unlikely (rfd documented)
**Plans**: TBD

### Phase 53: Media Playback
**Goal**: Audio playback with rodio, video player integration
**Depends on**: Phase 52
**Research**: Unlikely (rodio documented)
**Plans**: TBD

### Phase 54: Media Recording
**Goal**: Voice message recorder, video recorder
**Depends on**: Phase 52
**Research**: Unlikely (cpal/rodio documented)
**Plans**: TBD

### Phase 55: Media Previews
**Goal**: PDF viewer, syntax-highlighted code preview
**Depends on**: Phase 52
**Research**: Unlikely (standard libraries)
**Plans**: TBD

### Phase 56: Media GIF & Embeds
**Goal**: GIF picker with Tenor/Giphy, YouTube/link embeds
**Depends on**: Phase 52
**Research**: Unlikely (HTTP API calls)
**Plans**: TBD

### Phase 57: UI Foundation
**Goal**: Slint setup, Rust-Slint bridge, theme (colors, typography)
**Depends on**: Phase 48
**Research**: Likely (Slint API)
**Research topics**: Slint component model, data binding, ModelRc, callbacks
**Plans**: TBD

### Phase 58: UI Basic Components
**Goal**: Avatar, button, input, modal, toast, tooltip, skeleton
**Depends on**: Phase 57
**Research**: Likely (Slint component patterns)
**Research topics**: Slint styling, custom components, animations
**Plans**: TBD

### Phase 59: UI Pickers
**Goal**: Virtualized emoji picker, GIF picker, mention autocomplete
**Depends on**: Phase 58
**Research**: Likely (Slint virtualization)
**Research topics**: Slint ListView, virtual scrolling, popup positioning
**Plans**: TBD

### Phase 60: UI Sidebar
**Goal**: Channel list, DM list, user profile, saved messages
**Depends on**: Phase 58, Phase 45
**Research**: Likely (Slint patterns)
**Research topics**: Slint list rendering, state binding
**Plans**: TBD

### Phase 61: UI Chat Core
**Goal**: Virtualized message list, message item, message input
**Depends on**: Phase 60
**Research**: Likely (Slint virtualization)
**Research topics**: Slint virtualized lists, scroll behavior, input handling
**Plans**: TBD

### Phase 62: UI Chat Features
**Goal**: Pinned bar, thread panel, reactions, typing indicator
**Depends on**: Phase 61
**Research**: Likely (Slint patterns)
**Research topics**: Slint animations, conditional rendering
**Plans**: TBD

### Phase 63: UI File Components
**Goal**: Attachment tray, file preview, code preview, PDF viewer
**Depends on**: Phase 61, Phase 55
**Research**: Likely (Slint integration)
**Research topics**: Slint image/canvas, external rendering
**Plans**: TBD

### Phase 64: UI Modals
**Goal**: Create channel, settings, profile, forward, invite modals
**Depends on**: Phase 58
**Research**: Likely (Slint patterns)
**Research topics**: Slint modal/popup patterns, form handling
**Plans**: TBD

### Phase 65: UI Settings Screens
**Goal**: Account, privacy, encryption, appearance settings
**Depends on**: Phase 64
**Research**: Likely (Slint patterns)
**Research topics**: Slint form controls, toggle/checkbox patterns
**Plans**: TBD

### Phase 66: UI Auth Screens
**Goal**: Login, register, loading screens
**Depends on**: Phase 57
**Research**: Likely (Slint patterns)
**Research topics**: Slint form validation, loading states
**Plans**: TBD

### Integration & Verification

### Phase 67: Main Binary Integration
**Goal**: Wire UI callbacks to services, create main.rs entry point, initialize all subsystems
**Depends on**: Phase 66
**Research**: Unlikely (wiring existing components)
**Plans**: TBD

### Phase 68: Build and Compile Verification
**Goal**: Ensure full cargo build succeeds, fix any compilation errors, resolve dependencies
**Depends on**: Phase 67
**Research**: Unlikely (debugging existing code)
**Plans**: TBD

### Phase 68.1: Signal Protocol Integration (INSERTED)
**Goal**: Integrate crypto primitives into MessageService and create SignalService
**Depends on**: Phase 14, Phase 32
**Research**: Unlikely (internal wiring)
**Plans**: TBD

### Phase 69: Proto Generation Pipeline
**Goal**: Set up buf generate for proto files, ensure proto types match server
**Depends on**: Phase 68
**Research**: Unlikely (buf tooling documented)
**Plans**: TBD

### Phase 70: End-to-End Smoke Test
**Goal**: Verify basic auth and messaging flow against running Chatter server
**Depends on**: Phase 69
**Research**: Unlikely (testing existing functionality)
**Plans**: TBD

### Phase 71: Complete UI Callback Wiring
**Goal**: Wire all remaining Slint UI callbacks to services (sidebar, chat, modals, settings)
**Depends on**: Phase 70
**Research**: Unlikely (connecting existing components)
**Status**: Done
**Plans**: 5/5 complete

**Details:**
All callbacks wired. Final plan (71-05) included major refactoring:
- Extracted ~2500 line main.rs into modular `src/bin_ui/` structure
- Created 8 feature modules: auth, navigation, sidebar, chat, chat_actions, header, composer, modals
- Follows idiomatic Rust patterns with `wire()` functions per feature

### Phase 72: Channel Creation, Search & Join UI
**Goal**: Complete channel discovery and creation flows with AI channel support
**Depends on**: Phase 71
**Research**: Unlikely (wiring existing components)
**Status**: Done
**Plans**: 1/1 complete

**Details:**
- Enhanced CreateChannelModal with 3-step wizard for AI channels
- Added 5th template (AI Assistant) to ChannelTemplates
- Created JoinChannelModal with search and direct ID input
- Wired `wire_create_channel()` and `wire_join_channel()` in modals.rs
- Debounced channel search with atomic counter
- Join button added to sidebar

### Phase 73: Signal Protocol Encryption Wiring
**Goal**: Connect existing crypto primitives to message flows for E2E encryption
**Depends on**: Phase 72
**Research**: Unlikely (wiring existing components)
**Status**: Done
**Plans**: 1/1

**Details:**
- DM encryption via Signal Protocol (sealed sender + mailbox) ✅
- Channel encryption via Sender Keys ✅
- SKDM distribution and rotation on member leave/kick ✅
- Message storage with deduplication ✅
- Sidebar updates for new DM messages and unread counts ✅
- MessageGateway: central decryption router for all paths ✅
- All Content types wired (typing, reactions, edit, delete, pin) ✅
  - Send: `_with_channel` variants embed channel_id in encrypted payload
  - Receive: `dispatch_channel_control_content` parses and emits proper events
  - Server never sees channel context for control messages

### Phase 74: LiveKit Video & Audio Calls
**Goal**: Integrate LiveKit Rust SDK for real-time video and audio calling
**Depends on**: Phase 73
**Research**: Likely (LiveKit SDK API, Slint video rendering)
**Research topics**: livekit crate API, video frame rendering to Slint, audio device integration
**Status**: Complete (2026-02-23)
**Plans**: 5/5 complete

**Key integration points:**
- LiveKit Rust SDK (`livekit = "0.7"`) for WebRTC
- Server-side: Go backend generates JWT access tokens
- Client: Room connection, track publishing/subscribing
- UI: Video frames → Slint Image component
- Audio: Integration with existing `media/audio_player.rs`
- Call state management and UI (incoming/outgoing/active call screens)

### Phase 75: Secure In-Memory Attachment Handling
**Goal**: Migrate from filesystem-based attachment storage to encrypted database storage with memory-only decryption
**Depends on**: Phase 74
**Research**: Unlikely (internal security hardening)
**Status**: In Progress
**Plans**: 0 plans

**Security principle:** Decrypted content exists ONLY in memory, wiped immediately after use.

**Key changes:**
- SEND: File → Memory → Encrypt → DB (BLOB) + CDN
- RECEIVE: CDN → DB (BLOB) → Memory (decrypt) → Player → Zeroize
- SecureBuffer with ZeroizeOnDrop for auto-wiping
- Linux memfd for video playback (fd: protocol)
- Remove all local_path filesystem usage
- Channel attachments use same encrypted CDN flow as DMs

**Details:**
[In progress - see .planning/refactor-ui-separation-of-concerns.md]

### Phase 76: Refactor Large Service Files
**Goal**: Split service files over 550 lines into focused modules for maintainability
**Depends on**: Phase 75
**Research**: Unlikely (internal refactoring)
**Status**: In Progress
**Plans**: 3/4 complete

**Target files (>550 lines):**
- `message_service.rs` (3143 lines) → dm/, group/, storage/ modules ✅ (76-01)
- `inbox_events.rs` (2425 lines) → by event type ✅ (76-02)
- `signal_service.rs` (1486 lines) → session/, sender_key/, identity/ modules ✅ (76-03)
- `attachment_service.rs` (1395 lines) → upload/, download/ modules
- `message_gateway.rs` (1365 lines) → routing/, decryption/ modules
- `channel/service.rs` (1302 lines) → crud/, membership/ modules
- Additional files 550-1300 lines as needed

**Details:**
76-03 complete: signal_service.rs split into signal/ module (session, sender_key, identity)

### Phase 77: Rename public_key to svr_public_key
**Goal**: Replace legacy public_key field with svr_public_key for Secure Value Recovery across full stack
**Depends on**: Phase 76
**Research**: Unlikely (field rename across proto/backend/client)
**Status**: Not started
**Plans**: 0 plans

**Scope (~25 files):**
- Proto: auth.proto, common.proto (field rename, keep field number 3)
- Backend Go: types, repository, service, handlers, Temporal workflows/activities
- Backend DB: New migration to rename column + index
- Client Rust: domain, storage migration, repository, services
- Mocks: Regenerate after interface changes

**Details:**
Prepares for PIN-based Secure Value Recovery (SVR) feature. The legacy `public_key` field is redundant with Signal Protocol's `prekey_bundle.identity_key` and will be repurposed for SVR.

### Phase 78: Service Separation of Concerns
**Goal**: Enforce proper layering by moving business logic from UI/handlers into services, centralizing cross-cutting concerns
**Depends on**: Phase 77
**Research**: Unlikely (internal refactoring based on codebase analysis)
**Status**: In Progress
**Plans**: 2/5 complete

**Scope (~20+ files):**

Priority 1 (Critical - Security/Stability):
- Create `DmBootstrapService` - Consolidate bootstrap logic from `bin_ui/features/auth/inbox/dm_handlers.rs` and `inbox/handlers/`
- Create `SessionManagementService` - Centralize session reset/recovery/archive from `message/dm.rs` and UI handlers
- Remove direct repository access from UI - All `*_repo.insert()` calls in `bin_ui/features/` should go through services

Priority 2 (High - Architecture):
- Create `ChannelEventService` - System message creation (member join/leave) belongs here, not in `channel_events.rs`
- Create `SignalStateService` - Abstract KeyStore/Sesame access from UI layer (`login.rs`, `register.rs`)
- Create `CurrentUserProvider` trait - Eliminate 15+ duplicate `current_user_id()` implementations

Priority 3 (Medium - Cleanup):
- Consolidate offline queue - Keep implementation in services, remove from `network/offline_queue.rs`
- Move validation to services - Network layer should only deserialize, not validate
- Create `MessageBuilderService` - Consolidate content builders from `gateway/builders.rs`

Priority 4 (Low - Refactoring):
- Create `ConverterService` - Proto ↔ Domain conversions in one place
- Move storage initialization to services - Reduce UI coupling to database

**Key violations to fix:**
- `bin_ui/features/auth/channel_events.rs:172,270` - Direct `message_repo.insert()`, `channel_repo.update()`
- `bin_ui/features/auth/inbox/dm_handlers.rs:1271,1280` - Direct `channel_repo.insert()`
- `bin_ui/features/auth/inbox/dm_handlers.rs:36-75` - `handle_convergence_failure()` duplicates service logic
- `bin_ui/features/auth/login.rs` - Direct `ctx.key_store` access
- `services/message/dm.rs:401-417` - `sesame_manager` accessed directly instead of through service

**Details:**
Based on comprehensive separation of concerns analysis identifying layer violations between UI, handlers, services, and storage.

### Phase 79: Registration Privacy Hardening
**Goal**: Fix critical privacy leaks in registration and login flows based on Signal Protocol audit
**Depends on**: Phase 78
**Research**: Complete (see 79-DISCOVERY.md - Signal-Desktop and Signal-Server comparison)
**Status**: Done
**Plans**: 2/2 complete

**Fixes implemented:**
- Stop sending profile_key to server (only send derived UAK)
- Encrypt display_name with profile key before sending

**Not implemented (redundant):**
- Friend request message encryption - Server necessarily knows sender→recipient relationship from RPC metadata, so encrypting message content provides negligible privacy benefit

**Scope (~10 files):**
- `src/services/auth_service.rs` - Registration/login flow
- `src/network/grpc/auth_client.rs` - RegisterRequest construction
- `src/crypto/profile_crypto.rs` - New module for profile encryption
- `src/services/user/mod.rs` - Friend request encryption
- `src/services/friend_request_crypto.rs` - New friend request crypto service
- `src/storage/repositories/settings_repo.rs` - Store profile_key locally

**Deferred (requires server changes):**
- Username hashing (server stores plaintext)
- Social graph privacy (server sees friend/block lists)
- ACI/PNI dual identity model (server architecture)
- Phone number discoverability toggle (server feature)

**Reference:**
- Signal-Desktop: `/home/alex/Documents/Signal-Desktop/ts/textsecure/AccountManager.preload.ts`
- Signal-Server: `/home/alex/Documents/Signal-server/service/src/main/java/org/whispersystems/textsecuregcm/controllers/RegistrationController.java`

### Phase 80: Local AI Integration
**Goal:** Add local LLM inference using mistral.rs with actor pattern (like SignalActorHandle) for GPU-accelerated AI features
**Depends on:** Phase 42 (Services AI Integration)
**Research:** Complete (benchmarked mistral.rs v0.7 with CUDA + FlashAttention on RTX 4070 Ti Super: 72.7 t/s decode for 7B Q4)
**Status:** Not started
**Plans:** 0 plans

**Architecture:**
- Actor pattern isolates GPU inference in dedicated thread (same as crypto actor)
- `LocalAIActorHandle` with command enum for load/complete/stream operations
- Optional feature flag `local-ai` to avoid mandatory CUDA dependency
- Support for GGUF quantized models (Q4_K_M optimal for 16GB VRAM)

**Benchmarks (RTX 4070 Ti Super):**
| Model | Format | Decode | TTFT |
|-------|--------|--------|------|
| Qwen2.5-3B | BF16 | 62.5 t/s | 27ms |
| Qwen2.5-7B | Q4_K_M GGUF | 72.7 t/s | 71ms |

**Scope (~8 files):**
- `Cargo.toml` - Add mistralrs dependency with cuda/flash-attn features
- `src/services/ai/mod.rs` - LocalAIService orchestrator
- `src/services/ai/local_actor.rs` - LocalAIActorHandle (actor pattern)
- `src/services/ai/provider.rs` - AIProvider trait for local/remote abstraction
- `src/services/ai/models.rs` - Model config, sample params
- `src/app/services.rs` - Wire LocalAIActorHandle into ServiceContainer
- `src/bin_ui/features/modals/ai_settings.rs` - Model selection UI
- `src/storage/repositories/ai_repo.rs` - Store model preferences

**Details:**
Local AI enables offline summarization, smart replies, and content analysis without cloud dependency. Uses same actor pattern as SignalActorHandle for thread isolation.

## Progress

**Execution Order:** Phases execute in numeric order (1 → 2 → ... → 70)

| Phase | Name | Plans | Status | Completed |
|-------|------|-------|--------|-----------|
| 1 | Scaffolding | 1 | Done | 2026-02-03 |
| 2 | Domain Core Entities | 2 | Done | 2026-02-03 |
| 3 | Domain Supporting Entities | 2 | Done | 2026-02-03 |
| 4 | Domain Privacy Entities | 2 | Done | 2026-02-03 |
| 5 | Storage Database Setup | 2 | Done | 2026-02-03 |
| 6 | Storage Core Repositories | 3 | Done | 2026-02-03 |
| 7 | Storage Supporting Repositories | 4 | Done | 2026-02-03 |
| 8 | Storage Search Index | 1 | Done | 2026-02-03 |
| 9 | Storage Key Store | 1 | Done | 2026-02-03 |
| 10 | Crypto Signal Store | 1 | Done | 2026-02-03 |
| 11 | Crypto Identity & Sessions | 1 | Done | 2026-02-03 |
| 12 | Crypto Prekeys | 1 | Done | 2026-02-03 |
| 13 | Crypto Sender Keys | 1 | Done | 2026-02-03 |
| 14 | Crypto Sealed Sender | 1 | Done | 2026-02-03 |
| 15 | Crypto Attachments | 1 | Done | 2026-02-03 |
| 16 | Network gRPC Foundation | 2 | Done | 2026-02-03 |
| 17 | Network Auth & User Clients | 3 | Done | 2026-02-03 |
| 18 | Network Channel & Message Clients | 2 | Done | 2026-02-03 |
| 19 | Network Supporting Clients | 4 | Done | 2026-02-03 |
| 20 | Network Streaming | 4 | Done | 2026-02-03 |
| 21 | Network Resilience | 3 | Done | 2026-02-03 |
| 22 | Network Validation | 2 | Done | 2026-02-03 |
| 23 | Inbox Provider & Poller | 2 | Done | 2026-02-03 |
| 24 | Inbox Message Handlers | 3 | Done | 2026-02-03 |
| 25 | Inbox Sync Handlers | 2 | Done | 2026-02-04 |
| 26 | Mailbox Token Manager | 1 | Done | 2026-02-04 |
| 27 | Mailbox DM System | 3 | Done | 2026-02-04 |
| 28 | Mailbox Group System | 3 | Done | 2026-02-04 |
| 29 | Mailbox Privacy Features | 3 | Done | 2026-02-04 |
| 30 | Services Auth | 2 | Done | 2026-02-04 |
| 31 | Services Account | 3 | Done | 2026-02-05 |
| 32 | Services Message Core | 2 | Done | 2026-02-05 |
| 33 | Services Message Advanced | 2 | Done | 2026-02-05 |
| 34 | Services Ephemeral Messages | 2 | Done | 2026-02-05 |
| 35 | Services Channel | 3 | Done | 2026-02-05 |
| 36 | Services User | 3 | Done | 2026-02-05 |
| 37 | Services Presence | 1 | Done | 2026-02-05 |
| 38 | Services Real-time | 1 | Done | 2026-02-05 |
| 39 | Services Reactions | 1 | Done | 2026-02-05 |
| 40 | Services Search | 1 | Done | 2026-02-05 |
| 41 | Services Moderation | 1 | Done | 2026-02-05 |
| 42 | Services AI Integration | 1 | Done | 2026-02-05 |
| 43 | Services Saved Messages | 1 | Done | 2026-02-05 |
| 44 | State Core Stores | 1 | Done | 2026-02-05 |
| 45 | State Chat Stores | 1 | Done | 2026-02-05 |
| 46 | State User Stores | 1 | Done | 2026-02-05 |
| 47 | State Streaming & Offline | 1 | Done | 2026-02-05 |
| 48 | Events Bus & Handlers | 1 | Done | 2026-02-05 |
| 49 | System Keyring & Directories | 1 | Done | 2026-02-05 |
| 50 | System Tray & Notifications | 1 | Done | 2026-02-05 |
| 51 | System Utilities | 1 | Done | 2026-02-05 |
| 52 | Media File Handling | 1 | Done | 2026-02-05 |
| 53 | Media Playback | 1 | Done | 2026-02-05 |
| 54 | Media Recording | 1 | Done | 2026-02-05 |
| 55 | Media Previews | 1 | Done | 2026-02-05 |
| 56 | Media GIF & Embeds | 2 | Done | 2026-02-05 |
| 57 | UI Foundation | 1 | Done | 2026-02-05 |
| 58 | UI Basic Components | 2 | Done | 2026-02-05 |
| 59 | UI Pickers | 2 | Done | 2026-02-05 |
| 60 | UI Sidebar | 2 | Done | 2026-02-05 |
| 61 | UI Chat Core | 2 | Done | 2026-02-05 |
| 62 | UI Chat Features | 2 | Done | 2026-02-05 |
| 63 | UI File Components | 2 | Done | 2026-02-05 |
| 64 | UI Modals | 3 | Done | 2026-02-05 |
| 65 | UI Settings Screens | 2 | Done | 2026-02-05 |
| 66 | UI Auth Screens | 1 | Done | 2026-02-05 |
| 67 | Main Binary Integration | 3 | Done | 2026-02-06 |
| 68 | Build and Compile Verification | 1 | Done | 2026-02-06 |
| 68.1 | Signal Protocol Integration | 1 | Done | 2026-02-10 |
| 69 | Proto Generation Pipeline | 1 | Done | 2026-02-08 |
| 70 | End-to-End Smoke Test | 1 | Done | 2026-02-10 |
| 71 | Complete UI Callback Wiring | 5 | Done | 2026-02-08 |
| 72 | Channel Creation, Search & Join UI | 1 | Done | 2026-02-10 |
| 73 | Signal Protocol Encryption Wiring | 1 | Done | 2026-02-10 |
| 74 | LiveKit Video & Audio Calls | 5 | Done | 2026-02-23 |
| 75 | Secure In-Memory Attachment Handling | 0 | In Progress | - |
| 76 | Refactor Large Service Files | 3/4 | In Progress | - |
| 77 | Rename public_key to svr_public_key | 0 | Not started | - |
| 78 | Service Separation of Concerns | 2/5 | In Progress | - |
| 79 | Registration Privacy Hardening | 2 | Done | 2026-03-05 |
| 80 | Local AI Integration | 0 | Not started | - |

---

## 🚧 Milestone: Jarvis AI Assistant (Phases 81-90)

**Milestone Goal:** Transform Jarvis from basic Q&A into an intelligent, proactive AI assistant with memory, relationship intelligence, commitment tracking, and cross-conversation insights.

**Foundation (Already Complete):**
- JarvisService with `ask()`, `observe_message()`, `summarize_conversation()` ✅
- JarvisMemoryRepository (conversations, facts, summaries) ✅
- JarvisGraphRepository (entities, relationships, recursive CTE traversal) ✅
- EntityExtractor (LLM-based entity extraction) ✅
- ContextBuilder (RAG-style context retrieval) ✅
- MemoryManager (compression, decay) ✅
- JarvisBackground (event bus observer, maintenance) ✅

### Jarvis Phases

- [x] **Phase 81: Jarvis UI Integration** - @Jarvis mentions, slash commands, Jarvis panel
- [ ] **Phase 82: Proactive Intelligence** - Notification system for insights, reminders, connections
- [ ] **Phase 83: Commitment Tracking** - Extract promises/action items, deadlines, follow-up nudges
- [ ] **Phase 84: Relationship Intelligence** - Contact profiles, communication patterns, open threads
- [ ] **Phase 85: Smart Compose** - Context-aware drafting, quick replies, conversation continuity
- [ ] **Phase 86: Semantic Search** - Natural language search across all conversations
- [ ] **Phase 87: Cross-Conversation Insights** - Connect dots across channels, pattern detection
- [ ] **Phase 88: Knowledge Graph Visualization** - Interactive entity/relationship graph UI
- [ ] **Phase 89: Communication Analytics** - Personal insights dashboard (local only)
- [ ] **Phase 90: Jarvis Automation** - User-defined triggers, scheduled messages, workflows

## Phase Details: Jarvis Milestone

### Phase 81: Jarvis UI Integration
**Goal**: Make Jarvis accessible from the UI via @mentions, slash commands, and dedicated panel
**Depends on**: Phase 80 (Local AI), existing Jarvis foundation
**Research**: Unlikely (internal UI wiring)
**Plans**: 3 plans

**Scope:**
- `@Jarvis` mention detection in message composer → route to JarvisService
- Slash commands: `/ask`, `/summarize`, `/knowledge`, `/remember`
- Jarvis panel in sidebar (conversation with Jarvis)
- "Jarvis is thinking..." typing indicator during inference
- Local-only "Jarvis" messages (not sent to server)
- JarvisState for UI state management

**Files (~12):**
- `src/bin_ui/features/jarvis/mod.rs` - Module
- `src/bin_ui/features/jarvis/mention_handler.rs` - @Jarvis detection
- `src/bin_ui/features/jarvis/slash_commands.rs` - Command parsing
- `src/bin_ui/features/jarvis/panel.rs` - Sidebar panel
- `src/state/jarvis_state.rs` - UI state
- `ui/jarvis/jarvis_panel.slint` - Panel UI
- `ui/jarvis/jarvis_message.slint` - Message bubble
- Modify: `src/bin_ui/features/chat/send_message.rs` - Intercept @Jarvis
- Modify: `ui/sidebar/sidebar.slint` - Add Jarvis entry

### Phase 82: Proactive Intelligence
**Goal**: Jarvis reaches out with insights, reminders, and connection suggestions
**Depends on**: Phase 81
**Research**: Likely (notification patterns, insight generation)
**Research topics**: Notification UX, insight relevance scoring, quiet hours
**Plans**: 3 plans

**Scope:**
- `JarvisInsightEngine` - Background analysis of messages for insights
- Insight types: topic connections, deadline reminders, relationship gaps, unresolved threads
- `JarvisNotification` - Dismissible notification cards
- Relevance scoring to avoid noise
- Quiet hours and notification settings
- "Jarvis noticed..." toast notifications

**Files (~10):**
- `src/services/jarvis/insight_engine.rs` - Insight generation
- `src/services/jarvis/insight_types.rs` - InsightType enum
- `src/services/jarvis/notification_manager.rs` - Notification queue
- `src/domain/jarvis_notification.rs` - Notification types
- `ui/jarvis/insight_card.slint` - Notification UI
- `ui/jarvis/insight_toast.slint` - Toast notification
- Modify: `src/services/jarvis/background.rs` - Add insight generation

### Phase 83: Commitment Tracking
**Goal**: Extract promises, action items, and deadlines from conversations
**Depends on**: Phase 82
**Research**: Likely (NLP for commitment extraction)
**Research topics**: LLM prompts for action item extraction, deadline parsing
**Plans**: 2 plans

**Scope:**
- `CommitmentExtractor` - LLM-based extraction of promises/action items
- Commitment types: promised_to_do, waiting_for, decision_made
- Deadline extraction and tracking
- Commitment dashboard UI
- "Nudge" suggestions for overdue items
- Integration with proactive notifications

**Files (~8):**
- `src/services/jarvis/commitment_extractor.rs` - Extraction logic
- `src/domain/jarvis_commitment.rs` - Commitment types
- `src/storage/repositories/jarvis_commitment_repo.rs` - Storage
- `src/storage/migrations/v033_jarvis_commitments.rs` - Migration
- `ui/jarvis/commitment_dashboard.slint` - Dashboard UI
- `ui/jarvis/commitment_item.slint` - Item UI
- Modify: `src/services/jarvis/service.rs` - Add commitment tracking

### Phase 84: Relationship Intelligence
**Goal**: Build rich profiles of contacts with communication patterns and context
**Depends on**: Phase 83
**Research**: Likely (relationship modeling)
**Research topics**: Contact profile structure, communication pattern analysis
**Plans**: 3 plans

**Scope:**
- `RelationshipAnalyzer` - Analyze communication patterns per contact
- Contact profile: last talked, response time, topics, open threads, expertise
- "Briefing" before calls (context about the person)
- Communication health indicators
- "Best time to reach" suggestions
- Relationship graph queries

**Files (~10):**
- `src/services/jarvis/relationship_analyzer.rs` - Analysis logic
- `src/domain/jarvis_contact_profile.rs` - Profile types
- `src/storage/repositories/jarvis_profile_repo.rs` - Storage
- `src/storage/migrations/v034_jarvis_profiles.rs` - Migration
- `ui/jarvis/contact_profile.slint` - Profile panel
- `ui/jarvis/call_briefing.slint` - Pre-call briefing
- Modify: `ui/calls/incoming_call_modal.slint` - Add briefing

### Phase 85: Smart Compose
**Goal**: AI-assisted message drafting with context awareness
**Depends on**: Phase 84
**Research**: Likely (contextual suggestion generation)
**Research topics**: Draft suggestion UX, completion triggers
**Plans**: 2 plans

**Scope:**
- `SmartComposer` - Generate contextual message suggestions
- Suggestion types: continue thread, follow up, respond to question
- Quick reply chips (1-tap responses)
- Draft enhancement (improve what user typed)
- Tone adjustment (formal, casual, etc.)
- Context from relationship profile

**Files (~8):**
- `src/services/jarvis/smart_composer.rs` - Suggestion generation
- `src/domain/jarvis_suggestion.rs` - Suggestion types
- `ui/jarvis/suggestion_bar.slint` - Suggestion chips
- `ui/jarvis/quick_replies.slint` - Quick reply UI
- Modify: `ui/chat/message_composer.slint` - Add suggestion bar
- Modify: `src/bin_ui/features/composer/mod.rs` - Wire suggestions

### Phase 86: Semantic Search
**Goal**: Natural language search across all conversations
**Depends on**: Phase 85
**Research**: Likely (semantic search implementation)
**Research topics**: Embedding models, vector search in SQLite, query expansion
**Plans**: 2 plans

**Scope:**
- `SemanticSearchService` - Natural language to search results
- Embedding generation for messages (using LocalAI)
- Vector similarity search (cosine distance in SQLite)
- Query expansion and rewriting
- Search result ranking (recency, relevance, relationship)
- "Find that discussion about..." UI

**Files (~8):**
- `src/services/jarvis/semantic_search.rs` - Search logic
- `src/services/jarvis/embedding_manager.rs` - Embedding generation
- `src/storage/repositories/jarvis_embedding_repo.rs` - Vector storage
- `src/storage/migrations/v035_jarvis_embeddings.rs` - Migration
- `ui/jarvis/semantic_search.slint` - Search UI
- `ui/jarvis/search_result.slint` - Result display
- Modify: `src/bin_ui/features/header.rs` - Wire search

### Phase 87: Cross-Conversation Insights
**Goal**: Connect information across different channels and DMs
**Depends on**: Phase 86
**Research**: Likely (cross-context correlation)
**Research topics**: Topic clustering, information flow detection
**Plans**: 2 plans

**Scope:**
- `CrossConversationAnalyzer` - Find connections across contexts
- Topic threading across channels
- "Related discussion in #channel" suggestions
- Information flow visualization
- Duplicate question detection
- "Others are discussing this" alerts

**Files (~6):**
- `src/services/jarvis/cross_analyzer.rs` - Analysis logic
- `src/domain/jarvis_connection.rs` - Connection types
- `ui/jarvis/related_discussions.slint` - Related panel
- `ui/jarvis/cross_insight.slint` - Insight card
- Modify: `src/services/jarvis/insight_engine.rs` - Add cross insights

### Phase 88: Knowledge Graph Visualization
**Goal**: Interactive visualization of entities and relationships
**Depends on**: Phase 87
**Research**: Likely (graph visualization in Slint)
**Research topics**: Force-directed layout, Slint canvas rendering
**Plans**: 2 plans

**Scope:**
- `GraphLayoutEngine` - Force-directed graph layout
- Interactive graph canvas in Slint
- Entity nodes with type colors
- Relationship edges with labels
- Zoom, pan, select interactions
- Entity detail panel on click
- Filter by entity type

**Files (~8):**
- `src/services/jarvis/graph_layout.rs` - Layout algorithm
- `src/bin_ui/features/jarvis/graph_view.rs` - Graph rendering
- `ui/jarvis/graph_canvas.slint` - Canvas component
- `ui/jarvis/graph_node.slint` - Node rendering
- `ui/jarvis/entity_detail.slint` - Detail panel
- Modify: `ui/jarvis/jarvis_panel.slint` - Add graph tab

### Phase 89: Communication Analytics
**Goal**: Personal insights dashboard for communication patterns
**Depends on**: Phase 88
**Research**: Unlikely (data aggregation)
**Plans**: 2 plans

**Scope:**
- `AnalyticsService` - Aggregate communication data
- Metrics: messages sent, response times, active hours, top collaborators
- Topic trends (rising/falling)
- Communication health indicators
- All data local-only (never sent to server)
- Export to JSON

**Files (~8):**
- `src/services/jarvis/analytics_service.rs` - Aggregation logic
- `src/domain/jarvis_analytics.rs` - Analytics types
- `ui/jarvis/analytics_dashboard.slint` - Dashboard UI
- `ui/jarvis/chart_component.slint` - Simple charts
- `ui/jarvis/trend_indicator.slint` - Trend badges
- Modify: `ui/modals/settings.slint` - Add analytics section

### Phase 90: Jarvis Automation
**Goal**: User-defined triggers and automated workflows
**Depends on**: Phase 89
**Research**: Likely (rule engine design)
**Research topics**: Trigger DSL, action types, rate limiting
**Plans**: 3 plans

**Scope:**
- `JarvisAutomation` - Rule engine for triggers
- Trigger types: keyword, person mention, time-based, pattern
- Action types: notify, auto-reply, generate briefing, log
- Rule builder UI
- Scheduled messages
- "When X happens, do Y" configuration

**Files (~10):**
- `src/services/jarvis/automation.rs` - Automation engine
- `src/services/jarvis/trigger_parser.rs` - Trigger DSL
- `src/domain/jarvis_rule.rs` - Rule types
- `src/storage/repositories/jarvis_rule_repo.rs` - Storage
- `src/storage/migrations/v036_jarvis_rules.rs` - Migration
- `ui/jarvis/rule_builder.slint` - Rule UI
- `ui/jarvis/trigger_selector.slint` - Trigger picker
- `ui/jarvis/action_selector.slint` - Action picker

### Phase 91: Multi-Device Support
**Goal**: Enable messages to reach all linked devices (phones, tablets, secondary desktops) per Signal Protocol spec
**Depends on**: Phase 90
**Research**: Unlikely (infrastructure exists, needs wiring)
**Status**: Not started
**Plans**: 2 plans

Plans:
- [ ] 91-01: Multi-Device Encryption for Groups and DMs (group.rs, core.rs, dm_bootstrap_service.rs)
- [ ] 91-02: SentTranscript Sync for Saved Messages (proto, saved.rs, sync handler)

**Current State:**
- ✅ `encrypt_message_all_devices()` implemented in `signal/session.rs:374` and `pairwise.rs:161`
- ✅ DM mailbox flow uses multi-device (`dm.rs:115`)
- ❌ Group messages hardcode `device_id = 1` (`group.rs:808`)
- ❌ Saved messages don't sync to other devices (`saved.rs:78-97`)
- ❌ Channel DMs hardcode `device_id = 1` (`core.rs:622`)
- ❌ Bootstrap messages hardcode `device_id = 1` (`dm_bootstrap.rs:251,322`)

**Scope (~15 files):**

Priority 1 (Groups - most visible):
- `src/services/message/group.rs:794-827` - Replace single encrypt with `encrypt_message_all_devices()` per member
- Need to handle partial failures (some devices succeed, some fail)

Priority 2 (Channel DMs):
- `src/services/message/core.rs:614-642` - Use `encrypt_message_all_devices()` for channel-based DMs
- Server may handle fanout, verify behavior

Priority 3 (Saved Messages Sync):
- `src/services/message/saved.rs:68-98` - Implement SentTranscript sync
- Create `SyncMessage` proto with SentTranscript variant
- Encrypt for each linked device using Signal Protocol
- Requires server support for device-specific queues

Priority 4 (Bootstrap):
- `src/services/message/dm_bootstrap.rs:251,322` - Multi-device bootstrap
- `src/inbox/streaming/dm_bootstrap_handler.rs:353,731` - Handle bootstrap for all devices

**Key changes:**
```rust
// Before (group.rs:808):
.encrypt_message(auth_client, &member_id.to_string(), 1, plaintext)

// After:
.encrypt_message_all_devices(auth_client, &member_id.to_string(), plaintext)
```

**Server requirements:**
- Device list API (already exists: `list_devices()`)
- Per-device message queues (verify exists)
- SentTranscript handling for saved messages (may need server work)

**Details:**
Multi-device is critical for users with phones + desktops. Current implementation only sends to primary device (device_id=1), causing messages to appear missing on secondary devices.

### Phase 92: Call Orchestration to Core
**Goal**: Move call state machine, key exchange orchestration, and call history from UI to core call services
**Depends on**: Phase 91
**Research**: Unlikely (internal refactoring, crypto already in core)
**Status**: Done
**Plans**: 2/2 complete

Plans:
- [x] 92-01: DM call orchestration (key exchange, signaling, pending state → core)
- [x] 92-02: Anon call orchestration (tokens, room joins, call history → core)

**Scope (~8 files):**

Move from UI (`chatter-ui/src/bin_ui/features/calls/`) to core (`chatter-core/src/services/`):
- `PendingOutgoingCall` state (key_exchange.rs thread_local) → CallWorkerHandle
- Ephemeral key generation + offer sending (outgoing.rs) → `CallWorkerHandle::start_dm_call()`
- Accept flow: ephemeral key gen + actor call + answer sending (incoming.rs) → `CallWorkerHandle::accept_dm_call()`
- `PENDING_ANON_CALLS` HashMap + anon call state machine (anon_calls.rs) → CallWorkerHandle
- Token acquisition for anon calls (outgoing.rs) → CallSignalingService
- Call history storage for missed/declined (anon_calls.rs) → CallWorkerHandle
- Group key generation + distribution trigger (outgoing.rs) → `CallWorkerHandle::start_group_call()`

**Does NOT move (stays in UI):**
- Slint property setting (ActiveCallData, verification emojis display)
- Call timer start/stop
- Video frame timer
- Modal show/hide

**Key insight:** All crypto (ECDH, HKDF, ephemeral keys, zeroization) is already in core (`crypto/call_key_provider.rs`, `signal/actor.rs`). Only the orchestration (pending state + actor calls + LiveKit updates) needs to move.

**Details:**
Based on architecture review identifying call orchestration as the biggest protocol/crypto leak from UI to core. The existing `CallWorkerHandle` and `CallSignalingService` are the right homes — no new service needed.

### Phase 93: SVR Encrypted Key Backup
**Goal**: Implement full Secure Value Recovery pipeline — encrypt identity/profile keys with PIN-derived SVR key, store on server, restore on new device
**Depends on**: Phase 92
**Research**: Likely (Signal SVR2/SVR3 design, rate-limiting enclave patterns)
**Status**: In progress
**Plans**: 5/5 complete
**Status**: Complete

Plans:
- [x] 93-01: Server proto + migration + types (svr.proto, 000028 migration, Go domain types)
- [x] 93-02: Server SVR backup service + handler (repository, rate-limited service, ConnectRPC handler)
- [x] 93-03: Client backup crypto TDD (serialize/encrypt/decrypt round-trip with AES-256-GCM)
- [x] 93-04: Client SvrClient + SvrBackupService (gRPC client, service orchestration, ChatterClient facade)
- [x] 93-05: Client auth integration (backup after registration, restore on new device login)
- [x] 93-03: Client backup crypto TDD (serialize/encrypt/decrypt round-trip with AES-256-GCM)
- [ ] 93-04: Client SvrClient + SvrBackupService (gRPC client, service orchestration, ChatterClient facade)
- [ ] 93-05: Client auth integration (backup after registration, restore on new device login)

**Scope (server + client):**

Server (Go):
- New `svr.proto` with `StoreBackup` / `FetchBackup` / `DeleteBackup` RPCs
- `svr_backups` Postgres table (user_id, encrypted_blob, version, created_at)
- SVR service with PIN-attempt rate limiting (max 10 attempts, exponential backoff, lockout)
- Backup versioning (overwrite on PIN change, single active backup per user)

Client (Rust):
- Backup creation: serialize identity keypair + profile key + signed prekey secrets → encrypt with AES-256-GCM using SVR secret
- Backup upload via new `SvrClient` gRPC client
- Restore flow: PIN entry → derive SVR secret → fetch encrypted blob → decrypt → hydrate local stores
- Integration into auth_service login (detect "new device" → prompt for PIN restore)
- `SvrKeypair::secret_bytes()` finally used for decrypt path

**Key design decisions:**
- AES-256-GCM encryption (not X25519 DH — SVR secret is symmetric, not a DH exchange)
- Server stores opaque encrypted blob (zero-knowledge — cannot read keys)
- Rate limiting replaces secure enclave (pragmatic alternative to SGX/nitro)
- Backup re-encrypted on every PIN change (derive new SVR secret, re-encrypt, re-upload)

**Details:**
Completes the SVR feature started in Phase 77 (rename) and the crypto foundation in `crypto/svr.rs`. The SVR keypair generation and registration lock are fully wired but the actual backup/restore pipeline is missing. This phase adds the server storage, client encryption, and restore flow.

---

### Phase 94: PIN Change Flow
**Goal**: Implement ChangePinBackup RPC end-to-end — server handler, client method, re-encrypt backup with new PIN, update registration lock atomically
**Depends on**: Phase 93
**Research**: No
**Status**: Complete
**Plans**: 2/2 complete

Plans:
- [x] 94-01: Server ChangePinBackup handler + service (atomic transaction, verify old hash, update registration lock + SVR public key)
- [x] 94-02: Client change_pin method (proto types, gRPC client, service, ChatterClient::change_pin)

**Scope:**
- Server: Implement ChangePinBackup handler that verifies old_backup_id_hash matches, atomically replaces backup + registration lock + SVR public key
- Client: `ChatterClient::change_pin(old_pin, new_pin, username)` method
- Proto already defined in svr.proto (ChangePinBackupRequest/Response added in Phase 93)
- Rate-limit ChangePinBackup (requires successful old backup verification)

---

### Phase 95: Account Recovery Codes
**Goal**: Generate one-time recovery codes at registration that allow account access when both password and PIN are forgotten
**Depends on**: Phase 94
**Research**: Likely (recovery code UX patterns, secure storage of hashed codes)
**Status**: Not started
**Plans**: 0/3

Plans:
- [ ] 95-01: Server proto + migration (recovery_codes table, UseRecoveryCode RPC)
- [ ] 95-02: Server service (hash codes with Argon2id, verify + invalidate on use, generate during registration)
- [ ] 95-03: Client integration (display codes at registration, use code to reset password + PIN)

**Scope:**
- Generate 8 recovery codes (8 alphanumeric chars each) during registration
- Store Argon2id hashes on server (zero-knowledge — server never sees plaintext codes)
- Each code is one-time use (invalidated after use)
- UseRecoveryCode RPC: verifies code hash, resets password, allows new PIN setup
- Client displays codes once at registration (user must save them)
- Rate-limited: 3 attempts per hour

**By-design limitations (not fixable):**
- PIN recovery without recovery codes is impossible (zero-knowledge model)
- Old group messages lost on device change (sender keys are ephemeral, Signal has same limitation)

---

## Jarvis Progress

| Phase | Name | Plans | Status | Completed |
|-------|------|-------|--------|-----------|
| 81 | Jarvis UI Integration | 3 | Done | 2026-03-07 |
| 82 | Proactive Intelligence | 3 | Not started | - |
| 83 | Commitment Tracking | 2 | Not started | - |
| 84 | Relationship Intelligence | 3 | Not started | - |
| 85 | Smart Compose | 2 | Not started | - |
| 86 | Semantic Search | 2 | Not started | - |
| 87 | Cross-Conversation Insights | 2 | Not started | - |
| 88 | Knowledge Graph Visualization | 2 | Not started | - |
| 89 | Communication Analytics | 2 | Not started | - |
| 90 | Jarvis Automation | 3 | Not started | - |
| 91 | Multi-Device Support | 0 | Not started | - |
| 92 | Call Orchestration to Core | 2 | Done | 2026-03-19 |
| 93 | SVR Encrypted Key Backup | 5 | Not started | - |
