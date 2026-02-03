# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-02-03)

**Core value:** True native experience with full Signal Protocol encryption — replacing Flutter/React with a first-class Linux citizen
**Current focus:** Phase 20 complete — Ready for Phase 21 (Network Resilience)

## Current Position

Phase: 20 of 66 (Network Streaming)
Plan: 4 of 4 in current phase
Status: Phase complete
Last activity: 2026-02-03 — Completed 20-04-PLAN.md

Progress: ████████████████████████████████████████████████████░░░░░░░░░░░░░░░░░░░░ 54.5%

## Performance Metrics

**Velocity:**
- Total plans completed: 33
- Average duration: ~4.2 min
- Total execution time: ~139 min

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

**Recent Trend:**
- Last 5 plans: 20-1 (~3m), 20-2 (~3m), 20-3 (~2m), 20-4 (~3m)
- Trend: Phase 20 complete at ~3m average

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

### Deferred Issues

None.

### Blockers/Concerns

None.

## Session Continuity

Last session: 2026-02-03
Stopped at: Completed 20-04-PLAN.md (Specialized Handlers - Phase 20 complete)
Resume file: None
Next action: Plan Phase 21 (Network Resilience)
