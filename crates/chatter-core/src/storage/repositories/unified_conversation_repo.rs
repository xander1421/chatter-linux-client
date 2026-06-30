//! Unified conversation repository.
//!
//! Provides a consolidated view of all conversation types (Channel, DM, Anon DM, Anon Group).
//! Computes last message timestamps and unread counts dynamically from the messages table.

use crate::domain::message::ConversationType;
use crate::domain::UnifiedConversation;
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::{params, Row};
use std::sync::Arc;

/// Repository for unified conversation access.
pub struct UnifiedConversationRepository {
    db: Arc<Database>,
}

impl UnifiedConversationRepository {
    /// Create a new UnifiedConversationRepository.
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// List all conversations for the unified sidebar.
    ///
    /// Consolidates data from channels, anon_dm_conversations, and anon_groups.
    /// Sorts by last_message_at DESC, then created_at DESC.
    pub fn list_all(&self) -> Result<Vec<UnifiedConversation>, StorageError> {
        let conn = self.db.read_connection()?;
        
        let sql = r#"
            WITH conv_stats AS (
                SELECT 
                    conversation_id, 
                    conversation_type,
                    MAX(created_at) as last_msg_at,
                    SUM(CASE WHEN is_read = 0 AND is_deleted = 0 THEN 1 ELSE 0 END) as unread_cnt
                FROM messages
                GROUP BY conversation_id, conversation_type
            )
            -- 1. Server-managed Channels and DMs
            SELECT 
                c.id, 
                CASE 
                    WHEN c.channel_type = 2 THEN 'server_dm' 
                    WHEN c.channel_type = 3 THEN 'saved_messages'
                    ELSE 'channel' 
                END as conv_type,
                c.name,
                c.description,
                c.owner_id,
                '' as my_role, -- Not needed for sidebar view of channels
                CASE WHEN c.archived = 1 THEN 2 ELSE 1 END as status,
                c.archived,
                CO_STATS.last_msg_at,
                COALESCE(CO_STATS.unread_cnt, 0) as unread_count,
                c.created_at,
                c.updated_at
            FROM channels c
            LEFT JOIN conv_stats CO_STATS ON c.id = CO_STATS.conversation_id AND (CO_STATS.conversation_type IN ('channel', 'server_dm', 'saved_messages'))
            
            UNION ALL

            -- 2. Anonymous DMs
            SELECT 
                a.id,
                'anon_dm' as conv_type,
                COALESCE(a.peer_display_name, 'Anonymous Peer') as name,
                NULL as description,
                NULL as owner_id,
                '' as my_role,
                a.status,
                0 as archived,
                CO_STATS.last_msg_at,
                COALESCE(CO_STATS.unread_cnt, 0) as unread_count,
                a.created_at,
                a.updated_at
            FROM anon_dm_conversations a
            LEFT JOIN conv_stats CO_STATS ON a.id = CO_STATS.conversation_id AND CO_STATS.conversation_type = 'anon_dm'

            UNION ALL

            -- 3. Anonymous Groups
            SELECT 
                g.id,
                'anon_group' as conv_type,
                g.name,
                NULL as description,
                g.owner_id,
                CASE g.my_role
                    WHEN 0 THEN 'member'
                    WHEN 1 THEN 'admin'
                    WHEN 2 THEN 'owner'
                    ELSE 'member'
                END as my_role,
                g.status,
                0 as archived,
                CO_STATS.last_msg_at,
                COALESCE(CO_STATS.unread_cnt, 0) as unread_count,
                g.created_at,
                g.updated_at
            FROM anon_groups g
            LEFT JOIN conv_stats CO_STATS ON g.id = CO_STATS.conversation_id AND CO_STATS.conversation_type = 'anon_group'

            ORDER BY last_msg_at DESC NULLS LAST, created_at DESC
        "#;

        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_unified)?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Get a single conversation by ID and type.
    pub fn get_conversation(
        &self,
        id: &str,
        conv_type: &ConversationType,
    ) -> Result<Option<UnifiedConversation>, StorageError> {
        let conn = self.db.read_connection()?;
        
        let sql = match conv_type {
            ConversationType::Channel | ConversationType::ServerDm | ConversationType::SavedMessages => {
                r#"
                SELECT 
                    c.id, 
                    CASE 
                        WHEN c.channel_type = 2 THEN 'server_dm' 
                        WHEN c.channel_type = 3 THEN 'saved_messages'
                        ELSE 'channel' 
                    END as conv_type,
                    c.name,
                    c.description,
                    c.owner_id,
                    '' as my_role,
                    CASE WHEN c.archived = 1 THEN 2 ELSE 1 END as status,
                    c.archived,
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = c.id AND conversation_type = ?2 AND is_deleted = 0) as last_msg_at,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id AND conversation_type = ?2 AND is_read = 0 AND is_deleted = 0) as unread_count,
                    c.created_at,
                    c.updated_at
                FROM channels c WHERE c.id = ?1
                "#
            },
            ConversationType::AnonDm => {
                r#"
                SELECT 
                    a.id,
                    'anon_dm' as conv_type,
                    COALESCE(a.peer_display_name, 'Anonymous Peer') as name,
                    NULL as description,
                    NULL as owner_id,
                    '' as my_role,
                    a.status,
                    0 as archived,
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = a.id AND conversation_type = 'anon_dm' AND is_deleted = 0) as last_msg_at,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = a.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0) as unread_count,
                    a.created_at,
                    a.updated_at
                FROM anon_dm_conversations a WHERE a.id = ?1
                "#
            },
            ConversationType::AnonGroup => {
                r#"
                SELECT 
                    g.id,
                    'anon_group' as conv_type,
                    g.name,
                    NULL as description,
                    g.owner_id,
                    CASE g.my_role 
                        WHEN 0 THEN 'member'
                        WHEN 1 THEN 'admin'
                        WHEN 2 THEN 'owner'
                        ELSE 'member'
                    END as my_role,
                    g.status,
                    0 as archived,
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = g.id AND conversation_type = 'anon_group' AND is_deleted = 0) as last_msg_at,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = g.id AND conversation_type = 'anon_group' AND is_read = 0 AND is_deleted = 0) as unread_count,
                    g.created_at,
                    g.updated_at
                FROM anon_groups g WHERE g.id = ?1
                "#
            },
            _ => return Ok(None),
        };

        let mut stmt = conn.prepare(sql)?;
        let result = stmt.query_row(params![id, conv_type.as_str()], row_to_unified);

        match result {
            Ok(conv) => Ok(Some(conv)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(StorageError::Database(e)),
        }
    }
}

fn row_to_unified(row: &Row) -> rusqlite::Result<UnifiedConversation> {
    let conv_type_str: String = row.get(1)?;
    let conv_type = ConversationType::from_str_value(&conv_type_str)
        .map_err(|_| rusqlite::Error::InvalidQuery)?;

    let last_message_at_str: Option<String> = row.get(8)?;
    let created_at_str: String = row.get(10)?;
    let updated_at_str: String = row.get(11)?;

    Ok(UnifiedConversation {
        id: row.get(0)?,
        conversation_type: conv_type,
        name: row.get(2)?,
        description: row.get(3)?,
        owner_id: row.get::<_, Option<String>>(4)?
            .and_then(|s| s.parse().ok()),
        my_role: row.get(5)?,
        status: row.get(6)?,
        archived: row.get::<_, i32>(7)? != 0,
        last_message_at: last_message_at_str.and_then(|s| parse_datetime(&s)),
        unread_count: row.get(9)?,
        created_at: parse_datetime(&created_at_str).unwrap_or_else(|| Utc::now()),
        updated_at: parse_datetime(&updated_at_str).unwrap_or_else(|| Utc::now()),
    })
}

fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Schema mirrors the live channels table post-v54: `icon BLOB`, no
    // `image_url`. Guards against re-introducing a `c.image_url` reference,
    // which errors the channel query and silently blanks names in the UI.
    fn repo_with_channel() -> (UnifiedConversationRepository, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let db = Database::open(&dir.path().join("t.db"), "pw").unwrap();
        db.connection()
            .unwrap()
            .execute_batch(
                r#"
                CREATE TABLE channels (
                    id TEXT PRIMARY KEY, channel_type INTEGER NOT NULL DEFAULT 0,
                    name TEXT NOT NULL, description TEXT, icon BLOB, owner_id TEXT NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL, updated_at TEXT NOT NULL
                );
                CREATE TABLE messages (
                    id TEXT PRIMARY KEY, conversation_type TEXT NOT NULL,
                    conversation_id TEXT NOT NULL, is_read INTEGER NOT NULL DEFAULT 0,
                    is_deleted INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL
                );
                INSERT INTO channels (id, channel_type, name, owner_id, created_at, updated_at)
                VALUES ('ch1', 0, 'My Channel', 'owner', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                "#,
            )
            .unwrap();
        (UnifiedConversationRepository::new(Arc::new(db)), dir)
    }

    #[test]
    fn get_conversation_returns_channel_name() {
        let (repo, _dir) = repo_with_channel();
        let conv = repo
            .get_conversation("ch1", &ConversationType::Channel)
            .expect("query must not error on the post-v54 channels schema")
            .expect("channel row should be found");
        assert_eq!(conv.name, "My Channel");
    }
}
