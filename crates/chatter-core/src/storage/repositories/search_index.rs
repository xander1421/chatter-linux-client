//! Full-text search index.
//!
//! FTS5 full-text search integration for message content.

use crate::domain::ids::{ChannelId, MessageId};
use crate::error::StorageError;
use crate::storage::Database;
use rusqlite::params;
use std::sync::Arc;

/// Search result with message ID and relevance score.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// The matching message ID.
    pub message_id: MessageId,
    /// The conversation containing the message (channel ID, group ID, etc.).
    pub conversation_id: ChannelId,
    /// BM25 relevance score (lower is better match).
    pub score: f64,
}

/// Search index using FTS5.
///
/// Provides full-text search over message content with optional
/// channel filtering and relevance ranking.
pub struct SearchIndex {
    db: Arc<Database>,
}

impl SearchIndex {
    /// Create a new SearchIndex with the given database.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Search messages by text query.
    ///
    /// Returns message IDs with relevance scores, ordered by relevance.
    /// Uses FTS5 MATCH with bm25() ranking.
    ///
    /// # Arguments
    ///
    /// * `query` - The search query (FTS5 query syntax)
    /// * `channel_id` - Optional channel filter
    /// * `limit` - Maximum results to return
    pub fn search(
        &self,
        query: &str,
        channel_id: Option<&ChannelId>,
        limit: u32,
    ) -> Result<Vec<SearchResult>, StorageError> {
        let conn = self.db.read_connection()?;

        // Escape special FTS5 characters to prevent query injection
        let escaped_query = escape_fts5_query(query);
        if escaped_query.is_empty() {
            return Ok(Vec::new());
        }

        let results = if let Some(cid) = channel_id {
            let mut stmt = conn.prepare(
                "SELECT message_id, conversation_id, bm25(messages_fts) as score
                 FROM messages_fts
                 WHERE messages_fts MATCH ?1 AND conversation_id = ?2
                 ORDER BY score, rowid DESC
                 LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![escaped_query, cid.to_string(), limit])?;
            collect_results(&mut rows)?
        } else {
            let mut stmt = conn.prepare(
                "SELECT message_id, conversation_id, bm25(messages_fts) as score
                 FROM messages_fts
                 WHERE messages_fts MATCH ?1
                 ORDER BY score, rowid DESC
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![escaped_query, limit])?;
            collect_results(&mut rows)?
        };

        Ok(results)
    }

    /// Page FTS hits after applying message metadata filters in SQLite.
    pub fn search_scoped(
        &self,
        query: &str,
        sender: Option<&str>,
        conversation: Option<&str>,
        since: Option<i64>,
        until: Option<i64>,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<SearchResult>, StorageError> {
        let escaped_query = escape_fts5_query(query);
        if escaped_query.is_empty() {
            // No text but a sender or chat filter: that person's / chat's
            // messages, newest first, as Telegram's "From: …" lists them.
            if sender.is_none() && conversation.is_none() {
                return Ok(Vec::new());
            }
            let conn = self.db.read_connection()?;
            let mut stmt = conn.prepare(
                "SELECT m.id, m.conversation_id, 0.0 FROM messages m
                 WHERE m.is_deleted = 0
                   AND m.id IN (SELECT message_id FROM messages_fts)
                   AND (?1 IS NULL OR m.sender_id = ?1)
                   AND (?2 IS NULL OR m.conversation_id = ?2)
                   AND (?3 IS NULL OR m.sent_at >= ?3)
                   AND (?4 IS NULL OR m.sent_at < ?4)
                 ORDER BY m.sent_at DESC LIMIT ?5 OFFSET ?6",
            )?;
            let mut rows = stmt.query(params![sender, conversation, since, until, limit, offset])?;
            return collect_results(&mut rows);
        }
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT messages_fts.message_id, messages_fts.conversation_id, bm25(messages_fts) AS score
             FROM messages_fts JOIN messages m ON m.id = messages_fts.message_id
             WHERE messages_fts MATCH ?1 AND m.is_deleted = 0
               AND (?2 IS NULL OR m.sender_id = ?2)
               AND (?3 IS NULL OR m.conversation_id = ?3)
               AND (?4 IS NULL OR m.sent_at >= ?4)
               AND (?5 IS NULL OR m.sent_at < ?5)
             ORDER BY score, m.sent_at DESC, messages_fts.rowid DESC LIMIT ?6 OFFSET ?7",
        )?;
        let mut rows = stmt.query(params![
            escaped_query,
            sender,
            conversation,
            since,
            until,
            limit,
            offset
        ])?;
        collect_results(&mut rows)
    }
}

/// Escape special FTS5 query characters.
///
/// FTS5 has special syntax characters that need escaping for literal search.
fn escape_fts5_query(query: &str) -> String {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Quote the entire query to treat as literal phrase
    // Double any existing quotes to escape them
    let escaped = trimmed.replace('"', "\"\"");
    // Trailing `*` makes the LAST token a prefix match. FTS5 is whole-token by
    // default, so searching "fix" found nothing in a conversation full of
    // "fixcheck-alpha" — which reads as search being broken. Prefix matching is
    // what every messenger's search does as you type.
    format!("\"{}\"*", escaped)
}

/// Collect search results from a Rows iterator.
fn collect_results(rows: &mut rusqlite::Rows<'_>) -> Result<Vec<SearchResult>, StorageError> {
    let mut results = Vec::new();
    while let Some(row) = rows.next()? {
        let message_id: String = row.get(0)?;
        let conversation_id: String = row.get(1)?;
        let score: f64 = row.get(2)?;

        results.push(SearchResult {
            message_id: message_id
                .parse()
                .map_err(|_| StorageError::InvalidData("Invalid message ID".into()))?,
            conversation_id: conversation_id
                .parse()
                .map_err(|_| StorageError::InvalidData("Invalid conversation ID".into()))?,
            score,
        });
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::UserId;
    use crate::domain::message::{ConversationType, Message};
    use crate::storage::Database;
    use crate::storage::repositories::MessageRepository;

    fn setup_db() -> Arc<Database> {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();

        // Keep temp_dir alive until the test ends.
        crate::test_tempdir::hold(temp_dir);

        db
    }

    #[test]
    fn test_fts5_available() {
        let db = setup_db();
        let conn = db.connection().unwrap();
        let res: rusqlite::Result<i32> = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='messages_fts' AND type='table'",
            [],
            |row| row.get(0),
        );
        assert!(res.is_ok(), "Query failed: {:?}", res.err());
        assert_eq!(
            res.unwrap(),
            1,
            "messages_fts table not found in sqlite_master"
        );
    }

    #[test]
    fn test_search_basic() {
        let db = setup_db();
        let index = SearchIndex::new(db.clone());
        let repo = MessageRepository::new(db.clone());

        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        // Insert some messages
        let msg1 = Message::new_text(
            channel_id.clone(),
            ConversationType::Channel,
            Some(user_id.clone()),
            "Hello world".to_string(),
            0,
        );
        repo.insert(&msg1).unwrap();

        let msg2 = Message::new_text(
            channel_id.clone(),
            ConversationType::Channel,
            Some(user_id.clone()),
            "Another message about rust".to_string(),
            0,
        );
        repo.insert(&msg2).unwrap();

        // Search for "hello"
        let results = index.search("hello", None, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_id, msg1.id);

        // Search for "rust"
        let results = index.search("rust", None, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_id, msg2.id);

        // Search for something that doesn't exist
        let results = index.search("nomatch", None, 10).unwrap();
        assert_eq!(results.len(), 0);

        // Prefix match: typing the start of a word finds it (§112q).
        let results = index.search("wor", None, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message_id, msg1.id);
    }

    #[test]
    fn test_search_phrase() {
        let db = setup_db();
        let index = SearchIndex::new(db.clone());
        let repo = MessageRepository::new(db.clone());

        let channel_id = ChannelId::new();
        let user_id = UserId::new();

        repo.insert(&Message::new_text(
            channel_id.clone(),
            ConversationType::Channel,
            Some(user_id.clone()),
            "the quick brown fox".to_string(),
            0,
        ))
        .unwrap();

        // Search for "quick brown"
        let results = index.search("quick brown", None, 10).unwrap();
        assert_eq!(results.len(), 1);

        // Search for "brown quick" - currently it won't match because we quote the query
        let results = index.search("brown quick", None, 10).unwrap();
        assert_eq!(results.len(), 0);
    }
}
