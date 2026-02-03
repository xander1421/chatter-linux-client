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
    /// The channel containing the message.
    pub channel_id: ChannelId,
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
        let conn = self.db.connection()?;

        // Escape special FTS5 characters to prevent query injection
        let escaped_query = escape_fts5_query(query);
        if escaped_query.is_empty() {
            return Ok(Vec::new());
        }

        let results = if let Some(cid) = channel_id {
            let mut stmt = conn.prepare(
                "SELECT message_id, channel_id, bm25(messages_fts) as score
                 FROM messages_fts
                 WHERE messages_fts MATCH ?1 AND channel_id = ?2
                 ORDER BY score
                 LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![escaped_query, cid.to_string(), limit])?;
            collect_results(&mut rows)?
        } else {
            let mut stmt = conn.prepare(
                "SELECT message_id, channel_id, bm25(messages_fts) as score
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
            "INSERT INTO messages_fts(message_id, channel_id, content)
             SELECT
                 id,
                 channel_id,
                 CASE content_type
                     WHEN 0 THEN json_extract(content_json, '$')
                     WHEN 5 THEN json_extract(content_json, '$.AI.text')
                     WHEN 6 THEN json_extract(content_json, '$.Mixed.text')
                 END
             FROM messages
             WHERE content_type IN (0, 5, 6)
               AND COALESCE(
                   CASE content_type
                       WHEN 0 THEN json_extract(content_json, '$')
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
        let channel_id: String = row.get(1)?;
        let score: f64 = row.get(2)?;

        results.push(SearchResult {
            message_id: message_id
                .parse()
                .map_err(|_| StorageError::InvalidData("Invalid message ID".into()))?,
            channel_id: channel_id
                .parse()
                .map_err(|_| StorageError::InvalidData("Invalid channel ID".into()))?,
            score,
        });
    }
    Ok(results)
}
