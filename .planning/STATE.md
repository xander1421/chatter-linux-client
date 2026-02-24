# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-02-03)

**Core value:** True native experience with full Signal Protocol encryption — replacing Flutter/React with a first-class Linux citizen
**Current focus:** Phase 71 — Complete UI Callback Wiring

## Current Position

Phase: 76 of 76 (Refactor Large Service Files)
Plan: 1 of 4 in current phase (76-02-PLAN completed)
Status: In progress
Last activity: 2026-02-24 — Completed 76-02-PLAN.md (inbox_events modularization)

Progress: ██████████████████████████████████████████████████████████████████████████ 100%

## Performance Metrics

**Velocity:**
- Total plans completed: 80
- Average duration: ~4.7 min
- Total execution time: ~379 min

**By Phase:**

| Phase | Plans | Total | Avg/Plan |
|-------|-------|-------|----------|
| 1 | 1 | ~30m | ~30m |
| 2 | 2 | ~10m | ~5m |
| 3 | 2 | ~12m | ~6m |
| 4 | 2 | ~10m | ~5m |
| 5 | 2 | ~4m | ~2m |
| 6 | 3 | ~11m | ~4m |
| 7 | 4 | ~12m | ~3m |
| 8 | 1 | ~3m | ~3m |
| 9 | 1 | ~2m | ~2m |
| 10 | 1 | ~4m | ~4m |
| 11 | 1 | ~4m | ~4m |
| 12 | 1 | ~3m | ~3m |
| 13 | 1 | ~3m | ~3m |
| 14 | 1 | ~4m | ~4m |
| 15 | 1 | ~12m | ~12m |
| 16 | 2 | ~9m | ~4.5m |
| 17 | 3 | ~14m | ~5m |
| 18 | 2 | ~6m | ~3m |
| 19 | 4 | ~11m | ~3m |
| 20 | 4 | ~11m | ~3m |
| 21 | 3 | ~12m | ~4m |
| 22 | 2 | ~9m | ~4.5m |
| 23 | 2 | ~8m | ~4m |
| 24 | 3 | ~26m | ~9m |
| 25 | 2 | ~20m | ~10m |
| 26 | 1 | ~15m | ~15m |
| 27 | 3 | ~21m | ~7m |
| 28 | 3 | ~12m | ~4m |
| 29 | 3 | ~7m | ~2.3m |
| 30 | 2 | ~7m | ~3.5m |
| 31 | 3 | ~7m | ~2.3m |
| 32 | 2 | ~10m | ~5m |
| 33 | 2 | ~4m | ~2m |
| 34 | 2 | ~6m | ~3m |
| 35 | 3 | ~20m | ~7m |
| 36 | 3 | ~19m | ~6m |

| 37 | 1 | ~5m | ~5m |
| 38 | 1 | ~5m | ~5m |
| 39 | 1 | ~5m | ~5m |
| 40 | 1 | ~6m | ~6m |
| 41 | 1 | ~3m | ~3m |
| 42 | 1 | ~5m | ~5m |
| 43 | 2 | ~8m | ~4m |
| 44 | 1 | ~3m | ~3m |
| 45 | 1 | ~4m | ~4m |
| 46 | 1 | ~4m | ~4m |
| 47 | 1 | ~4m | ~4m |
| 48 | 1 | ~18m | ~18m |
| 49 | 1 | ~2m | ~2m |
| 50 | 1 | ~3m | ~3m |
| 51 | 1 | ~5m | ~5m |
| 52 | 1 | ~7m | ~7m |
| 53 | 1 | ~4m | ~4m |

| 54 | 1 | ~5m | ~5m |
| 55 | 1 | ~4m | ~4m |
| 56 | 2 | ~12m | ~6m |
| 57 | 1 | ~4m | ~4m |
| 58 | 2 | ~3m | ~1.5m |
| 59 | 2 | ~22m | ~11m |
| 60 | 2 | ~5m | ~2.5m |

| 61 | 2 | ~7m | ~3.5m |
| 62 | 2 | ~5m | ~2.5m |
| 63 | 2 | ~6m | ~3m |
| 64 | 3 | ~13m | ~4.3m |
| 65 | 2 | ~10m | ~5m |
| 66 | 1 | ~1m | ~1m |
| 67 | 3 | ~30m | ~10m |
| 68 | 1 | ~6m | ~6m |

**Recent Trend:**
- Last 5 plans: 67-1 (~25m), 67-2 (~2m), 67-3 (~3m), 68-1 (~6m)
- Trend: Phase 68 complete, build verified clean

## Accumulated Context

### Decisions

Decisions are logged in PROJECT.md Key Decisions table.
Recent decisions affecting current work:

- Slint UI framework committed (no GTK/Qt fallback)
- libsignal-client for Signal Protocol
- SQLCipher for encrypted storage
- Bottom-up build order (storage → crypto → network → services → UI)
- Stub file pattern: doc comment + empty struct
- ID types use macro for DRY implementation
- DeviceId uses u32 not UUID (matches Signal protocol)
- No Default for IDs (prevent uninitialized use)
- sender_id is Option (supports sealed sender)
- ChannelType uses simple enum (no Unspecified variant)
- Permissions uses factory methods for role mapping
- FileCategory.from_mime_type uses liberal contains() checks
- Emoji custom format uses :name: pattern
- NotificationType uses struct variants for rich data
- is_actionable limited to FriendRequest, ChannelInvite, Incoming calls
- CallState::Ended uses struct variant with reason
- Mutes always require expiry duration (unlike permanent bans)
- Used uuid::Uuid for moderation IDs (no special types)
- DraftKey implements Hash/Eq for HashMap keys
- EphemeralMessage countdown starts on read, not send (matches Signal)
- Empty draft detection checks trimmed content AND attachments
- Tag operations use case-insensitive comparison
- Mention extraction returns target_id as None (caller resolves)
- LinkEmbed.source_url() returns String for GitHub URL construction
- YouTube duration uses std::time::Duration (not chrono)
- Single Mutex<Connection> for v1 (can upgrade to pool later)
- cipher_compatibility=4 for SQLCipher 4.x mode
- Verify encryption key on open via cipher_version query
- schema_version managed by MigrationRunner, not individual migrations
- Migrations sorted by version, each in own transaction
- Arc<Database> for repository dependency injection
- Enums as integers in SQLite (PresenceStatus 0-3, FriendshipStatus 0-2)
- While loop for list queries (avoid rusqlite lifetime issues)
- UPSERT for profile updates
- Flatten ChannelConfig in database (no JSON blobs)
- JOIN query for list_for_user (channels via memberships)
- row_to_X helper functions for complex struct reconstruction
- JSON TEXT columns for MessageContent (complex nested type)
- content_type discriminant for filtering without JSON parsing
- Cursor-based pagination using (created_at, id) tuple
- emoji_json in unique constraint (same user, different emoji allowed)
- Toggle pattern for add/remove operations
- COALESCE in unique index for NULL handling (drafts)
- Upsert instead of separate insert/update for key-based entities
- Kyber1024 for PQXDH post-quantum prekeys
- DeviceId uses TryFrom (NonZeroU8 range 1-127)
- Optional one-time prekey in PreKeyBundle (fallback support)
- AES-256-CBC + HMAC-SHA256 for attachments (matches Signal, not AES-GCM)
- Log-bucketing padding to prevent file size inference
- Constant-time comparison for digest and HMAC verification
- parking_lot::RwLock for sync interceptors (tonic needs sync)
- broadcast channel capacity 16 for connection events
- Panic on channel() if not connected (explicit contract)
- StreamManager uses broadcast (capacity 4) for coordinated shutdown
- StreamHandle uses oneshot for individual stream control
- MessageEventRouter uses broadcast capacity 256 for typed event channels
- route() returns bool for success/failure tracking
- PresenceRouter uses broadcast capacity 128 with 1000-entry LRU cache
- TypingAggregator uses HashMap<channel, HashMap<user, Instant>> with 10s timeout
- ReactionEvent wraps proto ReactionUpdate with user_id from stream context
- ReadReceiptAggregator uses high-water mark (only update if newer timestamp)
- FIFO eviction over LRU for BoundedHashSet (simpler, sufficient for 'recently seen')
- Empty author_id allowed in validation (sealed sender per Signal protocol)
- Inbox event channel capacity 64 for message lifecycle monitoring
- Adaptive polling: 5s active, 30s idle, 2-min activity threshold
- Ed25519 challenge-response for mailbox authentication
- ContentType from protobuf wire format (peek first bytes)
- ProcessingError.is_recoverable() for retry decisions
- LocalQueue exponential backoff: base_delay * 2^attempts, capped at 5 min
- SHA256-based deterministic UUIDs for message IDs (no Uuid v5 feature)
- Stateless handlers with HandlerContext for broadcast event emission
- DomainEvent via broadcast channels (capacity 64) for multi-subscriber consumption
- KeyExchange validates 32-byte ephemeral_pk (Ed25519 public key)
- SenderKey passes raw distribution_message bytes to crypto layer
- PinHandler uses local_user_id fallback for sealed sender messages
- blind-rsa-signatures 0.14 for Rust 1.85 compatibility (0.15+ has derive_more issues)
- Ed25519 key generation via rand::fill() to avoid rand_core version conflicts
- Separate Ed25519 and X25519 for DM mailbox: Ed25519 for mailbox identity/auth, X25519 for message encryption (Signal pattern)
- 64-byte Ed25519 private key storage: Full keypair bytes (32 secret + 32 public) for ed25519-dalek compatibility
- 64-byte combined key for DM bootstrap: first 32 = mailbox pk, next 32 = conversation ephemeral pk
- Bootstrap detection via ephemeral_pk length (64 = bootstrap, 32 = regular key exchange)
- GroupMailbox parallels DmMailbox: Ed25519 keypair, RSA signature, expiry tracking
- HashMap<UserId, MemberMailbox> for O(1) member lookup in group state
- futures::future::join_all for parallel fanout (collect all results even with failures)
- FanoutResult builder methods (success/failure) for clearer construction
- 24-hour rotation threshold for mailbox key rolling (rotate when expiring within 24h)
- 1-hour overlap period for old mailboxes (grace period for in-flight messages)
- RwLock<Vec<PendingDeletion>> for async-safe pending deletion tracking
- Exponential distribution for cover traffic timing (more realistic than uniform)
- Callback-based cover traffic send (decouples from network layer)
- Cover traffic disabled by default (user opt-in for privacy vs bandwidth)
- DeviceCredentials uses rand::rng() directly (avoids rand_core version conflicts)
- UAK derivation: HKDF-SHA256, info="UnidentifiedDelivery", 16 bytes output
- SHA256 password hashing for server auth (server expects pre-hashed)
- Fresh profile key per login (UAK derivation)
- Preserve identity keys on logout (support re-login)
- RegistrationResult captures user_id from UserRegistered event stream
- Proto millis timestamps to chrono DateTime via timestamp_millis_opt with fallback
- MessageService takes MessageClient by mutable reference (caller owns connection)
- Soft delete for message deletion (keep for history/sync)
- Server-first fetch for get_messages (always fetch from server, then upsert locally)
- Ownership validation allows edit/delete if sender_id is None (sealed sender)
- Streaming RPC consumption: while let loop with event variant matching
- ChannelService follows MessageService pattern: &mut client, server-first, upsert locally
- FriendRequestFilter enum mirrors proto; respond_friend_request returns Option<Friendship>
- Local query methods skip auth check (get_user_local, get_profile_local, search_users_local)
- Search results NOT stored locally (discovery pattern, not relationships)
- New ReactionId per conversion: proto Reaction lacks ID, generate during conversion (acceptable for display use)
- ReactionEvent re-export: Added from aggregator for convenience
- Local search does not require authentication (querying locally stored data)
- Server search requires authentication, results NOT stored locally (discovery pattern)
- Combined search gracefully degrades (local-only if not authenticated or network fails)
- Max 3 concurrent thread panels enforced in ChatState
- PaginationCursor uses (has_more, oldest_id, oldest_timestamp) tuple
- Navigation wraparound in PinnedState (next after last goes to first)
- UserStateData friends as Vec<UserId> (profiles fetched separately for lazy loading)
- PresenceStateData last_updated for staleness detection
- PrivacyState global defaults apply when no per-conversation override exists
- ConversationPrivacySettings.private() factory for disabling all privacy signals
- StreamingState is simplified UI view, not raw aggregator data
- OfflineState automatically updates last_connected on successful connection
- set_queue_stats syncs total_operations to queue_count for convenience
- Re-export DomainEvent from inbox::handlers instead of duplicating (single source of truth)
- EventBus channel capacities: 64 for domain (burst handling), 32 for UI
- EventHandlers.spawn() for background processing, sync handle_domain_event() for testability
- Use directories crate ProjectDirs (not BaseDirs/UserDirs) for app-specific paths
- Runtime directory is optional (best-effort creation)
- Keyring entries created per-operation (not cached in struct)
- Keyring tests marked #[ignore] for CI environments without Secret Service
- arboard with wayland-data-control for native Wayland clipboard support (auto-fallback to X11)
- ExternalOpener is stateless (struct as namespace, all methods independent)
- AutostartManager uses dirs crate for config dir (independent of Directories struct)
- URL validation limited to http/https/mailto for security
- GIF cache uses simple HashMap with TTL instead of LRU library
- Inline URL encoding for Tenor API (no urlencoding crate)
- Prefer gif format over mediumgif, tinygif over nanogif for quality
- slint::include_modules! in binary (not library) to avoid duplicate types
- UiBridge provides subscribe methods for async state sync
- Main binary owns Slint-generated types and wires callbacks directly
- Avatar presence dot uses background border for contrast
- Button computed bg-color property for variant/state combinations
- Input uses separate placeholder Text element for styling control
- Modal nested TouchAreas for backdrop dismiss vs content click blocking
- Toast 4px left accent border in variant color
- Skeleton shimmer via linear gradient with infinite animation
- EmojiPicker: embedded 700+ static emoji (no external crate), keywords on Rust side for search
- EmojiPicker: Flickable with Math.floor/mod grid positioning for emoji layout
- components/mod.slint centralizes all component exports
- GifPicker: preview_url for thumbnails, full_url for selection (Tenor API)
- MentionAutocomplete: string-based type field for Slint compatibility
- Tenor attribution footer required per API TOS
- Slint animation via states with iteration-count -1 for infinite animation loop
- Cosine function for smooth opacity wave pattern on typing dots
- has-multiple flag for conditional navigation controls
- String template identifiers for channel creation (general, project, announcement, ephemeral)
- Wizard modal pattern: step indicator with numbered circles and connector line
- ForwardTarget struct with target-type string for channel/dm discrimination
- InviteUser struct includes already-member bool for disabled state
- Chips bar uses horizontal Flickable for overflow handling
- Dynamic invite button text shows selection count
- UserProfileModal uses empty title (visual profile header instead)
- Presence color/label helper functions for string-to-color mapping
- Tab bar pattern: equal-width tabs with primary underline for active state
- FriendRequest struct for model arrays in friend request lists
- Toggle component: 44x24px with 20px thumb, 150ms animation
- Settings sidebar: 240px fixed width with vertical navigation
- Inline form pattern: toggle visibility with save/cancel buttons
- SessionInfo struct for session management modal
- Letter-spacing 1px for fingerprint display (Slint lacks monospace font specification)
- Prekey thresholds: green >= 10, yellow 5-9, red < 5
- Toggle row pattern with label/description and aligned toggle
- Auth form validation triggers on submit (not blur)
- Three-dot rotating spinner for LoadingPage (more visual interest)
- SharedString type annotations required for Slint callback parameters
- Create AuthService fresh in each callback (stateless, cheap)
- invoke_from_event_loop for all UI updates from async tasks
- Graceful offline mode if server connection fails

### Deferred Issues

None.

### Blockers/Concerns

None.

### Roadmap Evolution

- Phase 71 added: Complete UI Callback Wiring (wire all remaining Slint callbacks to services)
- Phase 74 added: LiveKit Video & Audio Calls (WebRTC via livekit Rust SDK)
- Phase 75 added: Secure In-Memory Attachment Handling (encrypted DB storage, memory-only decryption)
- Phase 76 added: Refactor Large Service Files (split files >550 lines into focused modules)

## Session Continuity

Last session: 2026-02-24
Stopped at: Completed 76-02-PLAN.md (inbox_events.rs modularization)
Resume file: None
Next action: Execute 76-03-PLAN.md or next plan in phase
