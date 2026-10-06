//! One active emoji per actor and exact message scope, including retained history.
use super::Migration;
use crate::{
    domain::message::ReactionCount, error::StorageError, storage::serialization::db_serialize,
};
use rusqlite::{Connection, params};

pub struct V087SingleReaction;
impl Migration for V087SingleReaction {
    fn version(&self) -> u32 {
        87
    }
    fn description(&self) -> &'static str {
        "One emoji reaction per user per message"
    }
    fn up(&self) -> &'static str {
        r#"
        DELETE FROM unified_reactions WHERE id IN (
            SELECT id FROM (
                SELECT r.id, ROW_NUMBER() OVER (
                    PARTITION BY r.conversation_type,r.conversation_id,r.sender_id,r.sent_at_ns,r.user_id
                    ORDER BY COALESCE(v.action_ts,0) DESC,r.created_at DESC,r.emoji DESC,r.id DESC
                ) AS position
                FROM unified_reactions r LEFT JOIN reaction_action_versions v
                ON v.conversation_type=r.conversation_type AND v.conversation_id=r.conversation_id
                AND v.author_id=r.sender_id AND v.target_sent_at=r.sent_at_ns
                AND v.actor_id=r.user_id AND v.emoji=r.emoji
            ) WHERE position>1
        );
        CREATE TABLE reaction_actor_selections (
            conversation_id TEXT NOT NULL, conversation_type TEXT NOT NULL,
            author_id TEXT NOT NULL,target_sent_at INTEGER NOT NULL,
            actor_id TEXT NOT NULL,emoji TEXT NOT NULL,action_ts INTEGER NOT NULL,
            PRIMARY KEY(conversation_type,conversation_id,author_id,target_sent_at,actor_id)
        );
        INSERT INTO reaction_actor_selections
        SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id,emoji,action_ts
        FROM (SELECT *,ROW_NUMBER() OVER (
            PARTITION BY conversation_type,conversation_id,author_id,target_sent_at,actor_id
            ORDER BY action_ts DESC,emoji DESC
        ) AS position FROM reaction_action_versions) WHERE position=1;
        CREATE TRIGGER delete_message_reaction_selection AFTER DELETE ON messages
        BEGIN
            DELETE FROM reaction_actor_selections WHERE conversation_id=OLD.conversation_id
            AND conversation_type=OLD.conversation_type AND author_id=OLD.sender_id
            AND target_sent_at=OLD.sent_at;
        END;
        CREATE UNIQUE INDEX idx_ur_single_actor ON unified_reactions
            (conversation_type,conversation_id,sender_id,sent_at_ns,user_id);
        "#
    }
    fn apply(&self, conn: &Connection) -> Result<(), StorageError> {
        conn.execute_batch(self.up())?;
        // Bincode caches must be rebuilt in the same migration transaction:
        // otherwise the first display would still show the old stacked votes.
        let own: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key='auth.current_user_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let own = own
            .and_then(|v| serde_json::from_str::<String>(&v).ok())
            .unwrap_or_default();
        let mut stmt = conn.prepare("SELECT DISTINCT m.id,m.conversation_id,m.conversation_type,m.sender_id,m.sent_at FROM messages m JOIN unified_reactions r ON r.conversation_id=m.conversation_id AND r.conversation_type=m.conversation_type AND r.sender_id=m.sender_id AND r.sent_at_ns=m.sent_at")?;
        let targets = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (id, scope, kind, author, clock) in targets {
            let mut counts_stmt = conn.prepare("SELECT emoji,COUNT(*) FROM unified_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4 GROUP BY emoji ORDER BY emoji")?;
            let counts = counts_stmt
                .query_map(params![scope, kind, author, clock], |r| {
                    Ok(ReactionCount::new(
                        r.get::<_, String>(0)?,
                        r.get::<_, u32>(1)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut own_stmt = conn.prepare("SELECT emoji FROM unified_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4 AND user_id=?5")?;
            let emojis = own_stmt
                .query_map(params![scope, kind, author, clock, own], |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            conn.execute("UPDATE messages SET reaction_counts_blob=?1,current_user_reactions_blob=?2 WHERE id=?3",params![db_serialize(&counts)?,db_serialize(&emojis)?,id])?;
        }
        Ok(())
    }
}
use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::serialization::db_deserialize;

    #[test]
    fn single_reaction_migration_keeps_latest_vote_and_rebuilds_both_caches() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE settings(key TEXT,value TEXT);
            INSERT INTO settings VALUES('auth.current_user_id','\"me\"');
            CREATE TABLE messages(id TEXT,conversation_id TEXT,conversation_type TEXT,sender_id TEXT,sent_at INTEGER,reaction_counts_blob BLOB,current_user_reactions_blob BLOB);
            INSERT INTO messages VALUES('message','scope','channel','author',100,X'00',X'00');
            CREATE TABLE unified_reactions(id TEXT PRIMARY KEY,conversation_id TEXT,conversation_type TEXT,sender_id TEXT,sent_at_ns INTEGER,user_id TEXT,emoji TEXT,created_at TEXT);
            CREATE TABLE reaction_action_versions(conversation_id TEXT,conversation_type TEXT,author_id TEXT,target_sent_at INTEGER,actor_id TEXT,emoji TEXT,action_ts INTEGER,remove INTEGER);
            INSERT INTO unified_reactions VALUES('old','scope','channel','author',100,'me','👍','2026-02'),('new','scope','channel','author',100,'me','❤️','2026-01'),('peer','scope','channel','author',100,'peer','👍','2026-01');
            INSERT INTO reaction_action_versions VALUES('scope','channel','author',100,'me','👍',10,0),('scope','channel','author',100,'me','❤️',20,0);").unwrap();
        V087SingleReaction.apply(&db).unwrap();
        let (counts, own): (Vec<u8>, Vec<u8>) = db
            .query_row(
                "SELECT reaction_counts_blob,current_user_reactions_blob FROM messages",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(db_deserialize::<Vec<String>>(&own).unwrap(), vec!["❤️"]);
        let counts: Vec<ReactionCount> = db_deserialize(&counts).unwrap();
        assert_eq!(counts.iter().map(|r| r.count).sum::<u32>(), 2);
        assert!(db.execute("INSERT INTO unified_reactions VALUES('extra','scope','channel','author',100,'me','😂','now')",[]).is_err());
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
