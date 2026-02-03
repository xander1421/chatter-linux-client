# Summary: FTS5 Search Index Plan

## What Was Built

### V009 FTS5 Migration (94548b8)
Created `v009_search_index.rs` with:
- **FTS5 virtual table** `messages_fts` for full-text search
- Content columns: `message_id` (unindexed), `channel_id` (unindexed), `content` (searchable)
- Uses `content=''` (contentless) with `contentless_delete=1` for manual content management

### Automatic Sync Triggers
Three triggers keep FTS in sync with messages table:
- **INSERT trigger**: Extracts text from searchable content types (0=Text, 5=AI, 6=Mixed) using `json_extract()`
- **UPDATE trigger**: Removes old entry, inserts new if content changed
- **DELETE trigger**: Removes entry when message deleted

Text extraction paths:
- `content_type=0`: `json_extract(content_json, '$')` (plain text)
- `content_type=5`: `json_extract(content_json, '$.AI.text')` (AI response text)
- `content_type=6`: `json_extract(content_json, '$.Mixed.text')` (mixed message text)

### SearchIndex Repository (512d5a7)
Full `SearchIndex` implementation:
- `search(query, channel_id, limit)` - FTS5 MATCH with bm25() ranking
- `reindex_all()` - Rebuild index from messages table
- Query escaping via phrase quoting to prevent FTS5 injection
- `SearchResult` struct with message_id, channel_id, and relevance score

### Re-exports (98f82c5)
- `storage::SearchIndex` and `storage::SearchResult` accessible at crate level

## Decisions Made

| Decision | Rationale |
|----------|-----------|
| Contentless FTS5 table | Avoids data duplication; triggers manage sync |
| json_extract() in triggers | Parse MessageContent JSON at insert time, not query time |
| Phrase quoting for queries | Simple, safe escaping; treats entire query as literal |
| bm25() ranking | Standard relevance scoring (lower = better match) |

## Commits

| Hash | Type | Description |
|------|------|-------------|
| 94548b8 | feat | V009 FTS5 migration with virtual table and triggers |
| 512d5a7 | feat | SearchIndex repository with search/reindex methods |
| 98f82c5 | feat | SearchIndex re-export at storage level |

## Verification

- [x] `cargo build` succeeds
- [x] `cargo clippy` passes without warnings
- [x] V009 migration registered in MigrationRunner
- [x] SearchIndex::search() implemented
- [x] SearchIndex::reindex_all() implemented
- [x] Re-exports work: storage::SearchIndex accessible

## Next Steps

Phase 8 complete (1/1 plans). Ready for Phase 9: Storage Key Store.
