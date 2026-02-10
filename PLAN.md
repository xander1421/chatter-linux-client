# Chatter Linux Client - Development Plan

## Overview

Native Rust Linux desktop client for Chatter secure messaging. Full feature parity with Electron frontend including Signal Protocol encryption, ephemeral mailboxes, and privacy-preserving messaging.

## Tech Stack

| Layer | Crate | Purpose |
|-------|-------|---------|
| UI | `slint` 1.9 | Native, cross-platform, declarative UI |
| Async | `tokio` | Async runtime |
| gRPC | `tonic` + `prost` | Generated from buf protos |
| Database | `rusqlite` + `bundled-sqlcipher` | Encrypted local storage |
| Keyring | `zbus` → Secret Service | D-Bus to GNOME Keyring / KWallet |
| Crypto | `libsignal-client` | Official Signal Protocol |
| Tray | `ksni` | StatusNotifierItem |
| Notifications | `notify-rust` | freedesktop notifications |
| XDG | `directories` | Standard Linux paths |

---

## Current Status

- [x] Phase 1: Foundation - Proto generation, compiling project
- [x] Phase 2: Storage Layer - SQLCipher database, keyring, migrations
- [x] Phase 3: Network Layer - gRPC client, auth interceptor, offline mode
- [x] Phase 4: UI Shell - Iced application, login view, main view
- [ ] Phase 5+: Signal Protocol and beyond

---

## Phase 1: Foundation

**Goal:** Get a compiling Rust project with proto imports working.

### Tasks

1. **Fix proto module structure**
   - [ ] Create proper mod.rs that exports generated code
   - [ ] Fix nested `#[path]` issues in tonic clients
   - [ ] Ensure all imports resolve correctly
   - [ ] Test: `cargo check` passes

2. **Minimal main.rs**
   - [ ] Import proto types
   - [ ] Print "Chatter Linux Client v0.1.0" and exit
   - [ ] Test: `cargo run` works

3. **Cargo.toml cleanup**
   - [ ] Only dependencies needed for current phase
   - [ ] Feature-gate future deps

**Deliverable:** `cargo run` prints version and exits cleanly.

---

## Phase 2: Storage Layer

**Goal:** SQLCipher database + Secret Service keyring working.

### Tasks

1. **XDG directories**
   - [ ] `~/.local/share/chatter` (data - database)
   - [ ] `~/.config/chatter` (config - settings.json)
   - [ ] `~/.cache/chatter` (cache - attachments, thumbnails)

2. **Secret Service (D-Bus keyring)**
   - [ ] Connect to `org.freedesktop.secrets`
   - [ ] Store/retrieve database encryption key
   - [ ] Store/retrieve device auth token
   - [ ] Fallback for headless (file-based with warning)

3. **SQLCipher database**
   - [ ] Open encrypted database with keyring key
   - [ ] Migration system (versioned schema)
   - [ ] Basic CRUD operations

4. **Schema (Phase 2 minimal)**
   ```sql
   -- Core device info
   CREATE TABLE device_info (
       key TEXT PRIMARY KEY,
       value TEXT NOT NULL
   );

   -- Settings
   CREATE TABLE settings (
       key TEXT PRIMARY KEY,
       value TEXT NOT NULL
   );
   ```

**Deliverable:** App opens encrypted DB, stores test value, retrieves it.

---

## Phase 3: Network Layer

**Goal:** Connect to backend, authenticate, make API calls.

### Tasks

1. **gRPC client setup**
   - [ ] Create tonic channel to backend (configurable URL)
   - [ ] TLS support (rustls)
   - [ ] Connection state management
   - [ ] Automatic reconnection with exponential backoff

2. **Authentication flow (Signal-style Basic auth)**
   - [ ] Generate device auth token (32 bytes random)
   - [ ] Login request → receive user_id + device_id
   - [ ] Store credentials: `{user_id}.{device_id}:token`
   - [ ] Auth interceptor adds `Authorization: Basic {base64}`
   - [ ] Restore session on startup

3. **Basic API calls**
   - [ ] `GetUserProfile` - verify auth works
   - [ ] `ListChannels` - fetch channel list
   - [ ] Error handling (auth expired, network errors)

**Deliverable:** App logs in, fetches user profile, prints username.

---

## Phase 4: Slint UI Shell

**Goal:** Basic windowed app with placeholder views.

### Tasks

1. **App structure**
   - [ ] `App` struct with Slint backend
   - [ ] `.slint` definitions for views
   - [ ] Rust <-> Slint bindings
   - [ ] Event handling callbacks
   - [ ] Property binding updates

2. **Theme**
   - [ ] Dark theme (default)
   - [ ] Light theme option
   - [ ] Custom colors matching Chatter brand

3. **Layout skeleton**
   ```
   ┌─────────────────────────────────────────────────────────┐
   │  Chatter                              [_] [□] [X]       │
   ├────────────┬────────────────────────────────────────────┤
   │            │                                            │
   │  Sidebar   │           Content Area                     │
   │            │                                            │
   │  - DMs     │     (channel messages / settings / etc)    │
   │  - Groups  │                                            │
   │  - Chans   │                                            │
   │            │                                            │
   │            ├────────────────────────────────────────────┤
   │            │  Composer (text input + send button)       │
   │  [+] New   │                                            │
   └────────────┴────────────────────────────────────────────┘
   ```

4. **Views (stubs)**
   - [ ] Login view (username/password form)
   - [ ] Main view (sidebar + content)
   - [ ] Settings view
   - [ ] Loading states

**Deliverable:** Window opens, shows sidebar with placeholder items.

---

## Phase 5: Signal Protocol Integration

**Goal:** Full E2E encryption compatible with other clients.

### Tasks

1. **libsignal-client integration**
   - [ ] Add `libsignal-client` dependency
   - [ ] Implement protocol stores (SQLite-backed):
     - `IdentityKeyStore` - our identity + trusted identities
     - `SessionStore` - Double Ratchet sessions
     - `PreKeyStore` - one-time prekeys
     - `SignedPreKeyStore` - signed prekeys
     - `SenderKeyStore` - group sender keys
     - `KyberPreKeyStore` - post-quantum keys (if supported)

2. **Key generation (first run)**
   - [ ] Generate identity keypair (Curve25519)
   - [ ] Generate signed prekey (rotate monthly)
   - [ ] Generate 100 one-time prekeys
   - [ ] Store in encrypted database

3. **Prekey bundle upload**
   - [ ] `UploadPreKeyBundle` on registration
   - [ ] `RefreshPreKeys` when count low
   - [ ] `GetPreKeyStats` to monitor

4. **Session establishment (X3DH)**
   - [ ] Fetch recipient's `PreKeyBundle`
   - [ ] Perform X3DH key agreement
   - [ ] Initialize Double Ratchet session
   - [ ] Store session in SessionStore

5. **Message encryption/decryption**
   - [ ] Encrypt with session → `CiphertextMessage`
   - [ ] Handle `PreKeySignalMessage` (new session)
   - [ ] Handle `SignalMessage` (established session)
   - [ ] Session ratchet on each message

**Deliverable:** Can establish session and encrypt/decrypt messages with another client.

---

## Phase 6: Sealed Sender

**Goal:** Anonymous messaging - server cannot see sender identity.

### Tasks

1. **Sender certificate management**
   - [ ] `RequestSenderCertificate` from server
   - [ ] Cache with expiration tracking
   - [ ] Auto-renew when < 48 hours remaining
   - [ ] Check interval: 60 minutes

2. **Server trust root**
   - [ ] Fetch `GetServerTrustRoot` on startup
   - [ ] Store for certificate validation
   - [ ] Verify server signature on certificates

3. **Sealed sender encryption**
   - [ ] Create `UnidentifiedSenderMessageContent`
   - [ ] Embed sender certificate
   - [ ] Encrypt entire envelope with recipient's identity key
   - [ ] Result: server sees only recipient, not sender

4. **Sealed sender decryption**
   - [ ] Decrypt envelope
   - [ ] Validate certificate against trust root
   - [ ] Check expiration (allow ±5 min clock skew)
   - [ ] Reject messages > 24h old (replay protection)
   - [ ] Extract sender identity

5. **UAK (Unidentified Access Key)**
   - [ ] Derive 16-byte UAK from profile key
   - [ ] Include in sealed sender requests
   - [ ] Handle unrestricted access mode

**Deliverable:** Messages sent anonymously, interop with other clients.

---

## Phase 7: Device Inbox

**Goal:** Receive messages when offline, sync across devices.

### Tasks

1. **Device inbox polling**
   - [ ] `FetchDeviceInbox` on startup
   - [ ] Process queued messages
   - [ ] Mark as delivered after processing
   - [ ] Delete from server after ack

2. **Message types from inbox**
   - [ ] Encrypted chat messages
   - [ ] Key exchange messages
   - [ ] Sender key distributions
   - [ ] Call E2EE keys
   - [ ] Sync messages (edit/delete)

3. **Inbox streaming**
   - [ ] `StreamDeviceInbox` for real-time
   - [ ] Reconnect on disconnect
   - [ ] Merge with polling results

4. **Multi-device sync**
   - [ ] Detect messages from own devices
   - [ ] Skip re-encryption to self
   - [ ] Sync read receipts across devices

**Deliverable:** Receive messages sent while offline.

---

## Phase 8: Ephemeral Mailbox System

**Goal:** Privacy-preserving DM delivery that breaks timing correlation.

### Tasks

1. **Blind signature client (Chaum)**
   - [ ] RSA blind signature implementation
   - [ ] Blind token creation
   - [ ] Unblind signed token
   - [ ] Token verification

2. **Token manager**
   - [ ] Request batch of tokens (max 10)
   - [ ] Store tokens in database
   - [ ] Track token count
   - [ ] Auto-replenish when low (< 5)

3. **Mailbox operations**
   - [ ] Create ephemeral mailbox (anonymous)
   - [ ] Poll mailbox for messages
   - [ ] Send to recipient's mailbox
   - [ ] Delete mailbox after use

4. **Conversation key exchange**
   - [ ] Generate ephemeral keypair per DM
   - [ ] Exchange keys on first message
   - [ ] Store their ephemeral public key
   - [ ] Key rotation support

5. **Mailbox message content types**
   - [ ] Chat messages
   - [ ] Typing indicators
   - [ ] Delivery receipts
   - [ ] Read receipts
   - [ ] Reactions
   - [ ] Pins
   - [ ] Delete requests
   - [ ] Edit requests

6. **Polling configuration**
   - [ ] Default interval: 5 seconds
   - [ ] Configurable in settings
   - [ ] Message deduplication (prevent replay errors)
   - [ ] Cover traffic option (privacy enhancement)

**Deliverable:** DMs route through ephemeral mailboxes, breaking timing correlation.

---

## Phase 9: Messages (Core CRUD)

**Goal:** Send, receive, display messages with full encryption.

### Tasks

1. **Send message flow**
   - [ ] Compose message content (protobuf)
   - [ ] Apply NDSS 2021 padding (size obfuscation)
   - [ ] Encrypt with Signal Protocol
   - [ ] Wrap in sealed sender envelope
   - [ ] Route: DM → ephemeral mailbox, Group → device inbox fanout
   - [ ] Optimistic UI update
   - [ ] Handle send failures

2. **Receive message flow**
   - [ ] Detect sealed sender vs regular
   - [ ] Decrypt envelope
   - [ ] Validate sender certificate
   - [ ] Decrypt Signal message
   - [ ] Parse content protobuf
   - [ ] Store in local database
   - [ ] Update UI

3. **Message display**
   - [ ] Message list (virtualized for performance)
   - [ ] Sender name + avatar
   - [ ] Timestamp formatting
   - [ ] Message grouping (same sender, close time)
   - [ ] Unread indicator

4. **Message persistence**
   - [ ] SQLite schema for messages
   - [ ] Index by channel + timestamp
   - [ ] Pagination support
   - [ ] Search index (FTS5)

5. **Content encoding (NDSS 2021)**
   - [ ] Size-based padding buckets
   - [ ] Prevent message type inference
   - [ ] Compatible with other clients

**Deliverable:** Full send/receive with encryption, persisted locally.

---

## Phase 10: Typing Indicators

**Goal:** Real-time typing status, privacy-preserving.

### Tasks

1. **Send typing indicator**
   - [ ] Detect text input changes
   - [ ] Debounce: send after 300ms of typing
   - [ ] Refresh: resend every 10s while typing
   - [ ] Stop: send after 3s of no input
   - [ ] Route via ephemeral mailbox (DMs)
   - [ ] Route via device inbox (groups)

2. **Receive typing indicator**
   - [ ] Parse typing message content
   - [ ] Store in memory (not persisted)
   - [ ] Auto-clear after 10s timeout
   - [ ] Update UI (animated dots)

3. **Privacy controls**
   - [ ] Setting to disable sending
   - [ ] Respect block list (don't send to blocked)
   - [ ] Don't show from blocked users

4. **UI display**
   - [ ] "User is typing..." in chat
   - [ ] Multiple users: "User1, User2 are typing..."
   - [ ] Subtle animation

**Deliverable:** See when others type, send own typing status.

---

## Phase 11: Receipts & Reactions

**Goal:** Delivery/read receipts and emoji reactions.

### Tasks

1. **Delivery receipts**
   - [ ] Send on message receive (after decrypt)
   - [ ] Store receipt in database
   - [ ] UI: single checkmark

2. **Read receipts**
   - [ ] Send when message scrolled into view
   - [ ] Batch receipts (don't spam)
   - [ ] Store per-user read state
   - [ ] UI: double checkmark / blue
   - [ ] Setting to disable sending

3. **Reactions**
   - [ ] Emoji picker UI
   - [ ] Send reaction message
   - [ ] Store reactions per message
   - [ ] Display reaction bar under message
   - [ ] Remove own reaction
   - [ ] Aggregate same emoji (show count)

4. **Sync across devices**
   - [ ] Receipt sync messages
   - [ ] Reaction sync messages

**Deliverable:** See delivery/read status, react with emoji.

---

## Phase 12: Edit, Delete, Pins

**Goal:** Message modification features.

### Tasks

1. **Edit message**
   - [ ] UI: edit button (own messages only)
   - [ ] Edit modal with current text
   - [ ] Send edit message (references original)
   - [ ] Receive edit: update local message
   - [ ] Show "edited" indicator
   - [ ] Edit history (optional)

2. **Delete message**
   - [ ] UI: delete button
   - [ ] Confirm dialog
   - [ ] "Delete for me" - local only
   - [ ] "Delete for everyone" - send delete message
   - [ ] Receive delete: remove or show "[deleted]"
   - [ ] Permission check (own messages, or admin in groups)

3. **Pin message**
   - [ ] UI: pin button
   - [ ] Send pin/unpin message
   - [ ] Store pins per channel
   - [ ] Pinned messages panel
   - [ ] Permission check (admin/mod in groups)

**Deliverable:** Edit, delete, pin messages.

---

## Phase 13: Threading & Replies

**Goal:** Reply to messages, threaded conversations.

### Tasks

1. **Quote/Reply**
   - [ ] UI: reply button on messages
   - [ ] Shows quoted message in composer
   - [ ] Send with quote reference (authorId, timestamp)
   - [ ] Display quoted message above reply

2. **Thread view**
   - [ ] Click quoted message → jump to original
   - [ ] Reply count indicator
   - [ ] "View thread" to see all replies
   - [ ] Thread panel (side view)

3. **Quote resolution**
   - [ ] Lookup parent message locally
   - [ ] Handle missing parent (deleted / not yet received)
   - [ ] Async resolution when parent arrives later

**Deliverable:** Reply to messages with quotes, view threads.

---

## Phase 14: Group Encryption (Sender Keys)

**Goal:** Efficient group messaging with forward secrecy.

### Tasks

1. **Sender key generation**
   - [ ] Generate sender key for each group
   - [ ] Store in SenderKeyStore
   - [ ] Ratchet after each message (forward secrecy)

2. **Sender key distribution**
   - [ ] Distribute key to all group members
   - [ ] Send via device inbox (per-member)
   - [ ] Handle new member joins
   - [ ] Handle member removals (rotate key)

3. **Group message encryption**
   - [ ] Encrypt with sender key (one operation)
   - [ ] All members can decrypt
   - [ ] Much more efficient than N×encrypt

4. **Group content types**
   - [ ] Messages
   - [ ] Typing indicators
   - [ ] Reactions
   - [ ] Edits/deletes
   - [ ] Pins

5. **Group mailbox addresses**
   - [ ] Per-member ephemeral mailbox for group
   - [ ] Exchange addresses on join
   - [ ] Privacy-preserving group delivery

**Deliverable:** Efficient encrypted group messaging.

---

## Phase 15: Files & Attachments

**Goal:** Send and receive encrypted files.

### Tasks

1. **Anonymous file upload**
   - [ ] Request presigned URL (anonymous)
   - [ ] Generate file encryption key (AES-256-GCM)
   - [ ] Encrypt file client-side
   - [ ] Upload encrypted blob
   - [ ] Create AttachmentPointer (key, digest, size)

2. **Send attachment**
   - [ ] File picker (GTK portal via ashpd)
   - [ ] Show upload progress
   - [ ] Embed AttachmentPointer in message
   - [ ] Support caption text

3. **Receive attachment**
   - [ ] Download encrypted blob
   - [ ] Decrypt with key from AttachmentPointer
   - [ ] Verify SHA-256 digest
   - [ ] Store in cache directory
   - [ ] Open with xdg-open

4. **Image handling**
   - [ ] Inline image preview
   - [ ] Thumbnail generation
   - [ ] Lazy loading
   - [ ] Image viewer overlay

5. **Supported types**
   - [ ] Images (JPEG, PNG, GIF, WebP)
   - [ ] Documents (PDF, etc.)
   - [ ] Video (MP4, WebM)
   - [ ] Audio files
   - [ ] Generic files

**Deliverable:** Share encrypted files and images.

---

## Phase 16: System Integration

**Goal:** Native Linux desktop experience.

### Tasks

1. **System tray (StatusNotifierItem)**
   - [ ] Tray icon (ksni)
   - [ ] Unread badge count
   - [ ] Menu: Show/Hide, Quit
   - [ ] Click: toggle window
   - [ ] Start minimized option

2. **Desktop notifications**
   - [ ] New message notification
   - [ ] Action buttons (Mark Read, Reply)
   - [ ] Notification grouping
   - [ ] Respect Do Not Disturb
   - [ ] Sound (optional)

3. **Autostart**
   - [ ] XDG autostart entry
   - [ ] Toggle in settings
   - [ ] Start minimized to tray

4. **Window management**
   - [ ] Remember window position/size
   - [ ] Minimize to tray on close (optional)
   - [ ] Single instance (D-Bus activation)

5. **File associations**
   - [ ] `chatter://` URL scheme
   - [ ] Open links in app

**Deliverable:** Full desktop integration.

---

## Phase 17: Search

**Goal:** Find messages and content.

### Tasks

1. **Local search**
   - [ ] SQLite FTS5 index
   - [ ] Search by text content
   - [ ] Filter by channel
   - [ ] Filter by sender
   - [ ] Filter by date range

2. **Search UI**
   - [ ] Global search bar (Ctrl+K)
   - [ ] Results list with context
   - [ ] Highlight matches
   - [ ] Click to jump to message

3. **Attachment search**
   - [ ] Search by filename
   - [ ] Filter by type (images, docs, etc.)

**Deliverable:** Find messages quickly.

---

## Phase 18: Voice & Video Calls (Optional)

**Goal:** Real-time communication with E2E encryption.

### Tasks

1. **Call initiation**
   - [ ] Start 1:1 call
   - [ ] Start group call
   - [ ] Incoming call notification
   - [ ] Accept/decline UI

2. **WebRTC integration**
   - [ ] libwebrtc or GStreamer
   - [ ] Audio capture/playback (PipeWire)
   - [ ] Video capture/display
   - [ ] Screen sharing

3. **E2E encryption for calls**
   - [ ] Generate call E2EE keys
   - [ ] Distribute via ephemeral mailbox
   - [ ] Encrypt media with AES-GCM
   - [ ] Key rotation

4. **Call UI**
   - [ ] Floating call window
   - [ ] Mute/unmute audio
   - [ ] Enable/disable video
   - [ ] End call button
   - [ ] Participant list (group)

5. **LiveKit integration**
   - [ ] Connect to LiveKit server
   - [ ] Handle room management
   - [ ] Client-side encryption layer

**Deliverable:** Voice and video calls with encryption.

---

## Phase 19: AI Integration (Optional)

**Goal:** AI-powered features.

### Tasks

1. **AI panel**
   - [ ] Side panel for AI chat
   - [ ] Conversation history
   - [ ] Model selection

2. **Features**
   - [ ] Summarize conversation
   - [ ] Suggest responses
   - [ ] Analyze sentiment
   - [ ] Translate messages

3. **Integration**
   - [ ] OpenAI API
   - [ ] HuggingFace models
   - [ ] Local LLM option

**Deliverable:** AI assistance in chat.

---

## Phase 20: Polish

**Goal:** Production-ready quality.

### Tasks

1. **Offline support**
   - [ ] Queue outgoing messages
   - [ ] Sync on reconnect
   - [ ] Offline indicator
   - [ ] Conflict resolution

2. **Error handling**
   - [ ] User-friendly error messages
   - [ ] Retry logic with backoff
   - [ ] Crash recovery
   - [ ] Error reporting (opt-in)

3. **Performance**
   - [ ] Message list virtualization
   - [ ] Image caching & lazy load
   - [ ] Startup optimization
   - [ ] Memory profiling

4. **Accessibility**
   - [ ] Keyboard navigation (full)
   - [ ] Screen reader support
   - [ ] High contrast theme
   - [ ] Font size settings

5. **Testing**
   - [ ] Unit tests for crypto
   - [ ] Integration tests for network
   - [ ] UI tests
   - [ ] Cross-client compatibility tests

6. **Packaging**
   - [ ] AppImage
   - [ ] Flatpak
   - [ ] .deb package
   - [ ] AUR package

**Deliverable:** Ready for daily use.

---

## File Structure (Target)

```
linux-client/
├── Cargo.toml
├── buf.gen.yaml
├── PLAN.md
│
├── src/
│   ├── main.rs                 # Entry point
│   ├── app.rs                  # Iced Application
│   ├── error.rs                # Error types
│   │
│   ├── proto/                  # Generated (do not edit)
│   │   ├── mod.rs
│   │   └── *.rs
│   │
│   ├── ui/                     # UI components
│   │   ├── mod.rs
│   │   ├── theme.rs            # Colors, fonts
│   │   ├── views/
│   │   │   ├── login.rs
│   │   │   ├── main.rs
│   │   │   ├── settings.rs
│   │   │   └── call.rs
│   │   ├── components/
│   │   │   ├── sidebar.rs
│   │   │   ├── chat_view.rs
│   │   │   ├── message.rs
│   │   │   ├── composer.rs
│   │   │   ├── emoji_picker.rs
│   │   │   └── search.rs
│   │   └── widgets/
│   │       ├── avatar.rs
│   │       └── badge.rs
│   │
│   ├── crypto/                 # Signal Protocol
│   │   ├── mod.rs
│   │   ├── identity.rs         # Key generation
│   │   ├── session.rs          # X3DH + Double Ratchet
│   │   ├── sealed_sender.rs    # Anonymous messaging
│   │   ├── sender_key.rs       # Group encryption
│   │   ├── blind_signature.rs  # Ephemeral mailbox tokens
│   │   └── stores/             # Protocol stores
│   │       ├── identity.rs
│   │       ├── session.rs
│   │       ├── prekey.rs
│   │       └── sender_key.rs
│   │
│   ├── network/                # gRPC client
│   │   ├── mod.rs
│   │   ├── client.rs           # Service clients
│   │   ├── auth.rs             # Auth interceptor
│   │   ├── streams.rs          # Event streaming
│   │   └── retry.rs            # Reconnection logic
│   │
│   ├── storage/                # Database + keyring
│   │   ├── mod.rs
│   │   ├── db.rs               # SQLCipher wrapper
│   │   ├── keyring.rs          # Secret Service
│   │   ├── migrations.rs       # Schema migrations
│   │   └── models/             # Data models
│   │       ├── message.rs
│   │       ├── channel.rs
│   │       ├── user.rs
│   │       └── settings.rs
│   │
│   ├── mailbox/                # Ephemeral mailbox
│   │   ├── mod.rs
│   │   ├── token_manager.rs
│   │   ├── mailbox_client.rs
│   │   └── conversation_keys.rs
│   │
│   ├── messaging/              # Message handling
│   │   ├── mod.rs
│   │   ├── send.rs
│   │   ├── receive.rs
│   │   ├── content.rs          # Content encoding
│   │   ├── typing.rs           # Typing indicators
│   │   ├── receipts.rs         # Read/delivery
│   │   ├── reactions.rs
│   │   └── sync.rs             # Multi-device sync
│   │
│   └── system/                 # System integration
│       ├── mod.rs
│       ├── tray.rs             # System tray
│       ├── notifications.rs    # Desktop notifications
│       └── autostart.rs        # XDG autostart
│
├── migrations/                 # SQL migrations
│   ├── 001_initial.sql
│   ├── 002_messages.sql
│   └── ...
│
└── assets/
    ├── icons/
    │   ├── chatter.svg
    │   └── tray/
    └── sounds/
        └── notification.ogg
```

---

## Database Schema (Complete)

```sql
-- ============================================================================
-- Device & Identity
-- ============================================================================

CREATE TABLE device_info (
    key TEXT PRIMARY KEY,
    value BLOB NOT NULL
);
-- Keys: user_id, device_id, identity_keypair, registration_id

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- ============================================================================
-- Signal Protocol Stores
-- ============================================================================

CREATE TABLE identity_keys (
    address TEXT PRIMARY KEY,  -- {user_id}.{device_id}
    identity_key BLOB NOT NULL,
    trusted INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE sessions (
    address TEXT PRIMARY KEY,  -- {user_id}.{device_id}
    record BLOB NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE prekeys (
    id INTEGER PRIMARY KEY,
    key_pair BLOB NOT NULL
);

CREATE TABLE signed_prekeys (
    id INTEGER PRIMARY KEY,
    key_pair BLOB NOT NULL,
    signature BLOB NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE sender_keys (
    group_id TEXT NOT NULL,
    sender_address TEXT NOT NULL,
    record BLOB NOT NULL,
    PRIMARY KEY (group_id, sender_address)
);

-- ============================================================================
-- Sealed Sender
-- ============================================================================

CREATE TABLE sender_certificate (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    certificate BLOB NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE TABLE server_trust_root (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    public_key BLOB NOT NULL
);

-- ============================================================================
-- Ephemeral Mailbox
-- ============================================================================

CREATE TABLE mailbox_tokens (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    token BLOB NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE conversation_keys (
    user_id TEXT PRIMARY KEY,
    our_keypair BLOB NOT NULL,
    their_public_key BLOB,
    created_at INTEGER NOT NULL
);

CREATE TABLE ephemeral_mailboxes (
    user_id TEXT PRIMARY KEY,
    mailbox_address TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- ============================================================================
-- Channels & Users
-- ============================================================================

CREATE TABLE users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL,
    display_name TEXT,
    avatar_url TEXT,
    profile_key BLOB,
    updated_at INTEGER NOT NULL
);

CREATE TABLE channels (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    type INTEGER NOT NULL,  -- 1=DM, 2=Group, 3=Public
    avatar_url TEXT,
    last_message_at INTEGER,
    unread_count INTEGER DEFAULT 0,
    muted INTEGER DEFAULT 0,
    archived INTEGER DEFAULT 0,
    updated_at INTEGER NOT NULL
);

CREATE TABLE channel_members (
    channel_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    role INTEGER DEFAULT 0,  -- 0=member, 1=admin, 2=owner
    PRIMARY KEY (channel_id, user_id)
);

-- ============================================================================
-- Messages
-- ============================================================================

CREATE TABLE messages (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL,
    author_id TEXT NOT NULL,
    content TEXT,  -- Decrypted content (JSON)
    timestamp INTEGER NOT NULL,
    edited_at INTEGER,
    deleted INTEGER DEFAULT 0,
    quote_author_id TEXT,
    quote_timestamp INTEGER,
    FOREIGN KEY (channel_id) REFERENCES channels(id)
);

CREATE INDEX idx_messages_channel ON messages(channel_id, timestamp);
CREATE INDEX idx_messages_author ON messages(author_id);

-- FTS5 for search
CREATE VIRTUAL TABLE messages_fts USING fts5(
    content,
    content=messages,
    content_rowid=rowid
);

-- ============================================================================
-- Reactions, Receipts, Pins
-- ============================================================================

CREATE TABLE reactions (
    message_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    emoji TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (message_id, user_id, emoji)
);

CREATE TABLE read_receipts (
    channel_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    last_read_timestamp INTEGER NOT NULL,
    PRIMARY KEY (channel_id, user_id)
);

CREATE TABLE pins (
    channel_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    pinned_by TEXT NOT NULL,
    pinned_at INTEGER NOT NULL,
    PRIMARY KEY (channel_id, message_id)
);

-- ============================================================================
-- Attachments
-- ============================================================================

CREATE TABLE attachments (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL,
    filename TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    size INTEGER NOT NULL,
    encryption_key BLOB NOT NULL,
    digest BLOB NOT NULL,
    cdn_url TEXT NOT NULL,
    local_path TEXT,  -- Cached path after download
    FOREIGN KEY (message_id) REFERENCES messages(id)
);

-- ============================================================================
-- Pending Operations (offline queue)
-- ============================================================================

CREATE TABLE pending_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    channel_id TEXT NOT NULL,
    content BLOB NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE processed_message_ids (
    message_id TEXT PRIMARY KEY,
    processed_at INTEGER NOT NULL
);
-- Cleanup: DELETE WHERE processed_at < now() - 24h
```

---

## Commands

```bash
# Generate protos (from chatter root)
buf generate --template linux-client/buf.gen.yaml

# Build
cd linux-client && cargo build

# Run
cd linux-client && cargo run

# Run with logging
cd linux-client && RUST_LOG=debug cargo run

# Release build
cd linux-client && cargo build --release

# Run tests
cd linux-client && cargo test

# Check without building
cd linux-client && cargo check

# Format
cd linux-client && cargo fmt

# Lint
cd linux-client && cargo clippy
```

---

## Environment Variables

```bash
# Required
CHATTER_SERVER_URL=https://api.chatter.example.com

# Optional
CHATTER_LOG_LEVEL=debug
CHATTER_DATA_DIR=~/.local/share/chatter
CHATTER_CONFIG_DIR=~/.config/chatter
```

---

## Next Step

Complete **Phase 1: Foundation** - get `cargo check` passing.

```bash
cd linux-client
cargo check
```

Fix any proto module issues, create main.rs, verify compilation.
