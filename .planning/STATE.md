# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-02-03)

**Core value:** True native experience with full Signal Protocol encryption — replacing Flutter/React with a first-class Linux citizen
**Current focus:** Phase 28 in progress — Group mailbox fanout

## Current Position

Phase: 28 of 66 (Mailbox Group System)
Plan: 2 of 3 in current phase
Status: In progress
Last activity: 2026-02-04 — Completed 28-02 (group mailbox fanout)

Progress: ██████████████████████████████████████████████████████████████░░░░░░░░░░ 75.4%

## Performance Metrics

**Velocity:**
- Total plans completed: 48
- Average duration: ~4.8 min
- Total execution time: ~231 min

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
| 28 | 2+ | ~9m | ~4.5m |

**Recent Trend:**
- Last 5 plans: 27-2 (~7m), 27-3 (~4m), 28-1 (~3m), 28-2 (~6m)
- Trend: Group fanout using futures::join_all for parallel delivery

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

### Deferred Issues

None.

### Blockers/Concerns

None.

## Session Continuity

Last session: 2026-02-04
Stopped at: Completed Phase 28-02 (group mailbox fanout)
Resume file: .planning/phases/28-mailbox-group-system/28-02-SUMMARY.md
Next action: Execute Phase 28-03 (group coordination)
