# Chatter Native Client

## What This Is

A native cross-platform desktop client for Chatter secure messaging, built with Rust and Slint. Replaces both Flutter mobile and React web clients with a single codebase that's a first-class citizen on Linux (and later macOS/Windows).

## Core Value

**True native experience with full Signal Protocol encryption.** Flutter and Electron are not first-class Linux citizens and cannot be built for all platforms reliably. This client solves both problems: native Wayland support, proper system integration, lightweight binary, and cross-platform from a single Rust codebase.

## Requirements

### Validated (Integration Tests Pass)

**Foundation**
- [x] Scaffold all folders/modules from ARCHITECTURE.md
- [x] Proto generation with buf (prost + tonic) compiles
- [x] SQLCipher encrypted local database
- [x] Secret Service keyring for credentials

**Network Layer**
- [x] gRPC client connects to backend (localhost:50051)
- [x] Signal-style device authentication (Basic auth)
- [x] Connection recovery with exponential backoff
- [x] Offline queue for pending operations
- [x] Message validation pipeline (orderer, deduplicator, validator)

**Crypto Layer**
- [x] Signal Protocol encryption (libsignal-client)
- [x] X3DH key exchange
- [x] Double Ratchet sessions
- [x] Sender keys for groups
- [x] Sealed Sender (anonymous messaging)
- [x] Attachment encryption

**Mailbox System (NDSS '21)**
- [x] Device inbox polling and processing
- [x] Ephemeral mailbox for DMs (two-way anonymity)
- [x] Group mailboxes with N-way fanout
- [x] Blind signature token management (RFC 9474 RSABSSA)
- [x] Cover traffic generation (opt-in, exponential distribution)
- [x] Key rolling and mailbox expiration (24h rotation threshold)
- [x] Traffic analysis resistance (log-bucketing padding)
- [x] Timing obfuscation (random delays 50-500ms)
- [x] Dummy polling (20% decoy polls)

**Messaging Features**
- [x] Send, receive, edit, delete messages
- [x] Message reactions
- [x] Message pinning with Telegram-style bar
- [x] Draft auto-save per channel/thread
- [x] Saved Messages (personal vault)
- [x] @mentions with autocomplete

**Channel Features**
- [x] Create, join, leave, archive channels
- [x] Public, private, and DM channels
- [x] Channel settings and permissions
- [x] Channel templates (including AI template)

**User Features**
- [x] Authentication and session management
- [x] User profiles with custom status
- [x] Friends list and friend requests
- [x] User blocking
- [x] Presence (online/offline/away/DND)
- [x] Session management (list/revoke)

**File Handling**
- [x] File upload with progress
- [x] File download and caching
- [x] Image/video/audio preview
- [x] PDF viewer with navigation
- [x] Code preview with syntax highlighting (40+ languages)
- [x] GIF picker integration

**Real-Time Features**
- [x] Typing indicators
- [x] Read receipts
- [x] Reaction streaming
- [x] Presence updates

**System Integration (Linux)**
- [x] Native Wayland support (Slint)
- [x] System tray (StatusNotifierItem via ksni)
- [x] Desktop notifications (notify-rust)
- [x] Secret Service keyring integration
- [x] XDG directories compliance

**UI (Slint)**
- [x] Dark theme (light theme out of scope for v1)
- [x] Sidebar with channel list
- [x] Chat view with virtualized message list
- [x] Message composer with mentions/emoji/GIF
- [x] Pinned messages bar with navigation
- [x] Settings screens
- [x] Modal dialogs
- [x] Auth screens (login/register)

**AI Integration (Server-Side Only)**
- [x] AI chat in channels
- [x] AI streaming responses
- [x] Multiple provider support

### Remaining Work

**Messaging (Nice to Have)**
- [ ] Message threading (up to 3 concurrent)
- [ ] Message forwarding to channels/DMs/AI
- [ ] Ephemeral messages (TTL: 30s-24h)

**UI Polish**
- [ ] Thread panel
- [ ] Command palette (Ctrl+K)
- [ ] Conversation summarization (AI)

**Testing & Release**
- [ ] Full unit test coverage
- [ ] Manual QA pass
- [ ] Release builds for Linux

### Out of Scope

- Voice/video calls — LiveKit integration is complex, use mobile for calls
- Local AI (Ollama/vLLM/MLC-LLM) — use server-side AI, local inference adds complexity
- Mobile (Android/iOS) — desktop-first (Linux, macOS, Windows), mobile is distant future
- Light theme — dark only for v1, simplifies theming
- Data migration — fresh start, no import from Flutter client

## Context

**Parent project:** Chatter (`/home/alex/Documents/chatter`) is a secure chat platform with:
- Go monolith backend (ConnectRPC on port 50051)
- PostgreSQL, Redis, RustFS, SpiceDB, Temporal
- Signal Protocol E2EE with sealed sender
- Ephemeral mailbox system (NDSS '21)

**Why native client:**
- Flutter desktop runs on XWayland, not native Wayland
- Electron is heavy (~100MB+) and not truly native
- Want lightweight binary (~15MB target)
- Better system integration (StatusNotifierItem, Secret Service, D-Bus)
- Rust can use libsignal-client natively (no FFI bridge)
- Single codebase for Linux, macOS, Windows (mobile later)

**Existing assets:**
- ARCHITECTURE.md (2364 lines) — comprehensive blueprint
- FEATURES.md — 200+ features documented
- Proto files in pkg/proto/ — shared with backend
- Generated proto code in src/proto/

**Reference implementations:**
- Signal-server: `/home/alex/Documents/chatter/Signal-server`
- Signal-Desktop: `/home/alex/Documents/chatter/Signal-Desktop`
- Flutter client backup: `/home/alex/Desktop/CHATTER_CLIENTS BACKUP/flutter-client`
- React frontend backup: `/home/alex/Desktop/CHATTER_CLIENTS BACKUP/frontend`

## Constraints

- **UI Framework:** Slint — committed, no GTK/Qt fallback
- **Crypto:** libsignal-client (official Signal Rust crate)
- **Storage:** SQLCipher for encrypted local database
- **Backend:** No changes to Go backend, use existing APIs
- **Build order:** Bottom-up (storage → crypto → network → services → UI)
- **Testing:** Ship first, add tests before v1 release
- **Platform:** Linux first, macOS/Windows after v1
- **Distribution:** Cargo install initially, packages later

## Key Decisions

| Decision | Rationale | Outcome |
|----------|-----------|---------|
| Slint over GTK4/Qt | Native Rust, native Wayland, Elm architecture, cross-platform | Committed |
| libsignal-client | Official Signal crate, proven, maintained by Signal | Committed |
| SQLCipher | Encrypted SQLite, matches mobile approach, single file | Committed |
| Bottom-up build | Infrastructure first ensures solid foundation | Committed |
| Start fresh on UI | Existing .slint files need redesign | Committed |
| Skip calls in v1 | Focus on core messaging, calls are complex | Committed |
| Skip local AI in v1 | Complexity vs value, server AI sufficient | Committed |
| Linux first | Primary target, port to others after v1 | Committed |
| Fresh start (no migration) | Simplifies v1, migration can come later | Committed |

## Integration Test Results (Phase 70)

All 66 integration tests pass against running server:

| Test Suite | Passed |
|------------|--------|
| integration_auth | 17 |
| integration_channel | 9 |
| integration_message | 10 |
| integration_user | 14 |
| integration_sealed_sender | 16 |
| **TOTAL** | **66** |

---
*Last updated: 2026-02-10 after Phase 70 End-to-End Smoke Test completion*
