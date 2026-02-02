# Chatter Native Client

## What This Is

A native cross-platform desktop client for Chatter secure messaging, built with Rust and Slint. Replaces both Flutter mobile and React web clients with a single codebase that's a first-class citizen on Linux (and later macOS/Windows).

## Core Value

**True native experience with full Signal Protocol encryption.** Flutter and Electron are not first-class Linux citizens and cannot be built for all platforms reliably. This client solves both problems: native Wayland support, proper system integration, lightweight binary, and cross-platform from a single Rust codebase.

## Requirements

### Validated

(None yet — ship to validate)

### Active

**Foundation (Milestone 1)**
- [ ] Scaffold all folders/modules from ARCHITECTURE.md
- [ ] Validate no gaps in architecture
- [ ] Proto generation with buf (prost + tonic) compiles
- [ ] SQLCipher encrypted local database
- [ ] Secret Service keyring for credentials

**Network Layer**
- [ ] gRPC client connects to backend (localhost:50051)
- [ ] Signal-style device authentication (Basic auth)
- [ ] Connection recovery with exponential backoff
- [ ] Offline queue for pending operations
- [ ] Message validation pipeline (orderer, deduplicator, validator)

**Crypto Layer**
- [ ] Signal Protocol encryption (libsignal-client)
- [ ] X3DH key exchange
- [ ] Double Ratchet sessions
- [ ] Sender keys for groups
- [ ] Sealed Sender (anonymous messaging)
- [ ] Attachment encryption

**Mailbox System (NDSS '21)**
- [ ] Device inbox polling and processing
- [ ] Ephemeral mailbox for DMs (two-way anonymity)
- [ ] Group mailboxes with N-way fanout
- [ ] Blind signature token management
- [ ] Cover traffic generation
- [ ] Key rolling and mailbox expiration

**Messaging Features**
- [ ] Send, receive, edit, delete messages
- [ ] Message reactions
- [ ] Message threading (up to 3 concurrent)
- [ ] Message pinning with Telegram-style bar
- [ ] Message forwarding to channels/DMs/AI
- [ ] Ephemeral messages (TTL: 30s-24h)
- [ ] Draft auto-save per channel/thread
- [ ] Saved Messages (personal vault)
- [ ] @mentions with autocomplete

**Channel Features**
- [ ] Create, join, leave, archive channels
- [ ] Public, private, and DM channels
- [ ] Channel settings and permissions
- [ ] Channel templates
- [ ] Member management and moderation

**User Features**
- [ ] Authentication and session management
- [ ] User profiles with custom status
- [ ] Friends list and friend requests
- [ ] User blocking
- [ ] Presence (online/offline/away/DND)
- [ ] Session management (list/revoke)

**File Handling**
- [ ] File upload with progress
- [ ] File download and caching
- [ ] Image/video/audio preview
- [ ] PDF viewer with navigation
- [ ] Code preview with syntax highlighting (40+ languages)
- [ ] GIF picker integration

**Real-Time Features**
- [ ] Typing indicators
- [ ] Read receipts
- [ ] Reaction streaming
- [ ] Presence updates

**System Integration (Linux)**
- [ ] Native Wayland support (Slint)
- [ ] System tray (StatusNotifierItem via ksni)
- [ ] Desktop notifications (notify-rust)
- [ ] Secret Service keyring integration
- [ ] XDG directories compliance

**UI (Slint)**
- [ ] Dark theme (light theme out of scope for v1)
- [ ] Sidebar with channel list
- [ ] Chat view with virtualized message list
- [ ] Message composer with mentions/emoji/GIF
- [ ] Pinned messages bar with navigation
- [ ] Thread panel
- [ ] Settings screens
- [ ] Modal dialogs
- [ ] Command palette (Ctrl+K)

**AI Integration (Server-Side Only)**
- [ ] AI chat in channels
- [ ] AI streaming responses
- [ ] Conversation summarization
- [ ] Multiple provider support

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

---
*Last updated: 2026-02-03 after initialization*
