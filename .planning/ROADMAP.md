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
- [ ] **Phase 7: Storage Supporting Repositories** - AttachmentRepo, ReactionRepo, DraftRepo, SettingsRepo
- [ ] **Phase 8: Storage Search Index** - FTS5 full-text search
- [ ] **Phase 9: Storage Key Store** - Signal protocol key storage

### Crypto Layer
- [ ] **Phase 10: Crypto Signal Store** - SignalProtocolStore trait implementation
- [ ] **Phase 11: Crypto Identity & Sessions** - Identity keys, session management
- [ ] **Phase 12: Crypto Prekeys** - PreKey bundles, signed prekeys, OTPKs
- [ ] **Phase 13: Crypto Sender Keys** - Group sender key distribution
- [ ] **Phase 14: Crypto Sealed Sender** - Anonymous messaging, certificates
- [ ] **Phase 15: Crypto Attachments** - File encryption/decryption

### Network Layer
- [ ] **Phase 16: Network gRPC Foundation** - Client pool, interceptors, connection
- [ ] **Phase 17: Network Auth & User Clients** - AuthClient, UserClient
- [ ] **Phase 18: Network Channel & Message Clients** - ChannelClient, MessageClient
- [ ] **Phase 19: Network Supporting Clients** - FileClient, NotificationClient, IntegrationClient, MailboxClient
- [ ] **Phase 20: Network Streaming** - StreamManager, real-time streams
- [ ] **Phase 21: Network Resilience** - Connection recovery, retry, offline queue
- [ ] **Phase 22: Network Validation** - Orderer, deduplicator, validator pipeline

### Inbox & Mailbox
- [ ] **Phase 23: Inbox Provider & Poller** - DeviceInboxProvider, polling loop
- [ ] **Phase 24: Inbox Message Handlers** - Chat, attachment, reaction, receipt, typing
- [ ] **Phase 25: Inbox Sync Handlers** - Key exchange, sender key, pin, sync
- [ ] **Phase 26: Mailbox Token Manager** - Blind signature tokens, daily quota
- [ ] **Phase 27: Mailbox DM System** - Bootstrap, conversation keys, two-way anonymity
- [ ] **Phase 28: Mailbox Group System** - Member mailboxes, N-way fanout
- [ ] **Phase 29: Mailbox Privacy Features** - Cover traffic, key rolling

### Services Layer
- [ ] **Phase 30: Services Auth** - Login, logout, device auth
- [ ] **Phase 31: Services Account** - Password change, deletion, session management
- [ ] **Phase 32: Services Message Core** - Send, receive, edit, delete
- [ ] **Phase 33: Services Message Advanced** - Forward, quote, pin, thread, draft
- [ ] **Phase 34: Services Ephemeral Messages** - TTL support, countdown timers
- [ ] **Phase 35: Services Channel** - Create, join, leave, archive, templates
- [ ] **Phase 36: Services User** - Profile, status, friends, blocking
- [ ] **Phase 37: Services Presence** - Online/offline/away/DND
- [ ] **Phase 38: Services Real-time** - Typing indicators, read receipts
- [ ] **Phase 39: Services Reactions** - Add/remove, streaming
- [ ] **Phase 40: Services Search** - Global search, semantic search
- [ ] **Phase 41: Services Moderation** - Ban, mute, kick, permissions
- [ ] **Phase 42: Services AI Integration** - Server-side AI, summarization
- [ ] **Phase 43: Services Saved Messages** - Personal vault

### State & Events
- [ ] **Phase 44: State Core Stores** - AppState, AuthState, UIState
- [ ] **Phase 45: State Chat Stores** - ChatState, MessageState, PinnedState
- [ ] **Phase 46: State User Stores** - UserState, PresenceState, PrivacyState
- [ ] **Phase 47: State Streaming & Offline** - StreamingState, OfflineState
- [ ] **Phase 48: Events Bus & Handlers** - EventBus, domain/UI events

### System Integration
- [ ] **Phase 49: System Keyring & Directories** - Secret Service, XDG
- [ ] **Phase 50: System Tray & Notifications** - StatusNotifierItem, notify-rust
- [ ] **Phase 51: System Utilities** - Clipboard, open URLs, autostart

### Media
- [ ] **Phase 52: Media File Handling** - File picker, MIME, thumbnails
- [ ] **Phase 53: Media Playback** - Audio (rodio), video player
- [ ] **Phase 54: Media Recording** - Voice, video recorder
- [ ] **Phase 55: Media Previews** - PDF viewer, code preview
- [ ] **Phase 56: Media GIF & Embeds** - GIF picker, YouTube/link embeds

### UI Layer
- [ ] **Phase 57: UI Foundation** - Slint setup, bridge, theme
- [ ] **Phase 58: UI Basic Components** - Avatar, button, input, modal, toast
- [ ] **Phase 59: UI Pickers** - Emoji, GIF, mention autocomplete
- [ ] **Phase 60: UI Sidebar** - Channel list, DM list, user profile
- [ ] **Phase 61: UI Chat Core** - Message list, message item, input
- [ ] **Phase 62: UI Chat Features** - Pinned bar, thread panel, reactions
- [ ] **Phase 63: UI File Components** - Attachments, previews
- [ ] **Phase 64: UI Modals** - Create channel, settings, profile, forward
- [ ] **Phase 65: UI Settings Screens** - Account, privacy, encryption
- [ ] **Phase 66: UI Auth Screens** - Login, register, loading

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
**Plans**: TBD

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
**Plans**: TBD

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

## Progress

**Execution Order:** Phases execute in numeric order (1 → 2 → ... → 66)

| Phase | Name | Plans | Status | Completed |
|-------|------|-------|--------|-----------|
| 1 | Scaffolding | 1 | Done | 2026-02-03 |
| 2 | Domain Core Entities | 2 | Done | 2026-02-03 |
| 3 | Domain Supporting Entities | 2 | Done | 2026-02-03 |
| 4 | Domain Privacy Entities | 2 | Done | 2026-02-03 |
| 5 | Storage Database Setup | 2 | Done | 2026-02-03 |
| 6 | Storage Core Repositories | 3 | Done | 2026-02-03 |
| 7 | Storage Supporting Repositories | 4 | In progress (3/4) | - |
| 8 | Storage Search Index | TBD | Not started | - |
| 9 | Storage Key Store | TBD | Not started | - |
| 10 | Crypto Signal Store | TBD | Not started | - |
| 11 | Crypto Identity & Sessions | TBD | Not started | - |
| 12 | Crypto Prekeys | TBD | Not started | - |
| 13 | Crypto Sender Keys | TBD | Not started | - |
| 14 | Crypto Sealed Sender | TBD | Not started | - |
| 15 | Crypto Attachments | TBD | Not started | - |
| 16 | Network gRPC Foundation | TBD | Not started | - |
| 17 | Network Auth & User Clients | TBD | Not started | - |
| 18 | Network Channel & Message Clients | TBD | Not started | - |
| 19 | Network Supporting Clients | TBD | Not started | - |
| 20 | Network Streaming | TBD | Not started | - |
| 21 | Network Resilience | TBD | Not started | - |
| 22 | Network Validation | TBD | Not started | - |
| 23 | Inbox Provider & Poller | TBD | Not started | - |
| 24 | Inbox Message Handlers | TBD | Not started | - |
| 25 | Inbox Sync Handlers | TBD | Not started | - |
| 26 | Mailbox Token Manager | TBD | Not started | - |
| 27 | Mailbox DM System | TBD | Not started | - |
| 28 | Mailbox Group System | TBD | Not started | - |
| 29 | Mailbox Privacy Features | TBD | Not started | - |
| 30 | Services Auth | TBD | Not started | - |
| 31 | Services Account | TBD | Not started | - |
| 32 | Services Message Core | TBD | Not started | - |
| 33 | Services Message Advanced | TBD | Not started | - |
| 34 | Services Ephemeral Messages | TBD | Not started | - |
| 35 | Services Channel | TBD | Not started | - |
| 36 | Services User | TBD | Not started | - |
| 37 | Services Presence | TBD | Not started | - |
| 38 | Services Real-time | TBD | Not started | - |
| 39 | Services Reactions | TBD | Not started | - |
| 40 | Services Search | TBD | Not started | - |
| 41 | Services Moderation | TBD | Not started | - |
| 42 | Services AI Integration | TBD | Not started | - |
| 43 | Services Saved Messages | TBD | Not started | - |
| 44 | State Core Stores | TBD | Not started | - |
| 45 | State Chat Stores | TBD | Not started | - |
| 46 | State User Stores | TBD | Not started | - |
| 47 | State Streaming & Offline | TBD | Not started | - |
| 48 | Events Bus & Handlers | TBD | Not started | - |
| 49 | System Keyring & Directories | TBD | Not started | - |
| 50 | System Tray & Notifications | TBD | Not started | - |
| 51 | System Utilities | TBD | Not started | - |
| 52 | Media File Handling | TBD | Not started | - |
| 53 | Media Playback | TBD | Not started | - |
| 54 | Media Recording | TBD | Not started | - |
| 55 | Media Previews | TBD | Not started | - |
| 56 | Media GIF & Embeds | TBD | Not started | - |
| 57 | UI Foundation | TBD | Not started | - |
| 58 | UI Basic Components | TBD | Not started | - |
| 59 | UI Pickers | TBD | Not started | - |
| 60 | UI Sidebar | TBD | Not started | - |
| 61 | UI Chat Core | TBD | Not started | - |
| 62 | UI Chat Features | TBD | Not started | - |
| 63 | UI File Components | TBD | Not started | - |
| 64 | UI Modals | TBD | Not started | - |
| 65 | UI Settings Screens | TBD | Not started | - |
| 66 | UI Auth Screens | TBD | Not started | - |
