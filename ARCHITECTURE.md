# Chatter Linux Client - Production Architecture

## Overview

This document defines the architecture for a production-grade native Linux client for Chatter, supporting all 100+ features from the web frontend.

## Design Principles

1. **Separation of Concerns** - Clear boundaries between layers
2. **Async-First** - Non-blocking operations throughout
3. **Offline-First** - Full functionality without network, sync when available
4. **Type Safety** - Leverage Rust's type system for correctness
5. **Testability** - Every component independently testable
6. **Error Recovery** - Graceful degradation, automatic retry
7. **Memory Efficiency** - Bounded caches, lazy loading
8. **Security by Default** - Encrypted storage, secure defaults

---

## Directory Structure

```
src/
├── main.rs                      # Entry point, runtime setup
├── lib.rs                       # Library exports
│
├── app/                         # Application orchestration
│   ├── mod.rs
│   ├── bootstrap.rs             # App initialization sequence
│   ├── shutdown.rs              # Graceful shutdown handling
│   └── config.rs                # Runtime configuration
│
├── domain/                      # Core business entities (no dependencies)
│   ├── mod.rs
│   ├── user.rs                  # User, Profile, Presence, StatusMessage
│   ├── message.rs               # Message, Thread, Draft, EphemeralMessage
│   ├── channel.rs               # Channel, Membership, Permissions, Template
│   ├── attachment.rs            # Attachment, FileMetadata, GifMetadata
│   ├── reaction.rs              # Reaction, Emoji
│   ├── call.rs                  # Call, Participant, CallState, VerificationBadge
│   ├── notification.rs          # Notification types
│   ├── ai.rs                    # AI Integration, Provider, LocalAIBackend
│   ├── moderation.rs            # Ban, Mute, Role, Permission
│   ├── saved_message.rs         # SavedMessage (personal vault)
│   ├── draft.rs                 # Draft with per-channel/thread persistence
│   ├── ephemeral.rs             # EphemeralMessage, TTL configs (30s-24h)
│   ├── mention.rs               # Mention, MentionType (@user, @channel, @here)
│   ├── embed.rs                 # LinkEmbed, YouTubeEmbed, LinkPreview
│   └── ids.rs                   # Strongly-typed IDs (UserId, ChannelId, etc.)
│
├── crypto/                      # All cryptographic operations
│   ├── mod.rs
│   ├── signal/                  # Signal Protocol implementation
│   │   ├── mod.rs
│   │   ├── store.rs             # SignalProtocolStore (identity, prekey, session, sender_key)
│   │   ├── session.rs           # Session management
│   │   ├── identity.rs          # Identity key operations
│   │   ├── prekey.rs            # PreKey bundle management
│   │   └── sender_key.rs        # Sender key for groups
│   ├── sealed_sender.rs         # Anonymous message sending
│   ├── group_encryption.rs      # Group E2EE
│   ├── attachment_crypto.rs     # File encryption/decryption
│   ├── key_derivation.rs        # HKDF, key stretching
│   ├── device_credentials.rs    # Device key management
│   └── rng.rs                   # Secure random number generation
│
├── storage/                     # Persistent storage layer
│   ├── mod.rs
│   ├── database.rs              # SQLCipher connection pool
│   ├── migrations/              # Database migrations
│   │   ├── mod.rs
│   │   └── v001_initial.rs
│   ├── repositories/            # Data access
│   │   ├── mod.rs
│   │   ├── user_repo.rs
│   │   ├── message_repo.rs
│   │   ├── channel_repo.rs
│   │   ├── attachment_repo.rs
│   │   ├── draft_repo.rs
│   │   ├── reaction_repo.rs
│   │   ├── call_repo.rs
│   │   ├── settings_repo.rs
│   │   └── search_index.rs      # FTS5 full-text search
│   ├── key_store.rs             # Signal protocol key storage
│   ├── cache.rs                 # In-memory LRU caches
│   └── blob_store.rs            # File/attachment storage
│
├── network/                     # All network communication
│   ├── mod.rs
│   ├── grpc/                    # gRPC clients
│   │   ├── mod.rs
│   │   ├── client_pool.rs       # Connection pooling
│   │   ├── interceptors.rs      # Auth, retry, logging
│   │   ├── auth_client.rs
│   │   ├── message_client.rs
│   │   ├── channel_client.rs
│   │   ├── user_client.rs
│   │   ├── file_client.rs
│   │   ├── call_client.rs
│   │   ├── notification_client.rs
│   │   ├── integration_client.rs
│   │   └── mailbox_client.rs
│   ├── streaming/               # Real-time streams
│   │   ├── mod.rs
│   │   ├── stream_manager.rs    # Manages all active streams
│   │   ├── message_stream.rs
│   │   ├── presence_stream.rs
│   │   ├── typing_stream.rs
│   │   ├── reaction_stream.rs
│   │   └── read_receipt_stream.rs
│   ├── connection.rs            # Connection state machine
│   ├── recovery.rs              # Connection recovery with exponential backoff
│   ├── retry.rs                 # Retry logic with jitter
│   ├── offline_queue.rs         # Queued operations for offline
│   └── validation/              # Message admission control
│       ├── mod.rs
│       ├── orderer.rs           # Message ordering guarantees
│       ├── deduplicator.rs      # Prevent duplicate processing
│       └── validator.rs         # Content validation pipeline
│
├── services/                    # Business logic / Use cases
│   ├── mod.rs
│   ├── auth_service.rs          # Login, logout, session management
│   ├── session_service.rs       # List sessions, revoke individual/all
│   ├── account_service.rs       # Password change, account deletion
│   ├── message_service.rs       # Send, edit, delete, forward, quote
│   ├── message_gateway.rs       # Central message routing, validation pipeline
│   ├── channel_service.rs       # Create, join, leave, archive, settings
│   ├── channel_template.rs      # Pre-configured channel templates
│   ├── user_service.rs          # Profile, friends, blocking, presence
│   ├── status_service.rs        # Custom status messages, mood text
│   ├── attachment_service.rs    # Upload, download, share, encryption
│   ├── call_service.rs          # Initiate, answer, decline, participants
│   ├── call_verification.rs     # Call E2EE verification badges
│   ├── notification_service.rs  # Desktop notifications, sounds
│   ├── search_service.rs        # Global search, filters, highlighting
│   ├── semantic_search.rs       # Embedding-based search (local AI)
│   ├── sync_service.rs          # Multi-device sync, conflict resolution
│   ├── offline_queue.rs         # Queue ops when offline, retry on reconnect
│   ├── moderation_service.rs    # Ban, mute, kick, permissions
│   ├── ai_service.rs            # AI integrations, streaming responses
│   ├── local_ai_service.rs      # Ollama, vLLM, MLC-LLM backends
│   ├── ai_summarize.rs          # Conversation/thread summarization
│   ├── draft_service.rs         # Message drafts, auto-save per channel/thread
│   ├── pin_service.rs           # Pin/unpin, pinned bar navigation
│   ├── thread_service.rs        # Thread panel, multi-thread support (up to 3)
│   ├── reaction_service.rs      # Add/remove reactions
│   ├── typing_service.rs        # Typing indicator send/receive
│   ├── read_receipt_service.rs  # Mark as read, receipt tracking
│   ├── presence_service.rs      # Online/offline/away/DND
│   ├── ephemeral_message.rs     # Self-destructing messages (TTL: 30s-24h)
│   ├── ephemeral_service.rs     # Anonymous mailbox, cover traffic
│   ├── saved_message_service.rs # Personal message vault (__saved__ channel)
│   ├── forward_service.rs       # Forward to channels, DMs, AI bots
│   ├── mention_service.rs       # @user, @channel, @here parsing
│   ├── embed_service.rs         # YouTube, link preview unfurling
│   └── gif_service.rs           # GIF search and insertion
│
├── sync/                        # Synchronization engine
│   ├── mod.rs
│   ├── sync_engine.rs           # Orchestrates all sync
│   ├── message_sync.rs          # Message sync logic
│   ├── channel_sync.rs          # Channel membership sync
│   ├── user_sync.rs             # Profile/presence sync
│   ├── key_sync.rs              # Encryption key sync
│   └── conflict.rs              # Conflict resolution strategies
│
├── inbox/                       # Device Inbox (Signal-style message queue)
│   ├── mod.rs
│   ├── provider.rs              # DeviceInboxProvider - main coordinator
│   ├── poller.rs                # Polling loop for inbox messages
│   ├── processor.rs             # Process different message types
│   ├── handlers/                # Type-specific message handlers
│   │   ├── mod.rs
│   │   ├── chat.rs              # Chat message handler
│   │   ├── attachment.rs        # Attachment content handler
│   │   ├── reaction.rs          # Reaction handler
│   │   ├── receipt.rs           # Read/delivery receipt handler
│   │   ├── typing.rs            # Typing indicator handler
│   │   ├── key_exchange.rs      # Signal key exchange handler
│   │   ├── sender_key.rs        # Group sender key distribution
│   │   ├── pin.rs               # Pin/unpin event handler
│   │   └── sync.rs              # Multi-device sync messages
│   └── queue.rs                 # Local queue for retry/ordering
│
├── mailbox/                     # Ephemeral Mailbox System (NDSS '21)
│   ├── mod.rs
│   ├── client.rs                # MailboxClient - anonymous server communication
│   ├── provider.rs              # MailboxProvider - orchestrates DM + group mailboxes
│   ├── token_manager.rs         # Blind signature token management (daily quota)
│   ├── blind_signature.rs       # Chaum RSA blind signature implementation
│   ├── poller.rs                # Mailbox polling loop (configurable interval)
│   ├── dm/                      # DM-specific mailbox handling
│   │   ├── mod.rs
│   │   ├── bootstrap.rs         # Initial sealed sender → mailbox exchange
│   │   ├── conversation.rs      # Per-conversation ephemeral keys
│   │   └── two_way.rs           # Two-way anonymity establishment
│   ├── group/                   # Group-specific mailbox handling
│   │   ├── mod.rs
│   │   ├── member_mailbox.rs    # Per-member, per-group mailbox management
│   │   ├── fanout.rs            # Client-side N-way message distribution
│   │   └── coordination.rs      # Group mailbox ID exchange on join
│   ├── cover_traffic.rs         # Fake message generation (timing obfuscation)
│   ├── key_rolling.rs           # Ephemeral key rotation and expiration
│   └── content.rs               # Mailbox message content encoding (NDSS padding)
│
├── media/                       # Media handling
│   ├── mod.rs
│   ├── image_processor.rs       # Resize, thumbnail, format conversion
│   ├── audio_recorder.rs        # Voice message recording (WAV/MP3)
│   ├── video_recorder.rs        # Video message recording (60s max, preview/retake)
│   ├── audio_player.rs          # Audio playback (rodio)
│   ├── video_player.rs          # Video playback integration
│   ├── file_picker.rs           # Native file dialogs (rfd)
│   ├── mime_detector.rs         # MIME type detection
│   ├── thumbnail_cache.rs       # Thumbnail caching
│   ├── pdf_viewer.rs            # PDF rendering (page nav, zoom)
│   ├── code_preview.rs          # Syntax highlighting (40+ languages), virtualized
│   ├── gif/                     # GIF support
│   │   ├── mod.rs
│   │   ├── picker.rs            # GIF search and selection UI
│   │   ├── client.rs            # GIF API client (Tenor/Giphy)
│   │   └── cache.rs             # GIF caching
│   └── embed/                   # Link embeds
│       ├── mod.rs
│       ├── youtube.rs           # YouTube embed extraction
│       ├── link_preview.rs      # Generic link preview (OpenGraph)
│       └── unfurl.rs            # URL unfurling service
│
├── system/                      # OS integration
│   ├── mod.rs
│   ├── keyring.rs               # Secure credential storage
│   ├── notifications.rs         # Desktop notifications (notify-rust)
│   ├── tray.rs                  # System tray (ksni)
│   ├── autostart.rs             # XDG autostart
│   ├── directories.rs           # XDG directories
│   ├── dbus.rs                  # D-Bus integration
│   ├── clipboard.rs             # Clipboard operations
│   └── open.rs                  # Open URLs/files externally
│
├── state/                       # Application state management
│   ├── mod.rs
│   ├── app_state.rs             # Global application state
│   ├── auth_state.rs            # Authentication state
│   ├── ui_state.rs              # UI preferences, theme, compact mode
│   ├── chat_state.rs            # Active channel, thread (up to 3 concurrent)
│   ├── message_state.rs         # Message cache, pagination, drafts
│   ├── user_state.rs            # User profiles, presence cache
│   ├── streaming_state.rs       # Real-time data (typing, reactions)
│   ├── call_state.rs            # Active call state
│   ├── privacy_state.rs         # Per-conversation privacy settings
│   ├── offline_state.rs         # Offline queue, pending operations
│   ├── pinned_state.rs          # Pinned messages bar navigation
│   └── store.rs                 # Reactive state container
│
├── ui/                          # Slint UI layer
│   ├── mod.rs
│   ├── bridge.rs                # Rust <-> Slint data bridge
│   ├── adapters/                # Convert domain -> UI models
│   │   ├── mod.rs
│   │   ├── message_adapter.rs
│   │   ├── channel_adapter.rs
│   │   ├── user_adapter.rs
│   │   └── call_adapter.rs
│   ├── commands/                # UI -> Service commands
│   │   ├── mod.rs
│   │   ├── message_commands.rs
│   │   ├── channel_commands.rs
│   │   ├── user_commands.rs
│   │   └── call_commands.rs
│   ├── handlers/                # Event handlers
│   │   ├── mod.rs
│   │   ├── keyboard.rs          # Keyboard shortcuts
│   │   ├── drag_drop.rs         # Drag and drop files
│   │   ├── gestures.rs          # Swipe gestures on channels
│   │   └── command_palette.rs   # Ctrl+K command palette
│   └── widgets/                 # Custom UI widgets
│       ├── mod.rs
│       ├── mention_autocomplete.rs  # @user/@channel dropdown
│       ├── emoji_picker.rs      # Virtualized emoji picker
│       ├── gif_picker.rs        # GIF search/selection
│       ├── pinned_bar.rs        # Telegram-style pinned messages bar
│       ├── thread_panel.rs      # Thread conversation panel
│       ├── forward_dialog.rs    # Forward message dialog
│       └── verification_badge.rs # Call E2EE verification display
│
├── local_ai/                    # Local AI infrastructure
│   ├── mod.rs
│   ├── backend.rs               # Backend trait + implementations
│   ├── ollama.rs                # Ollama backend (default: llama3.2:3b)
│   ├── vllm.rs                  # vLLM backend
│   ├── mlc_llm.rs               # MLC-LLM backend
│   ├── model_manager.rs         # Download, update, switch models
│   ├── embeddings.rs            # Local embedding generation (nomic-embed-text)
│   ├── vector_store.rs          # Vector similarity search
│   ├── indexer.rs               # Auto-index messages for semantic search
│   └── settings.rs              # Local AI configuration
│
├── events/                      # Event system
│   ├── mod.rs
│   ├── bus.rs                   # Event bus (broadcast channel)
│   ├── domain_events.rs         # Business events
│   ├── ui_events.rs             # UI-specific events
│   └── handlers.rs              # Event handlers registry
│
├── error/                       # Error handling
│   ├── mod.rs
│   ├── app_error.rs             # Application error types
│   ├── network_error.rs         # Network-specific errors
│   ├── crypto_error.rs          # Crypto-specific errors
│   ├── storage_error.rs         # Storage-specific errors
│   └── recovery.rs              # Error recovery strategies
│
└── util/                        # Shared utilities
    ├── mod.rs
    ├── time.rs                  # Time formatting, relative time
    ├── text.rs                  # Text utilities, highlighting
    ├── id_generator.rs          # UUID generation
    ├── rate_limiter.rs          # Rate limiting
    ├── debounce.rs              # Debounce/throttle
    └── bounded_vec.rs           # Size-bounded collections

ui/                              # Slint UI files
├── main.slint                   # Root component
├── theme/
│   ├── colors.slint             # Color definitions
│   ├── typography.slint         # Font styles
│   ├── spacing.slint            # Spacing/sizing tokens
│   └── icons.slint              # Icon definitions
├── components/                  # Reusable components
│   ├── avatar.slint
│   ├── button.slint
│   ├── input.slint
│   ├── modal.slint
│   ├── toast.slint
│   ├── tooltip.slint
│   ├── skeleton.slint
│   ├── emoji_picker.slint       # Virtualized, with favorites
│   ├── gif_picker.slint         # GIF search integration
│   ├── file_preview.slint
│   ├── code_preview.slint       # Syntax highlighting (40+ langs)
│   ├── pdf_viewer.slint         # Page navigation, zoom
│   ├── audio_player.slint
│   ├── video_player.slint
│   ├── voice_recorder.slint     # WAV/MP3 recording
│   ├── video_recorder.slint     # 60s max, preview/retake
│   ├── progress.slint
│   ├── command_palette.slint    # Ctrl+K shortcut overlay
│   ├── mention_autocomplete.slint # @user/@channel dropdown
│   ├── link_embed.slint         # YouTube/URL preview
│   ├── swipeable_item.slint     # Swipe actions (archive/pin/mute)
│   ├── verification_badge.slint # E2EE call verification
│   └── ephemeral_badge.slint    # Self-destruct countdown timer
├── chat/
│   ├── chat_layout.slint        # Main chat layout
│   ├── message_list.slint       # Virtualized message list
│   ├── message_item.slint       # Single message with context menu
│   ├── message_input.slint      # Message composer with mentions
│   ├── chat_header.slint        # Channel header with call buttons
│   ├── pinned_bar.slint         # Telegram-style bar with nav arrows
│   ├── thread_panel.slint       # Thread view (up to 3 concurrent)
│   ├── thread_bubbles.slint     # Quick access to open threads
│   ├── reactions.slint          # Reaction display/picker
│   ├── typing_indicator.slint   # Typing status
│   ├── read_receipts.slint      # Read receipt indicators
│   ├── attachment_tray.slint    # File attachments
│   ├── draft_indicator.slint    # Draft message indicator
│   ├── forward_preview.slint    # Forward message preview
│   ├── quote_preview.slint      # Quoted message preview
│   └── ai_panel.slint           # AI assistant panel
├── sidebar/
│   ├── sidebar.slint            # Main sidebar
│   ├── channel_list.slint       # Channel list with swipe actions
│   ├── channel_item.slint       # Single channel (swipeable)
│   ├── dm_list.slint            # Direct messages
│   ├── saved_messages.slint     # Personal message vault entry
│   ├── user_profile.slint       # Compact user profile + status
│   ├── status_editor.slint      # Custom status message editor
│   └── navigation.slint         # Navigation controls
├── modals/
│   ├── create_channel.slint     # Channel creation with templates
│   ├── channel_templates.slint  # Pre-configured channel types
│   ├── channel_settings.slint
│   ├── user_profile.slint
│   ├── friend_requests.slint
│   ├── invite_member.slint
│   ├── forward_message.slint    # Forward to channels/DMs/AI
│   ├── file_share.slint
│   ├── integrations.slint
│   ├── settings.slint
│   ├── change_password.slint
│   ├── delete_account.slint
│   ├── session_management.slint # List/revoke sessions
│   ├── summarize_dialog.slint   # AI conversation summary
│   └── model_manager.slint      # Local AI model download/switch
├── calls/
│   ├── call_screen.slint        # Active call UI
│   ├── incoming_call.slint      # Incoming call notification
│   ├── call_controls.slint      # Mute, camera, etc.
│   └── participants.slint       # Call participants
├── settings/
│   ├── settings_layout.slint
│   ├── account.slint
│   ├── privacy.slint            # Typing/receipt toggles per convo
│   ├── encryption.slint
│   ├── notifications.slint
│   ├── appearance.slint         # Theme, compact mode
│   ├── ai_settings.slint        # Cloud AI integrations
│   ├── local_ai_settings.slint  # Ollama/vLLM/MLC config
│   ├── ephemeral_settings.slint # Default TTL, cover traffic
│   └── keyboard_shortcuts.slint # Shortcut customization
└── pages/
    ├── login.slint
    ├── register.slint
    └── loading.slint
```

---

## Layer Dependencies

```
┌─────────────────────────────────────────────────────────────┐
│                         UI (Slint)                          │
│  ┌─────────┐  ┌──────────┐  ┌──────────┐  ┌─────────────┐  │
│  │ Views   │  │Components│  │ Adapters │  │  Commands   │  │
│  └────┬────┘  └────┬─────┘  └────┬─────┘  └──────┬──────┘  │
└───────┼────────────┼─────────────┼───────────────┼──────────┘
        │            │             │               │
        ▼            ▼             ▼               ▼
┌─────────────────────────────────────────────────────────────┐
│                      State Layer                            │
│  ┌──────────┐  ┌───────────┐  ┌───────────┐  ┌──────────┐  │
│  │ AppState │  │ ChatState │  │ UserState │  │CallState │  │
│  └────┬─────┘  └─────┬─────┘  └─────┬─────┘  └────┬─────┘  │
└───────┼──────────────┼──────────────┼─────────────┼─────────┘
        │              │              │             │
        ▼              ▼              ▼             ▼
┌─────────────────────────────────────────────────────────────┐
│                    Service Layer                            │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐  │
│  │AuthService  │  │MessageSvc   │  │ChannelSvc, UserSvc  │  │
│  │CallService  │  │SyncService  │  │AIService, SearchSvc │  │
│  └──────┬──────┘  └──────┬──────┘  └──────────┬──────────┘  │
└─────────┼────────────────┼────────────────────┼─────────────┘
          │                │                    │
          ▼                ▼                    ▼
┌─────────────────────────────────────────────────────────────┐
│              Domain Layer (Pure Rust, No deps)              │
│  ┌──────┐  ┌─────────┐  ┌─────────┐  ┌──────┐  ┌────────┐  │
│  │ User │  │ Message │  │ Channel │  │ Call │  │ AI     │  │
│  └──────┘  └─────────┘  └─────────┘  └──────┘  └────────┘  │
└─────────────────────────────────────────────────────────────┘
          │                │                    │
          ▼                ▼                    ▼
┌─────────────────────────────────────────────────────────────┐
│                  Infrastructure Layer                       │
│  ┌───────────┐  ┌────────────┐  ┌───────────┐  ┌─────────┐ │
│  │  Storage  │  │  Network   │  │   Crypto  │  │ System  │ │
│  │ (SQLite)  │  │  (gRPC)    │  │ (Signal)  │  │ (D-Bus) │ │
│  └───────────┘  └────────────┘  └───────────┘  └─────────┘ │
└─────────────────────────────────────────────────────────────┘
```

---

## Core Components Detail

### 1. Domain Layer (`src/domain/`)

Pure Rust structs with no external dependencies. These are the source of truth.

```rust
// src/domain/ids.rs
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct UserId(pub Uuid);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ChannelId(pub Uuid);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MessageId(pub Uuid);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DeviceId(pub u32);

// src/domain/message.rs
#[derive(Clone, Debug)]
pub struct Message {
    pub id: MessageId,
    pub channel_id: ChannelId,
    pub sender_id: UserId,
    pub content: MessageContent,
    pub thread_id: Option<MessageId>,
    pub reply_to: Option<MessageId>,
    pub attachments: Vec<AttachmentId>,
    pub reactions: Vec<Reaction>,
    pub is_pinned: bool,
    pub is_edited: bool,
    pub is_deleted: bool,
    pub is_encrypted: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub enum MessageContent {
    Text(String),
    Encrypted(EncryptedContent),
    System(SystemMessage),
    Call(CallMessage),
    AI(AIMessage),
}

#[derive(Clone, Debug)]
pub struct EncryptedContent {
    pub ciphertext: Vec<u8>,
    pub sender_key_id: Option<u32>,
    pub message_type: EncryptedMessageType,
}

#[derive(Clone, Debug)]
pub enum EncryptedMessageType {
    Signal,
    SealedSender,
    SenderKey, // Group
}
```

### 2. Crypto Layer (`src/crypto/`)

Wraps libsignal with proper storage backends.

```rust
// src/crypto/signal/store.rs
pub struct SignalStore {
    db: Arc<Database>,
    identity_store: IdentityKeyStore,
    prekey_store: PreKeyStore,
    signed_prekey_store: SignedPreKeyStore,
    session_store: SessionStore,
    sender_key_store: SenderKeyStore,
}

impl SignalStore {
    pub async fn new(db: Arc<Database>) -> Result<Self>;

    // Identity operations
    pub async fn get_identity_key_pair(&self) -> Result<IdentityKeyPair>;
    pub async fn get_local_registration_id(&self) -> Result<u32>;

    // Session operations
    pub async fn encrypt_message(
        &self,
        recipient: &ProtocolAddress,
        plaintext: &[u8],
    ) -> Result<CiphertextMessage>;

    pub async fn decrypt_message(
        &self,
        sender: &ProtocolAddress,
        ciphertext: &CiphertextMessage,
    ) -> Result<Vec<u8>>;

    // Group operations
    pub async fn encrypt_group_message(
        &self,
        group_id: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>>;

    pub async fn decrypt_group_message(
        &self,
        sender: &ProtocolAddress,
        group_id: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>>;
}

// src/crypto/sealed_sender.rs
pub struct SealedSenderService {
    signal_store: Arc<SignalStore>,
    server_public_key: PublicKey,
}

impl SealedSenderService {
    pub async fn seal_message(
        &self,
        recipient: &ProtocolAddress,
        plaintext: &[u8],
    ) -> Result<Vec<u8>>;

    pub async fn unseal_message(
        &self,
        ciphertext: &[u8],
    ) -> Result<(ProtocolAddress, Vec<u8>)>;
}
```

### 3. Storage Layer (`src/storage/`)

SQLCipher-backed persistence with proper migrations.

```rust
// src/storage/database.rs
pub struct Database {
    pool: Pool<SqliteConnectionManager>,
    cipher_key: SecretVec<u8>,
}

impl Database {
    pub async fn open(path: &Path, passphrase: &str) -> Result<Self>;
    pub async fn run_migrations(&self) -> Result<()>;
    pub fn connection(&self) -> Result<PooledConnection>;
}

// src/storage/repositories/message_repo.rs
pub struct MessageRepository {
    db: Arc<Database>,
}

impl MessageRepository {
    // CRUD
    pub async fn insert(&self, message: &Message) -> Result<()>;
    pub async fn get(&self, id: MessageId) -> Result<Option<Message>>;
    pub async fn update(&self, message: &Message) -> Result<()>;
    pub async fn delete(&self, id: MessageId) -> Result<()>;

    // Queries
    pub async fn get_by_channel(
        &self,
        channel_id: ChannelId,
        before: Option<MessageId>,
        limit: usize,
    ) -> Result<Vec<Message>>;

    pub async fn get_thread(
        &self,
        thread_id: MessageId,
        before: Option<MessageId>,
        limit: usize,
    ) -> Result<Vec<Message>>;

    pub async fn get_pinned(&self, channel_id: ChannelId) -> Result<Vec<Message>>;

    pub async fn search(
        &self,
        query: &str,
        channel_id: Option<ChannelId>,
        limit: usize,
    ) -> Result<Vec<Message>>;

    // Sync
    pub async fn get_pending_sync(&self) -> Result<Vec<Message>>;
    pub async fn mark_synced(&self, id: MessageId, server_id: Uuid) -> Result<()>;
}
```

### 4. Network Layer (`src/network/`)

gRPC clients with proper connection management.

```rust
// src/network/grpc/client_pool.rs
pub struct GrpcClientPool {
    endpoint: Endpoint,
    auth_token: RwLock<Option<String>>,
    channel: RwLock<Option<Channel>>,
}

impl GrpcClientPool {
    pub async fn connect(&self) -> Result<()>;
    pub async fn disconnect(&self);
    pub fn set_auth_token(&self, token: String);
    pub async fn channel(&self) -> Result<Channel>;
}

// src/network/grpc/message_client.rs
pub struct MessageClient {
    pool: Arc<GrpcClientPool>,
    inner: chat::message_service_client::MessageServiceClient<InterceptedService>,
}

impl MessageClient {
    pub async fn send_message(&self, req: SendMessageRequest) -> Result<Message>;
    pub async fn edit_message(&self, req: EditMessageRequest) -> Result<Message>;
    pub async fn delete_message(&self, id: MessageId) -> Result<()>;
    pub async fn get_messages(&self, req: GetMessagesRequest) -> Result<Vec<Message>>;
    pub async fn pin_message(&self, id: MessageId) -> Result<()>;
    pub async fn unpin_message(&self, id: MessageId) -> Result<()>;
}

// src/network/streaming/stream_manager.rs
pub struct StreamManager {
    message_stream: Option<JoinHandle<()>>,
    presence_stream: Option<JoinHandle<()>>,
    typing_stream: Option<JoinHandle<()>>,
    reaction_stream: Option<JoinHandle<()>>,
    shutdown: broadcast::Sender<()>,
}

impl StreamManager {
    pub async fn start_all(&mut self, user_id: UserId) -> Result<()>;
    pub async fn stop_all(&mut self);
    pub async fn reconnect(&mut self) -> Result<()>;
}
```

### 5. Service Layer (`src/services/`)

Business logic orchestrating domain, storage, network, and crypto.

```rust
// src/services/message_service.rs
pub struct MessageService {
    repo: Arc<MessageRepository>,
    client: Arc<MessageClient>,
    crypto: Arc<SignalStore>,
    events: EventBus,
    state: Arc<MessageState>,
}

impl MessageService {
    /// Send a message, handling encryption and offline queue
    pub async fn send_message(
        &self,
        channel_id: ChannelId,
        content: String,
        attachments: Vec<AttachmentId>,
        reply_to: Option<MessageId>,
    ) -> Result<Message> {
        // 1. Create local message
        let message = Message::new_outgoing(channel_id, content, attachments, reply_to);

        // 2. Encrypt if channel requires it
        let encrypted = if self.should_encrypt(channel_id).await? {
            self.encrypt_for_channel(&message).await?
        } else {
            message.clone()
        };

        // 3. Store locally (optimistic)
        self.repo.insert(&message).await?;
        self.state.add_message(message.clone());
        self.events.emit(MessageEvent::Sent(message.clone()));

        // 4. Send to server (or queue if offline)
        match self.client.send_message(encrypted.into()).await {
            Ok(server_msg) => {
                self.repo.mark_synced(message.id, server_msg.server_id).await?;
                Ok(message)
            }
            Err(e) if e.is_offline() => {
                self.queue_for_sync(message.clone()).await?;
                Ok(message)
            }
            Err(e) => {
                self.repo.delete(message.id).await?;
                self.state.remove_message(message.id);
                Err(e)
            }
        }
    }

    /// Edit an existing message
    pub async fn edit_message(
        &self,
        id: MessageId,
        new_content: String,
    ) -> Result<Message>;

    /// Delete a message
    pub async fn delete_message(&self, id: MessageId) -> Result<()>;

    /// Forward a message to another channel
    pub async fn forward_message(
        &self,
        id: MessageId,
        to_channel: ChannelId,
    ) -> Result<Message>;

    /// Load messages for a channel (with pagination)
    pub async fn load_messages(
        &self,
        channel_id: ChannelId,
        before: Option<MessageId>,
        limit: usize,
    ) -> Result<Vec<Message>>;

    /// Pin a message
    pub async fn pin_message(&self, id: MessageId) -> Result<()>;

    /// Unpin a message
    pub async fn unpin_message(&self, id: MessageId) -> Result<()>;

    /// Search messages
    pub async fn search(
        &self,
        query: &str,
        channel_id: Option<ChannelId>,
    ) -> Result<Vec<Message>>;
}
```

### 6. State Layer (`src/state/`)

Reactive state management for UI binding.

```rust
// src/state/store.rs
pub struct Store<T: Clone + Send + Sync + 'static> {
    state: Arc<RwLock<T>>,
    subscribers: Arc<RwLock<Vec<Sender<T>>>>,
}

impl<T: Clone + Send + Sync + 'static> Store<T> {
    pub fn new(initial: T) -> Self;
    pub fn get(&self) -> T;
    pub fn set(&self, value: T);
    pub fn update(&self, f: impl FnOnce(&mut T));
    pub fn subscribe(&self) -> Receiver<T>;
}

// src/state/message_state.rs
#[derive(Clone, Default)]
pub struct MessageState {
    /// Messages indexed by channel
    pub by_channel: HashMap<ChannelId, Vec<Message>>,
    /// Pinned messages by channel
    pub pinned: HashMap<ChannelId, Vec<Message>>,
    /// Thread messages
    pub threads: HashMap<MessageId, Vec<Message>>,
    /// Pending messages (not yet synced)
    pub pending: HashSet<MessageId>,
    /// Failed messages
    pub failed: HashSet<MessageId>,
}

impl MessageState {
    pub fn add_message(&mut self, msg: Message);
    pub fn remove_message(&mut self, id: MessageId);
    pub fn update_message(&mut self, msg: Message);
    pub fn get_channel_messages(&self, channel_id: ChannelId) -> &[Message];
    pub fn get_thread_messages(&self, thread_id: MessageId) -> &[Message];
}

// src/state/streaming_state.rs
#[derive(Clone, Default)]
pub struct StreamingState {
    /// Users currently typing per channel
    pub typing: HashMap<ChannelId, Vec<TypingUser>>,
    /// Recent reactions (for animation)
    pub reactions: HashMap<MessageId, Vec<ReactionEvent>>,
    /// Read receipt state per channel
    pub read_receipts: HashMap<ChannelId, HashMap<UserId, MessageId>>,
    /// Presence updates
    pub presence: HashMap<UserId, PresenceStatus>,
}
```

### 7. Event System (`src/events/`)

Decoupled communication between components.

```rust
// src/events/bus.rs
pub struct EventBus {
    sender: broadcast::Sender<AppEvent>,
}

impl EventBus {
    pub fn new() -> Self;
    pub fn emit(&self, event: AppEvent);
    pub fn subscribe(&self) -> broadcast::Receiver<AppEvent>;
}

// src/events/domain_events.rs
#[derive(Clone, Debug)]
pub enum AppEvent {
    // Auth
    LoggedIn(User),
    LoggedOut,
    SessionExpired,

    // Messages
    MessageReceived(Message),
    MessageSent(Message),
    MessageEdited(Message),
    MessageDeleted(MessageId),
    MessagePinned(Message),
    MessageUnpinned(MessageId),

    // Channels
    ChannelCreated(Channel),
    ChannelUpdated(Channel),
    ChannelArchived(ChannelId),
    ChannelJoined(ChannelId),
    ChannelLeft(ChannelId),

    // Users
    ProfileUpdated(User),
    PresenceChanged(UserId, PresenceStatus),
    FriendRequestReceived(FriendRequest),
    FriendAdded(UserId),
    UserBlocked(UserId),

    // Real-time
    TypingStarted(ChannelId, UserId),
    TypingStopped(ChannelId, UserId),
    ReactionAdded(MessageId, Reaction),
    ReactionRemoved(MessageId, ReactionId),
    ReadReceiptReceived(ChannelId, UserId, MessageId),

    // Calls
    IncomingCall(Call),
    CallAnswered(CallId),
    CallEnded(CallId),

    // Sync
    SyncStarted,
    SyncCompleted,
    SyncFailed(String),

    // Connection
    Connected,
    Disconnected,
    Reconnecting,

    // Notifications
    NotificationReceived(Notification),
}
```

### 8. UI Bridge (`src/ui/`)

Connects Rust state to Slint UI.

```rust
// src/ui/bridge.rs
pub struct UiBridge {
    app: Weak<MainWindow>,
    state: Arc<AppState>,
    commands: CommandDispatcher,
}

impl UiBridge {
    pub fn new(app: &MainWindow, state: Arc<AppState>) -> Self;

    /// Bind state to UI models
    pub fn bind_state(&self) {
        // Bind channels
        let channels = self.state.chat.subscribe();
        let app = self.app.clone();
        tokio::spawn(async move {
            while let Ok(state) = channels.recv().await {
                if let Some(app) = app.upgrade() {
                    let model: Vec<ChannelModel> = state.channels
                        .iter()
                        .map(ChannelAdapter::to_model)
                        .collect();
                    app.set_channels(ModelRc::from(model));
                }
            }
        });

        // Bind messages, users, etc.
    }

    /// Setup UI callbacks
    pub fn setup_callbacks(&self) {
        let app = self.app.upgrade().unwrap();
        let commands = self.commands.clone();

        app.on_send_message(move |channel_id, content| {
            commands.send_message(channel_id.into(), content.into());
        });

        app.on_channel_selected(move |channel_id| {
            commands.select_channel(channel_id.into());
        });

        // More callbacks...
    }
}

// src/ui/adapters/message_adapter.rs
pub struct MessageAdapter;

impl MessageAdapter {
    pub fn to_model(msg: &Message) -> MessageModel {
        MessageModel {
            id: msg.id.to_string().into(),
            sender_id: msg.sender_id.to_string().into(),
            content: msg.content.text().into(),
            timestamp: format_time(msg.created_at).into(),
            is_edited: msg.is_edited,
            is_pinned: msg.is_pinned,
            is_encrypted: msg.is_encrypted,
            reactions: msg.reactions.iter()
                .map(ReactionAdapter::to_model)
                .collect::<Vec<_>>()
                .into(),
            // ...
        }
    }
}
```

### 9. Device Inbox (`src/inbox/`)

Signal-style message queue for receiving messages across devices. All incoming encrypted messages flow through the device inbox.

```rust
// src/inbox/provider.rs
pub struct DeviceInboxProvider {
    client: Arc<MailboxClient>,
    processor: Arc<MessageProcessor>,
    signal_store: Arc<SignalStore>,
    poller: Option<JoinHandle<()>>,
    shutdown: broadcast::Sender<()>,
}

impl DeviceInboxProvider {
    pub fn new(
        client: Arc<MailboxClient>,
        processor: Arc<MessageProcessor>,
        signal_store: Arc<SignalStore>,
    ) -> Self;

    /// Start polling the device inbox
    pub async fn start(&mut self, poll_interval: Duration) -> Result<()>;

    /// Stop polling
    pub async fn stop(&mut self);

    /// Fetch and process all pending messages (called on startup)
    pub async fn fetch_pending(&self) -> Result<usize>;
}

// src/inbox/processor.rs
pub struct MessageProcessor {
    handlers: HashMap<ContentType, Box<dyn MessageHandler>>,
    repo: Arc<MessageRepository>,
    events: EventBus,
}

impl MessageProcessor {
    /// Process a single inbox message, dispatching to appropriate handler
    pub async fn process(&self, envelope: InboxEnvelope) -> Result<()> {
        // 1. Decrypt outer envelope (sealed sender or regular)
        let (sender, content) = self.decrypt_envelope(&envelope).await?;

        // 2. Parse content type
        let content_type = ContentType::from_proto(&content);

        // 3. Dispatch to handler
        if let Some(handler) = self.handlers.get(&content_type) {
            handler.handle(sender, content).await?;
        }

        // 4. Acknowledge to server
        self.acknowledge(envelope.id).await?;

        Ok(())
    }
}

// src/inbox/handlers/mod.rs
#[async_trait]
pub trait MessageHandler: Send + Sync {
    async fn handle(&self, sender: ProtocolAddress, content: Content) -> Result<()>;
}

/// Content types the inbox can receive
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentType {
    ChatMessage,           // Regular encrypted message
    AttachmentPointer,     // File attachment metadata
    Reaction,              // Emoji reaction
    ReadReceipt,           // Read receipt batch
    DeliveryReceipt,       // Delivery confirmation
    TypingIndicator,       // Typing start/stop
    KeyExchange,           // X3DH prekey message
    SenderKeyDistribution, // Group sender key
    PinEvent,              // Pin/unpin message
    EditEvent,             // Message edit
    DeleteEvent,           // Message delete
    SyncMessage,           // Multi-device sync
    CallOffer,             // Incoming call
    CallAnswer,            // Call accepted
    CallIceCandidate,      // WebRTC ICE
}
```

**Inbox Message Flow:**

```
┌──────────────┐    ┌─────────────────┐    ┌─────────────────┐
│    Server    │───>│  DeviceInbox    │───>│   Processor     │
│   (Polling)  │    │   Provider      │    │                 │
└──────────────┘    └─────────────────┘    └────────┬────────┘
                                                    │
                    ┌───────────────────────────────┼───────────────────────────────┐
                    ▼                               ▼                               ▼
          ┌─────────────────┐            ┌─────────────────┐            ┌─────────────────┐
          │  ChatHandler    │            │ ReactionHandler │            │  KeyExchange    │
          │  (decrypt msg)  │            │ (update UI)     │            │  Handler        │
          └────────┬────────┘            └────────┬────────┘            └────────┬────────┘
                   │                              │                              │
                   ▼                              ▼                              ▼
          ┌─────────────────┐            ┌─────────────────┐            ┌─────────────────┐
          │ MessageService  │            │ StreamingState  │            │  SignalStore    │
          │ (store + emit)  │            │ (live update)   │            │ (new session)   │
          └─────────────────┘            └─────────────────┘            └─────────────────┘
```

### 10. Ephemeral Mailbox System (`src/mailbox/`)

Privacy-preserving message routing based on NDSS '21 research. Prevents server from learning communication patterns through Statistical Disclosure Attacks (SDAs).

**Core Concept:** Instead of sending to long-term identities (user IDs), messages are sent to **ephemeral anonymous mailboxes**. The server cannot correlate who is talking to whom.

```rust
// src/mailbox/provider.rs
pub struct MailboxProvider {
    client: Arc<MailboxClient>,
    token_manager: Arc<TokenManager>,
    dm_manager: Arc<DmMailboxManager>,
    group_manager: Arc<GroupMailboxManager>,
    cover_traffic: Option<JoinHandle<()>>,
}

impl MailboxProvider {
    /// Get or create mailbox for a DM conversation
    pub async fn get_dm_mailbox(&self, peer_id: UserId) -> Result<MailboxAddress>;

    /// Get or create mailbox for a group (our mailbox in that group)
    pub async fn get_group_mailbox(&self, group_id: ChannelId) -> Result<MailboxAddress>;

    /// Get all peer mailboxes for group fanout
    pub async fn get_group_peer_mailboxes(
        &self,
        group_id: ChannelId,
    ) -> Result<Vec<(UserId, MailboxAddress)>>;

    /// Start cover traffic (optional privacy enhancement)
    pub async fn start_cover_traffic(&mut self, interval: Duration);
}

// src/mailbox/token_manager.rs
/// Manages blind-signed tokens for anonymous mailbox creation
/// Users obtain tokens daily using long-term identity, spend them anonymously
pub struct TokenManager {
    db: Arc<Database>,
    client: Arc<AuthClient>,
    tokens: RwLock<Vec<BlindToken>>,
}

impl TokenManager {
    /// Request new batch of blind-signed tokens (authenticated)
    /// Server signs without knowing which mailboxes they'll open
    pub async fn request_tokens(&self, count: usize) -> Result<()> {
        // 1. Generate random tokens
        let tokens: Vec<_> = (0..count)
            .map(|_| BlindToken::generate())
            .collect();

        // 2. Blind them (hide actual value from server)
        let blinded: Vec<_> = tokens.iter()
            .map(|t| t.blind())
            .collect();

        // 3. Request signatures (authenticated with long-term identity)
        let signatures = self.client.sign_tokens(blinded).await?;

        // 4. Unblind signatures
        for (token, sig) in tokens.iter_mut().zip(signatures) {
            token.unblind_signature(sig);
        }

        // 5. Store for later use
        self.store_tokens(tokens).await?;

        Ok(())
    }

    /// Spend a token to create an anonymous mailbox
    pub async fn spend_token(&self) -> Result<(BlindToken, Signature)> {
        let mut tokens = self.tokens.write().await;
        tokens.pop().ok_or(Error::NoTokensAvailable)
    }

    /// Check if we need more tokens (< 5 remaining)
    pub fn needs_replenish(&self) -> bool;
}

// src/mailbox/blind_signature.rs
/// Chaum RSA Blind Signature implementation
pub struct BlindToken {
    pub value: [u8; 32],
    pub blinding_factor: BigUint,
    pub signature: Option<Vec<u8>>,
}

impl BlindToken {
    /// Generate fresh random token
    pub fn generate() -> Self;

    /// Blind the token before sending to server
    pub fn blind(&self) -> BlindedToken {
        // m' = m * r^e mod n (RSA blinding)
    }

    /// Unblind the signature after receiving from server
    pub fn unblind_signature(&mut self, blinded_sig: Vec<u8>) {
        // s = s' * r^-1 mod n (RSA unblinding)
    }

    /// Verify the unblinded signature
    pub fn verify(&self, server_public_key: &RsaPublicKey) -> bool;
}
```

**DM Mailbox Flow (Two-Way Anonymity):**

```rust
// src/mailbox/dm/bootstrap.rs
impl DmMailboxManager {
    /// Bootstrap a new DM conversation
    /// 1. Create our ephemeral mailbox
    /// 2. Send mailbox ID to peer via sealed sender (one-time long-term identity use)
    /// 3. Wait for peer's mailbox ID
    /// 4. All future messages use anonymous mailboxes only
    pub async fn bootstrap_conversation(&self, peer_id: UserId) -> Result<DmConversation> {
        // 1. Spend token to create anonymous mailbox
        let (token, sig) = self.token_manager.spend_token().await?;
        let our_mailbox = self.client.create_mailbox(token, sig).await?;

        // 2. Generate ephemeral keypair for this conversation
        let ephemeral_keypair = EphemeralKeyPair::generate();

        // 3. Send our mailbox ID to peer via sealed sender (last use of long-term identity)
        let bootstrap_msg = MailboxBootstrap {
            mailbox_address: our_mailbox.address.clone(),
            ephemeral_public_key: ephemeral_keypair.public_key(),
        };
        self.send_sealed_sender(peer_id, bootstrap_msg).await?;

        // 4. Store conversation state
        let conversation = DmConversation {
            peer_id,
            our_mailbox,
            our_keypair: ephemeral_keypair,
            their_mailbox: None, // Set when they respond
            their_public_key: None,
        };
        self.store_conversation(&conversation).await?;

        Ok(conversation)
    }
}
```

**Group Mailbox Flow (N-Way Fanout):**

```rust
// src/mailbox/group/fanout.rs
impl GroupMailboxManager {
    /// Send message to all group members via their anonymous mailboxes
    pub async fn send_to_group(
        &self,
        group_id: ChannelId,
        content: &[u8],
    ) -> Result<()> {
        // 1. Get all member mailboxes for this group
        let member_mailboxes = self.get_member_mailboxes(group_id).await?;

        // 2. Encrypt with sender key (efficient group encryption)
        let encrypted = self.signal_store
            .encrypt_group_message(group_id.as_bytes(), content)
            .await?;

        // 3. Fan out to each member's group-specific mailbox
        let futures: Vec<_> = member_mailboxes
            .iter()
            .map(|(user_id, mailbox)| {
                self.client.send_to_mailbox(mailbox, &encrypted)
            })
            .collect();

        // 4. Send in parallel
        futures::future::try_join_all(futures).await?;

        Ok(())
    }

    /// Handle new member joining - exchange mailbox IDs
    pub async fn on_member_joined(
        &self,
        group_id: ChannelId,
        new_member: UserId,
    ) -> Result<()> {
        // 1. Create our mailbox for this group if not exists
        let our_mailbox = self.get_or_create_group_mailbox(group_id).await?;

        // 2. Send our mailbox ID to new member (via their device inbox)
        let exchange = GroupMailboxExchange {
            group_id,
            mailbox_address: our_mailbox.address.clone(),
        };
        self.send_via_inbox(new_member, exchange).await?;

        // 3. Wait for their mailbox ID (async, handled by inbox)
        Ok(())
    }
}

// src/mailbox/group/member_mailbox.rs
/// Each user has a unique mailbox per group they're in
/// This prevents the server from correlating group membership
pub struct GroupMemberMailbox {
    pub group_id: ChannelId,
    pub mailbox: MailboxAddress,
    pub keypair: EphemeralKeyPair,
    pub created_at: DateTime<Utc>,
}
```

**Key Rolling (Mailbox Expiration):**

```rust
// src/mailbox/key_rolling.rs
impl KeyRollingService {
    /// Rotate mailbox keys periodically for forward secrecy
    /// Old mailboxes are eventually forgotten by the server
    pub async fn rotate_mailbox(
        &self,
        conversation: &mut DmConversation,
    ) -> Result<()> {
        // 1. Create new mailbox
        let new_mailbox = self.create_anonymous_mailbox().await?;

        // 2. Send new mailbox ID to peer via current mailbox
        self.send_key_rotation(conversation, &new_mailbox).await?;

        // 3. Keep old mailbox active briefly for in-flight messages
        let old_mailbox = std::mem::replace(&mut conversation.our_mailbox, new_mailbox);
        self.schedule_mailbox_deletion(old_mailbox, Duration::from_secs(3600)).await?;

        Ok(())
    }
}
```

**Cover Traffic (Optional Privacy Enhancement):**

```rust
// src/mailbox/cover_traffic.rs
/// Generate fake messages to obscure real traffic patterns
pub struct CoverTrafficGenerator {
    mailboxes: Vec<MailboxAddress>,
    interval: Duration,
}

impl CoverTrafficGenerator {
    /// Send dummy messages at random intervals to prevent timing analysis
    pub async fn run(&self, mut shutdown: broadcast::Receiver<()>) {
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                _ = tokio::time::sleep(self.random_interval()) => {
                    // Send fake message to random mailbox
                    if let Some(mailbox) = self.mailboxes.choose(&mut rand::thread_rng()) {
                        let _ = self.send_dummy(mailbox).await;
                    }
                }
            }
        }
    }
}
```

---

## Data Flow

### Sending a Message

```
┌─────────┐    ┌───────────┐    ┌─────────────────┐    ┌──────────────┐
│   UI    │───>│  Command  │───>│ MessageService  │───>│ MessageRepo  │
│ (Slint) │    │ Dispatcher│    │                 │    │   (Local)    │
└─────────┘    └───────────┘    └────────┬────────┘    └──────────────┘
                                         │
                                         ▼
                                ┌─────────────────┐
                                │   SignalStore   │
                                │   (Encrypt)     │
                                └────────┬────────┘
                                         │
                                         ▼
                                ┌─────────────────┐    ┌──────────────┐
                                │ MessageClient   │───>│    Server    │
                                │    (gRPC)       │    │    (gRPC)    │
                                └────────┬────────┘    └──────────────┘
                                         │
                                         ▼
                                ┌─────────────────┐
                                │   EventBus      │
                                │ (MessageSent)   │
                                └────────┬────────┘
                                         │
                                         ▼
                                ┌─────────────────┐    ┌──────────────┐
                                │  MessageState   │───>│     UI       │
                                │   (Update)      │    │   (Render)   │
                                └─────────────────┘    └──────────────┘
```

### Receiving a Message (Stream)

```
┌──────────────┐    ┌─────────────────┐    ┌─────────────────┐
│    Server    │───>│  StreamManager  │───>│ MessageStream   │
│    (gRPC)    │    │                 │    │   (Handler)     │
└──────────────┘    └─────────────────┘    └────────┬────────┘
                                                    │
                                                    ▼
                                           ┌─────────────────┐
                                           │   SignalStore   │
                                           │   (Decrypt)     │
                                           └────────┬────────┘
                                                    │
                                                    ▼
                                           ┌─────────────────┐
                                           │ MessageService  │
                                           │ (Process)       │
                                           └────────┬────────┘
                                                    │
                              ┌─────────────────────┼─────────────────────┐
                              ▼                     ▼                     ▼
                    ┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐
                    │ MessageRepo     │   │  MessageState   │   │   EventBus      │
                    │ (Persist)       │   │  (Update)       │   │ (Notify)        │
                    └─────────────────┘   └────────┬────────┘   └────────┬────────┘
                                                   │                     │
                                                   ▼                     ▼
                                          ┌─────────────────┐   ┌─────────────────┐
                                          │    UI Bridge    │   │ Notification    │
                                          │    (Render)     │   │ Service         │
                                          └─────────────────┘   └─────────────────┘
```

### Sending DM via Ephemeral Mailbox

```
┌─────────┐    ┌───────────────┐    ┌─────────────────┐    ┌──────────────────┐
│   UI    │───>│ MessageService│───>│ MailboxProvider │───>│ DmMailboxManager │
└─────────┘    └───────────────┘    └─────────────────┘    └────────┬─────────┘
                                                                    │
                                    ┌───────────────────────────────┘
                                    ▼
                           ┌─────────────────┐
                           │  Get/Bootstrap  │
                           │  Conversation   │
                           └────────┬────────┘
                                    │
           ┌────────────────────────┼────────────────────────┐
           │ (First message)        │                        │ (Established)
           ▼                        │                        ▼
  ┌─────────────────┐              │               ┌─────────────────┐
  │ Spend BlindToken│              │               │ Get Peer        │
  │ Create Mailbox  │              │               │ Mailbox Address │
  └────────┬────────┘              │               └────────┬────────┘
           │                        │                        │
           ▼                        │                        ▼
  ┌─────────────────┐              │               ┌─────────────────┐
  │ Send Bootstrap  │              │               │  SignalStore    │
  │ via SealedSender│              │               │  (Encrypt)      │
  └────────┬────────┘              │               └────────┬────────┘
           │                        │                        │
           └────────────────────────┼────────────────────────┘
                                    ▼
                           ┌─────────────────┐    ┌──────────────────┐
                           │  MailboxClient  │───>│ Server (Mailbox) │
                           │  (Anonymous)    │    │ No user identity │
                           └─────────────────┘    └──────────────────┘
```

### Sending Group Message via Mailbox Fanout

```
┌─────────┐    ┌───────────────┐    ┌─────────────────┐
│   UI    │───>│ MessageService│───>│ MailboxProvider │
└─────────┘    └───────────────┘    └────────┬────────┘
                                             │
                                             ▼
                                    ┌─────────────────┐
                                    │GroupMailboxMgr  │
                                    │ get_member_     │
                                    │ mailboxes()     │
                                    └────────┬────────┘
                                             │
                                             ▼
                                    ┌─────────────────┐
                                    │  SignalStore    │
                                    │ encrypt_group() │
                                    │ (Sender Key)    │
                                    └────────┬────────┘
                                             │
              ┌──────────────────────────────┼──────────────────────────────┐
              ▼                              ▼                              ▼
     ┌─────────────────┐           ┌─────────────────┐           ┌─────────────────┐
     │ Member A        │           │ Member B        │           │ Member N        │
     │ Mailbox (anon)  │           │ Mailbox (anon)  │           │ Mailbox (anon)  │
     └────────┬────────┘           └────────┬────────┘           └────────┬────────┘
              │                              │                              │
              └──────────────────────────────┼──────────────────────────────┘
                                             ▼
                                    ┌─────────────────┐
                                    │ Server sees N   │
                                    │ unrelated anon  │
                                    │ mailbox writes  │
                                    └─────────────────┘
```

### Blind Token Acquisition (Daily)

```
┌─────────────────┐    ┌─────────────────┐    ┌──────────────────┐
│  TokenManager   │───>│  AuthClient     │───>│ Server (Auth)    │
│  (Authenticated)│    │  (Long-term ID) │    │                  │
└─────────────────┘    └─────────────────┘    └────────┬─────────┘
                                                       │
        ┌──────────────────────────────────────────────┘
        │  1. Generate random tokens locally
        │  2. Blind tokens (hide actual values)
        │  3. Send blinded tokens to server
        │  4. Server signs WITHOUT seeing actual values
        │  5. Receive blinded signatures
        │  6. Unblind signatures locally
        │  7. Store (token, signature) pairs
        ▼
┌─────────────────┐
│ Local Storage   │  Tokens can now be spent ANONYMOUSLY
│ (Encrypted)     │  to create mailboxes - server cannot
│ [token, sig]×10 │  correlate token spend to user identity
└─────────────────┘
```

---

## Additional Feature Components

### 11. Ephemeral Messages (`src/services/ephemeral_message.rs`)

Self-destructing messages with configurable TTL.

```rust
// TTL options matching Signal/Telegram
pub enum MessageTTL {
    Seconds30,
    Minutes1,
    Minutes5,
    Hours1,
    Hours24,
    Custom(Duration),
}

pub struct EphemeralMessageService {
    repo: Arc<MessageRepository>,
    timer_manager: Arc<TimerManager>,
    events: EventBus,
}

impl EphemeralMessageService {
    /// Send message with self-destruct timer
    pub async fn send_ephemeral(
        &self,
        channel_id: ChannelId,
        content: String,
        ttl: MessageTTL,
    ) -> Result<Message> {
        let message = Message::new_ephemeral(channel_id, content, ttl);

        // Store with expiration timestamp
        self.repo.insert(&message).await?;

        // Schedule deletion timer
        self.timer_manager.schedule_deletion(
            message.id,
            message.expires_at.unwrap(),
        ).await;

        Ok(message)
    }

    /// Start countdown when message is viewed
    pub async fn start_countdown(&self, message_id: MessageId) -> Result<()> {
        let message = self.repo.get(message_id).await?;
        if let Some(ttl) = message.ttl {
            let expires_at = Utc::now() + ttl;
            self.repo.set_expires_at(message_id, expires_at).await?;
            self.timer_manager.schedule_deletion(message_id, expires_at).await;
            self.events.emit(AppEvent::EphemeralCountdownStarted(message_id, expires_at));
        }
        Ok(())
    }
}
```

### 12. Draft Service (`src/services/draft_service.rs`)

Auto-save drafts per channel and thread.

```rust
pub struct DraftService {
    repo: Arc<DraftRepository>,
    debouncer: Debouncer,
}

impl DraftService {
    /// Auto-save draft (debounced, 500ms)
    pub fn save_draft(&self, channel_id: ChannelId, thread_id: Option<MessageId>, content: String) {
        let key = DraftKey { channel_id, thread_id };
        self.debouncer.debounce(key.clone(), Duration::from_millis(500), move || {
            self.repo.upsert(Draft {
                key,
                content,
                updated_at: Utc::now(),
            })
        });
    }

    /// Get draft for channel/thread
    pub async fn get_draft(&self, channel_id: ChannelId, thread_id: Option<MessageId>) -> Option<Draft> {
        self.repo.get(&DraftKey { channel_id, thread_id }).await.ok()
    }

    /// Clear draft after sending
    pub async fn clear_draft(&self, channel_id: ChannelId, thread_id: Option<MessageId>) -> Result<()> {
        self.repo.delete(&DraftKey { channel_id, thread_id }).await
    }
}
```

### 13. Pinned Messages Bar (`src/state/pinned_state.rs`)

Telegram-style pinned bar with navigation.

```rust
pub struct PinnedBarState {
    /// All pinned messages for current channel (ordered by pin time)
    pub pinned_messages: Vec<Message>,
    /// Currently displayed pin index
    pub current_index: usize,
    /// Whether bar is dismissed (per session)
    pub dismissed: bool,
}

impl PinnedBarState {
    pub fn next_pin(&mut self) {
        if !self.pinned_messages.is_empty() {
            self.current_index = (self.current_index + 1) % self.pinned_messages.len();
        }
    }

    pub fn prev_pin(&mut self) {
        if !self.pinned_messages.is_empty() {
            self.current_index = if self.current_index == 0 {
                self.pinned_messages.len() - 1
            } else {
                self.current_index - 1
            };
        }
    }

    pub fn current_pin(&self) -> Option<&Message> {
        self.pinned_messages.get(self.current_index)
    }

    pub fn pin_count(&self) -> usize {
        self.pinned_messages.len()
    }
}
```

### 14. Saved Messages (`src/services/saved_message_service.rs`)

Personal message vault (like Telegram's Saved Messages).

```rust
pub const SAVED_CHANNEL_ID: &str = "__saved__";

pub struct SavedMessageService {
    repo: Arc<SavedMessageRepository>,
    crypto: Arc<SignalStore>,
}

impl SavedMessageService {
    /// Save content to personal vault (encrypted locally)
    pub async fn save(&self, content: SavedContent) -> Result<SavedMessage> {
        let encrypted = self.crypto.encrypt_local(&content)?;
        let message = SavedMessage {
            id: MessageId::new(),
            content: encrypted,
            created_at: Utc::now(),
            is_pinned: false,
        };
        self.repo.insert(&message).await?;
        Ok(message)
    }

    /// List saved messages with pagination
    pub async fn list(&self, cursor: Option<MessageId>, limit: usize) -> Result<Vec<SavedMessage>> {
        self.repo.list(cursor, limit).await
    }

    /// Pin/unpin saved message
    pub async fn set_pinned(&self, id: MessageId, pinned: bool) -> Result<()> {
        self.repo.set_pinned(id, pinned).await
    }
}
```

### 15. Forward Service (`src/services/forward_service.rs`)

Forward messages to channels, DMs, or AI bots.

```rust
pub enum ForwardTarget {
    Channel(ChannelId),
    DirectMessage(UserId),
    AIBot(IntegrationId),
    SavedMessages,
}

pub struct ForwardService {
    message_service: Arc<MessageService>,
    ai_service: Arc<AIService>,
    saved_service: Arc<SavedMessageService>,
}

impl ForwardService {
    pub async fn forward(
        &self,
        message: &Message,
        targets: Vec<ForwardTarget>,
    ) -> Result<Vec<ForwardResult>> {
        let mut results = Vec::new();

        for target in targets {
            let result = match target {
                ForwardTarget::Channel(id) => {
                    self.message_service.send_forwarded(id, message).await
                }
                ForwardTarget::DirectMessage(user_id) => {
                    let dm = self.get_or_create_dm(user_id).await?;
                    self.message_service.send_forwarded(dm.id, message).await
                }
                ForwardTarget::AIBot(integration_id) => {
                    self.ai_service.send_forwarded(integration_id, message).await
                }
                ForwardTarget::SavedMessages => {
                    self.saved_service.save(message.into()).await
                }
            };
            results.push(result);
        }

        Ok(results)
    }
}
```

### 16. Local AI Service (`src/local_ai/`)

Support for Ollama, vLLM, and MLC-LLM backends.

```rust
// src/local_ai/backend.rs
#[async_trait]
pub trait LocalAIBackend: Send + Sync {
    async fn chat(&self, messages: &[AIMessage]) -> Result<String>;
    async fn chat_stream(&self, messages: &[AIMessage]) -> Result<impl Stream<Item = String>>;
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    async fn health_check(&self) -> Result<bool>;
}

// src/local_ai/ollama.rs
pub struct OllamaBackend {
    client: reqwest::Client,
    base_url: String,
    chat_model: String,     // default: llama3.2:3b
    embed_model: String,    // default: nomic-embed-text
}

impl LocalAIBackend for OllamaBackend {
    async fn chat(&self, messages: &[AIMessage]) -> Result<String> {
        let resp = self.client.post(format!("{}/api/chat", self.base_url))
            .json(&OllamaRequest {
                model: &self.chat_model,
                messages,
                stream: false,
            })
            .send()
            .await?;
        Ok(resp.json::<OllamaResponse>().await?.message.content)
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let resp = self.client.post(format!("{}/api/embeddings", self.base_url))
            .json(&EmbedRequest {
                model: &self.embed_model,
                prompt: text,
            })
            .send()
            .await?;
        Ok(resp.json::<EmbedResponse>().await?.embedding)
    }
}

// src/local_ai/model_manager.rs
pub struct ModelManager {
    backend: Arc<dyn LocalAIBackend>,
    settings: Arc<LocalAISettings>,
}

impl ModelManager {
    pub async fn download_model(&self, model_name: &str) -> Result<DownloadProgress>;
    pub async fn delete_model(&self, model_name: &str) -> Result<()>;
    pub async fn switch_chat_model(&self, model_name: &str) -> Result<()>;
    pub async fn switch_embed_model(&self, model_name: &str) -> Result<()>;
}
```

### 17. Semantic Search (`src/services/semantic_search.rs`)

Embedding-based message search using local AI.

```rust
pub struct SemanticSearchService {
    vector_store: Arc<VectorStore>,
    embedder: Arc<dyn LocalAIBackend>,
    indexer: Arc<MessageIndexer>,
}

impl SemanticSearchService {
    /// Search messages by semantic similarity
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        // 1. Embed the query
        let query_embedding = self.embedder.embed(query).await?;

        // 2. Find similar messages
        let results = self.vector_store
            .search(&query_embedding, limit)
            .await?;

        Ok(results)
    }

    /// Index a message (called automatically on receive)
    pub async fn index_message(&self, message: &Message) -> Result<()> {
        let embedding = self.embedder.embed(&message.content.text()).await?;
        self.vector_store.insert(message.id, &embedding).await
    }
}

// src/local_ai/vector_store.rs
pub struct VectorStore {
    db: Arc<Database>,
}

impl VectorStore {
    /// Find K nearest neighbors using cosine similarity
    pub async fn search(&self, query: &[f32], k: usize) -> Result<Vec<SearchResult>> {
        // SQLite with vec0 extension or in-memory HNSW
        self.db.query_raw(
            "SELECT message_id, vec_distance_cosine(embedding, ?) as distance
             FROM message_embeddings
             ORDER BY distance ASC LIMIT ?",
            (query, k)
        ).await
    }
}
```

### 18. Mention Service (`src/services/mention_service.rs`)

Parse and resolve @mentions.

```rust
pub enum MentionType {
    User(UserId),
    Channel(ChannelId),
    Here,  // @here - all online
    All,   // @all - everyone
}

pub struct MentionService {
    user_repo: Arc<UserRepository>,
    channel_repo: Arc<ChannelRepository>,
}

impl MentionService {
    /// Parse mentions from message content
    pub fn parse_mentions(&self, content: &str) -> Vec<Mention> {
        let mut mentions = Vec::new();
        let regex = Regex::new(r"@(\w+)").unwrap();

        for cap in regex.captures_iter(content) {
            let name = &cap[1];
            if name == "here" {
                mentions.push(Mention::Here);
            } else if name == "all" {
                mentions.push(Mention::All);
            } else {
                // Resolve user/channel by name
                if let Some(user) = self.resolve_user(name) {
                    mentions.push(Mention::User(user.id));
                } else if let Some(channel) = self.resolve_channel(name) {
                    mentions.push(Mention::Channel(channel.id));
                }
            }
        }
        mentions
    }

    /// Get autocomplete suggestions
    pub async fn autocomplete(&self, query: &str, channel_id: ChannelId) -> Vec<MentionSuggestion> {
        let mut suggestions = Vec::new();

        // Get channel members matching query
        let members = self.channel_repo.search_members(channel_id, query, 10).await?;
        for member in members {
            suggestions.push(MentionSuggestion::User(member));
        }

        // Add @here and @all if matching
        if "here".starts_with(query) {
            suggestions.push(MentionSuggestion::Here);
        }
        if "all".starts_with(query) {
            suggestions.push(MentionSuggestion::All);
        }

        suggestions
    }
}
```

### 19. Session Management (`src/services/session_service.rs`)

List and revoke active sessions.

```rust
pub struct Session {
    pub id: SessionId,
    pub device_name: String,
    pub device_type: DeviceType,
    pub ip_address: String,
    pub last_active: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub is_current: bool,
}

pub struct SessionService {
    client: Arc<AuthClient>,
    keyring: Arc<Keyring>,
}

impl SessionService {
    /// List all active sessions
    pub async fn list_sessions(&self) -> Result<Vec<Session>> {
        self.client.list_sessions().await
    }

    /// Revoke a specific session
    pub async fn revoke_session(&self, session_id: SessionId) -> Result<()> {
        self.client.revoke_session(session_id).await
    }

    /// Revoke all sessions except current
    pub async fn revoke_all_other_sessions(&self) -> Result<()> {
        self.client.revoke_all_other_sessions().await
    }
}
```

### 20. Privacy Settings (`src/state/privacy_state.rs`)

Per-conversation privacy toggles.

```rust
pub struct ConversationPrivacySettings {
    pub channel_id: ChannelId,
    pub typing_indicators_enabled: bool,  // default: true
    pub read_receipts_enabled: bool,       // default: true
}

pub struct PrivacySettingsService {
    repo: Arc<SettingsRepository>,
}

impl PrivacySettingsService {
    /// Get privacy settings for a conversation
    pub async fn get(&self, channel_id: ChannelId) -> ConversationPrivacySettings {
        self.repo.get_privacy(channel_id).await
            .unwrap_or_default()
    }

    /// Toggle typing indicators for a conversation
    pub async fn set_typing_enabled(&self, channel_id: ChannelId, enabled: bool) -> Result<()> {
        self.repo.set_typing_enabled(channel_id, enabled).await
    }

    /// Toggle read receipts for a conversation
    pub async fn set_receipts_enabled(&self, channel_id: ChannelId, enabled: bool) -> Result<()> {
        self.repo.set_receipts_enabled(channel_id, enabled).await
    }
}
```

### 21. Connection Recovery (`src/network/recovery.rs`)

Automatic reconnection with exponential backoff.

```rust
pub struct ConnectionRecovery {
    state: Arc<RwLock<ConnectionState>>,
    config: RecoveryConfig,
    events: EventBus,
}

pub struct RecoveryConfig {
    pub initial_delay: Duration,      // 1s
    pub max_delay: Duration,          // 30s
    pub multiplier: f64,              // 2.0
    pub jitter: f64,                  // 0.1
    pub max_attempts: Option<usize>,  // None = unlimited
}

impl ConnectionRecovery {
    pub async fn start_recovery_loop(&self, connect_fn: impl Fn() -> Future<Output = Result<()>>) {
        let mut attempt = 0;
        let mut delay = self.config.initial_delay;

        loop {
            self.events.emit(AppEvent::Reconnecting { attempt });

            match connect_fn().await {
                Ok(_) => {
                    self.events.emit(AppEvent::Connected);
                    return;
                }
                Err(e) => {
                    attempt += 1;

                    if let Some(max) = self.config.max_attempts {
                        if attempt >= max {
                            self.events.emit(AppEvent::ConnectionFailed(e));
                            return;
                        }
                    }

                    // Add jitter
                    let jittered = delay.mul_f64(1.0 + rand::random::<f64>() * self.config.jitter);
                    tokio::time::sleep(jittered).await;

                    // Exponential backoff
                    delay = std::cmp::min(
                        delay.mul_f64(self.config.multiplier),
                        self.config.max_delay,
                    );
                }
            }
        }
    }
}
```

### 22. Message Validation Pipeline (`src/network/validation/`)

Admission control for incoming messages.

```rust
// src/network/validation/mod.rs
pub struct MessageValidationPipeline {
    orderer: MessageOrderer,
    deduplicator: MessageDeduplicator,
    validator: ContentValidator,
}

impl MessageValidationPipeline {
    pub async fn process(&self, envelope: InboxEnvelope) -> Result<Option<ValidatedMessage>> {
        // 1. Check for duplicates (by GUID)
        if self.deduplicator.is_duplicate(&envelope.guid) {
            return Ok(None);
        }

        // 2. Validate content
        self.validator.validate(&envelope)?;

        // 3. Order message (wait for dependencies if needed)
        let ordered = self.orderer.order(envelope).await?;

        // 4. Mark as processed
        self.deduplicator.mark_processed(&ordered.guid);

        Ok(Some(ordered))
    }
}

// src/network/validation/deduplicator.rs
pub struct MessageDeduplicator {
    /// Bounded set of recently processed message GUIDs
    processed: RwLock<BoundedHashSet<Uuid>>,
}

impl MessageDeduplicator {
    pub fn new(capacity: usize) -> Self {
        Self {
            processed: RwLock::new(BoundedHashSet::new(capacity)),
        }
    }

    pub fn is_duplicate(&self, guid: &Uuid) -> bool {
        self.processed.read().contains(guid)
    }

    pub fn mark_processed(&self, guid: &Uuid) {
        self.processed.write().insert(*guid);
    }
}
```

---

## Initialization Sequence

```rust
// src/app/bootstrap.rs
pub async fn bootstrap() -> Result<App> {
    // 1. Load configuration
    let config = Config::load()?;

    // 2. Initialize logging
    init_tracing(&config)?;

    // 3. Setup directories
    let dirs = Directories::new()?;

    // 4. Initialize database (get passphrase from keyring or user)
    let db = Database::open(&dirs.data_dir.join("chatter.db"), &passphrase).await?;
    db.run_migrations().await?;

    // 5. Initialize crypto
    let signal_store = SignalStore::new(db.clone()).await?;

    // 6. Initialize repositories
    let repos = Repositories::new(db.clone());

    // 7. Initialize network clients
    let grpc_pool = GrpcClientPool::new(&config.server_url);
    let clients = Clients::new(grpc_pool.clone());

    // 8. Initialize event bus
    let events = EventBus::new();

    // 9. Initialize state stores
    let state = AppState::new();

    // 10. Initialize services
    let services = Services::new(
        repos.clone(),
        clients.clone(),
        signal_store.clone(),
        events.clone(),
        state.clone(),
    );

    // 11. Initialize stream manager
    let streams = StreamManager::new(clients.clone(), services.clone());

    // 12. Initialize system integration
    let system = SystemIntegration::new(&config)?;

    // 13. Create Slint UI
    let ui = MainWindow::new()?;

    // 14. Setup UI bridge
    let bridge = UiBridge::new(&ui, state.clone());
    bridge.bind_state();
    bridge.setup_callbacks();

    // 15. Try auto-login
    if let Some(session) = services.auth.try_restore_session().await? {
        streams.start_all(session.user_id).await?;
    }

    Ok(App {
        ui,
        services,
        streams,
        system,
        state,
        events,
    })
}
```

---

## Error Handling Strategy

```rust
// src/error/app_error.rs
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    // Network errors (recoverable)
    #[error("Network unavailable")]
    Offline,
    #[error("Connection timeout")]
    Timeout,
    #[error("Server error: {0}")]
    Server(String),

    // Auth errors
    #[error("Session expired")]
    SessionExpired,
    #[error("Invalid credentials")]
    InvalidCredentials,

    // Crypto errors
    #[error("Encryption failed: {0}")]
    Encryption(#[from] CryptoError),
    #[error("Decryption failed: {0}")]
    Decryption(String),
    #[error("Key not found for {0}")]
    MissingKey(String),

    // Storage errors
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    // Domain errors
    #[error("Not found: {0}")]
    NotFound(String),
    #[error("Permission denied")]
    PermissionDenied,
    #[error("Validation failed: {0}")]
    Validation(String),
}

impl AppError {
    pub fn is_recoverable(&self) -> bool {
        matches!(self,
            AppError::Offline |
            AppError::Timeout |
            AppError::Server(_)
        )
    }

    pub fn is_offline(&self) -> bool {
        matches!(self, AppError::Offline)
    }
}

// src/error/recovery.rs
pub struct ErrorRecovery;

impl ErrorRecovery {
    pub async fn handle(error: AppError, context: &RecoveryContext) -> RecoveryAction {
        match error {
            AppError::Offline => RecoveryAction::QueueForRetry,
            AppError::Timeout => RecoveryAction::RetryWithBackoff,
            AppError::SessionExpired => RecoveryAction::Reauthenticate,
            AppError::MissingKey(addr) => RecoveryAction::FetchKeys(addr),
            _ => RecoveryAction::ShowError(error.to_string()),
        }
    }
}
```

---

## Testing Strategy

```rust
// Unit tests for domain
#[cfg(test)]
mod tests {
    #[test]
    fn message_encryption_roundtrip() {
        // Test message can be encrypted and decrypted
    }
}

// Integration tests with mock server
#[cfg(test)]
mod integration {
    #[tokio::test]
    async fn send_message_online() {
        let mock_server = MockGrpcServer::start().await;
        let service = create_test_service(&mock_server);

        let msg = service.send_message(/*...*/).await.unwrap();
        assert!(msg.id.is_some());
    }

    #[tokio::test]
    async fn send_message_offline_queues() {
        let service = create_offline_service();

        let msg = service.send_message(/*...*/).await.unwrap();
        assert!(service.state.pending.contains(&msg.id));
    }
}
```

---

## Feature Checklist Mapping

| Feature Category | Implementation Location |
|-----------------|------------------------|
| Core Messaging | `services/message_service.rs`, `domain/message.rs` |
| Channels | `services/channel_service.rs`, `domain/channel.rs` |
| Encryption | `crypto/signal/*`, `crypto/sealed_sender.rs` |
| File Handling | `services/attachment_service.rs`, `media/*` |
| Calls | `services/call_service.rs`, `domain/call.rs` |
| Real-time | `network/streaming/*`, `state/streaming_state.rs` |
| AI Integration | `services/ai_service.rs`, `domain/ai.rs` |
| Search | `services/search_service.rs`, `storage/search_index.rs` |
| Notifications | `services/notification_service.rs`, `system/notifications.rs` |
| Multi-device | `sync/*`, `services/sync_service.rs` |
| Moderation | `services/moderation_service.rs`, `domain/moderation.rs` |
| Settings | `storage/repositories/settings_repo.rs`, `state/ui_state.rs` |
| System Integration | `system/*` |

---

## Performance Considerations

1. **Message Virtualization**: Only load visible messages + buffer
2. **Lazy Decryption**: Decrypt messages on-demand, cache plaintext in memory only
3. **Connection Pooling**: Reuse gRPC channels
4. **Batch Operations**: Batch profile fetches, read receipts
5. **Bounded Caches**: LRU caches with size limits
6. **Efficient Serialization**: Use protobuf for network, bincode for local cache
7. **Background Sync**: Sync in background task, don't block UI

---

## Security Considerations

1. **Memory Safety**: Rust's ownership prevents use-after-free
2. **Encrypted Storage**: All local data encrypted with SQLCipher
3. **Secure Key Storage**: Use system keyring for master password
4. **Zero-Copy Decryption**: Decrypt directly into secure memory when possible
5. **Constant-Time Comparison**: Use `subtle` crate for crypto comparisons
6. **No Plaintext Logging**: Never log message content or keys
7. **Secure Random**: Use `rand` with OS entropy
