# Project State

## Project Reference

See: .planning/PROJECT.md (updated 2026-02-03)

**Core value:** True native experience with full Signal Protocol encryption — replacing Flutter/React with a first-class Linux citizen
**Current focus:** Phase 6 — Storage Core Repositories — In progress

## Current Position

Phase: 6 of 66 (Storage Core Repositories)
Plan: 1 of 3 in current phase
Status: In progress
Last activity: 2026-02-03 — Completed 06-01-PLAN.md

Progress: [xxxxxxxxxx]░ 15.2%

## Performance Metrics

**Velocity:**
- Total plans completed: 9
- Average duration: ~5 min
- Total execution time: ~54 min

**By Phase:**

| Phase | Plans | Total | Avg/Plan |
|-------|-------|-------|----------|
| 1 | 1 | ~30m | ~30m |
| 2 | 2 | ~10m | ~5m |
| 3 | 2 | ~12m | ~6m |
| 4 | 2 | ~10m | ~5m |
| 5 | 2 | ~4m | ~2m |

**Recent Trend:**
- Last 5 plans: 4-1 (~4m), 4-2 (~6m), 5-1 (~2m), 5-2 (~2m)
- Trend: Storage setup fast with established patterns

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

### Deferred Issues

None.

### Blockers/Concerns

None.

## Session Continuity

Last session: 2026-02-03
Stopped at: Completed 06-01-PLAN.md
Resume file: None
