//! Unified conversation repository.
//!
//! Provides a consolidated view of all conversation types (Channel, DM, Anon DM, Anon Group).
//! Computes last message timestamps and unread counts dynamically from the messages table.

use crate::domain::UnifiedConversation;
use crate::domain::message::ConversationType;
use crate::error::StorageError;
use crate::storage::Database;
use chrono::{DateTime, Utc};
use rusqlite::{Row, params};
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

    /// Names for folder rows. Folder enrichment obtains message activity from
    /// `conversation_stats`, so this query need not aggregate messages again.
    pub fn conversation_names(
        &self,
    ) -> Result<std::collections::HashMap<(String, ConversationType), String>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, CASE channel_type WHEN 2 THEN 'server_dm' \
                WHEN 3 THEN 'saved_messages' ELSE 'channel' END, name FROM channels \
             UNION ALL SELECT id, 'anon_dm', COALESCE(peer_display_name, 'Anonymous Peer') \
                FROM anon_dm_conversations \
             UNION ALL SELECT id, 'anon_group', name FROM anon_groups",
        )?;
        let mut rows = stmt.query([])?;
        let mut names = std::collections::HashMap::new();
        while let Some(row) = rows.next()? {
            let kind: String = row.get(1)?;
            if let Ok(kind) = ConversationType::from_str_value(&kind) {
                names.insert((row.get(0)?, kind), row.get(2)?);
            }
        }
        Ok(names)
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
                    MAX(CASE WHEN thread_id IS NULL THEN created_at END) as last_msg_at,
                    SUM(CASE WHEN is_read = 0 AND is_deleted = 0 AND thread_id IS NULL THEN 1 ELSE 0 END) as unread_cnt
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
        let mut rows = stmt.query([])?;
        super::repo_utils::collect_decoded_rows(&mut rows, "conversation", |row| {
            row_to_unified(row).map_err(StorageError::Database)
        })
    }

    /// Get a single conversation by ID and type.
    pub fn get_conversation(
        &self,
        id: &str,
        conv_type: &ConversationType,
    ) -> Result<Option<UnifiedConversation>, StorageError> {
        let conn = self.db.read_connection()?;

        let sql = match conv_type {
            ConversationType::Channel
            | ConversationType::ServerDm
            | ConversationType::SavedMessages => {
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
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = c.id AND conversation_type = ?2 AND is_deleted = 0 AND thread_id IS NULL) as last_msg_at,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id AND conversation_type = ?2 AND is_read = 0 AND is_deleted = 0 AND thread_id IS NULL) as unread_count,
                    c.created_at,
                    c.updated_at
                FROM channels c WHERE c.id = ?1
                "#
            }
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
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = a.id AND conversation_type = 'anon_dm' AND is_deleted = 0 AND thread_id IS NULL) as last_msg_at,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = a.id AND conversation_type = 'anon_dm' AND is_read = 0 AND is_deleted = 0 AND thread_id IS NULL) as unread_count,
                    a.created_at,
                    a.updated_at
                FROM anon_dm_conversations a WHERE a.id = ?1
                "#
            }
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
                    (SELECT MAX(created_at) FROM messages WHERE conversation_id = g.id AND conversation_type = 'anon_group' AND is_deleted = 0 AND thread_id IS NULL) as last_msg_at,
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = g.id AND conversation_type = 'anon_group' AND is_read = 0 AND is_deleted = 0 AND thread_id IS NULL) as unread_count,
                    g.created_at,
                    g.updated_at
                FROM anon_groups g WHERE g.id = ?1
                "#
            }
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

    /// Friend-DM conversations `me` has sent in, newest activity first — the
    /// conversations we accepted, as Signal counts a reply. Ids are the peer's
    /// user id (a friend DM is keyed by the peer).
    pub fn dm_conversations_sent_in(&self, me: &str) -> Result<Vec<String>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT conversation_id
            FROM messages
            WHERE conversation_type = 'server_dm'
            GROUP BY conversation_id
            HAVING SUM(CASE WHEN sender_id = ?1 THEN 1 ELSE 0 END) > 0
            ORDER BY MAX(created_at) DESC
            "#,
        )?;
        let ids = stmt
            .query_map(params![me], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Unread count and last-message time for EVERY conversation, in one query.
    ///
    /// This is `list_all`'s `conv_stats` CTE on its own. It reads the messages
    /// table directly, so it also covers conversations with no parent row —
    /// notably channel-free server DMs, which `list_all` cannot emit because it
    /// starts from `channels`. Sidebar folders need those stats or a foldered
    /// DM shows no badge and sorts as if it had never received a message.
    pub fn conversation_stats(
        &self,
    ) -> Result<
        std::collections::HashMap<(String, ConversationType), ConversationStats>,
        StorageError,
    > {
        let conn = self.db.read_connection()?;
        let mut stmt = conn.prepare(
            r#"
            SELECT conversation_id,
                   conversation_type,
                   MAX(CASE WHEN thread_id IS NULL THEN created_at END) as last_msg_at,
                   SUM(CASE WHEN is_read = 0 AND is_deleted = 0 AND thread_id IS NULL THEN 1 ELSE 0 END) as unread_cnt
            FROM messages
            GROUP BY conversation_id, conversation_type
            "#,
        )?;

        let mut rows = stmt.query([])?;
        let mut out = std::collections::HashMap::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let type_str: String = row.get(1)?;
            let Ok(conv_type) = ConversationType::from_str_value(&type_str) else {
                continue;
            };
            let last_message_at: Option<String> = row.get(2)?;
            let unread_count: i64 = row.get(3).unwrap_or(0);
            out.insert(
                (id, conv_type),
                ConversationStats {
                    last_message_at: last_message_at
                        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                        .map(|dt| dt.with_timezone(&Utc)),
                    unread_count: unread_count.max(0) as u32,
                },
            );
        }
        Ok(out)
    }
}

/// Per-conversation activity, read straight from the messages table.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConversationStats {
    /// When the last message landed, if any.
    pub last_message_at: Option<DateTime<Utc>>,
    /// Unread, undeleted messages.
    pub unread_count: u32,
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
        owner_id: row
            .get::<_, Option<String>>(4)?
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
    fn corrupt_conversation_row_does_not_hide_chat_list() {
        let dir = tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("list.db"), "fixture").unwrap());
        db.run_migrations().unwrap();
        let conn = db.connection().unwrap();
        conn.execute_batch("INSERT INTO channels (id,channel_type,name,owner_id,created_at,updated_at) VALUES ('good',0,'Good','owner','2026-01-01','2026-01-01'), ('bad',0,X'FF','owner','2026-01-01','2026-01-01')").unwrap();
        drop(conn);
        let repo = UnifiedConversationRepository::new(db);
        let list = repo.list_all().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "good");
        assert!(
            repo.get_conversation("bad", &ConversationType::Channel)
                .is_err()
        );
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
