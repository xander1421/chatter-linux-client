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
                 ORDER BY score
                 LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![escaped_query, cid.to_string(), limit])?;
            collect_results(&mut rows)?
        } else {
            let mut stmt = conn.prepare(
                "SELECT message_id, conversation_id, bm25(messages_fts) as score
                 FROM messages_fts
                 WHERE messages_fts MATCH ?1
                 ORDER BY score
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![escaped_query, limit])?;
            collect_results(&mut rows)?
        };

        Ok(results)
    }

    /// Reindex all existing messages (for migration or repair).
    ///
    /// Clears the FTS table and reinserts from the messages table.
    /// Returns the number of messages indexed.
    pub fn reindex_all(&self) -> Result<u64, StorageError> {
        let conn = self.db.connection()?;

        // Clear existing FTS data
        conn.execute("DELETE FROM messages_fts", [])?;

        // Reindex all searchable messages (content_type 0, 5, 6)
        let count = conn.execute(
            "INSERT INTO messages_fts(message_id, conversation_id, content)
             SELECT
                 id,
                 conversation_id,
                 CASE content_type
                     WHEN 0 THEN json_extract(content_json, '$.Text')
                     WHEN 5 THEN json_extract(content_json, '$.AI.text')
                     WHEN 6 THEN json_extract(content_json, '$.Mixed.text')
                 END
             FROM messages
             WHERE content_type IN (0, 5, 6)
               AND COALESCE(
                   CASE content_type
                       WHEN 0 THEN json_extract(content_json, '$.Text')
                       WHEN 5 THEN json_extract(content_json, '$.AI.text')
                       WHEN 6 THEN json_extract(content_json, '$.Mixed.text')
                   END, ''
               ) != ''",
            [],
        )?;

        Ok(count as u64)
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
    format!("\"{}\"", escaped)
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
    use crate::storage::Database;
    use crate::domain::ids::UserId;
    use crate::domain::message::{Message, ConversationType};
    use crate::storage::repositories::MessageRepository;
    use std::path::Path;

    fn setup_db() -> Arc<Database> {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let db = Arc::new(Database::open(&db_path, "testpass").unwrap());
        db.run_migrations().unwrap();
        
        // Keep temp_dir alive by leaking it for simplicity in this test helper
        // (Not ideal but fine for unit tests)
        Box::leak(Box::new(temp_dir));
        
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
        assert_eq!(res.unwrap(), 1, "messages_fts table not found in sqlite_master");
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

        // Reindex just in case triggers didn't fire (though they should)
        index.reindex_all().unwrap();

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
        )).unwrap();

        index.reindex_all().unwrap();

        // Search for "quick brown"
        let results = index.search("quick brown", None, 10).unwrap();
        assert_eq!(results.len(), 1);

        // Search for "brown quick" - currently it won't match because we quote the query
        let results = index.search("brown quick", None, 10).unwrap();
        assert_eq!(results.len(), 0);
    }
}
