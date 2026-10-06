//! One active emoji per actor and exact message scope, including retained history.
use super::Migration;
use crate::error::StorageError;
use rusqlite::Connection;

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
        CREATE TABLE reaction_actor_selections (
            conversation_id TEXT NOT NULL, conversation_type TEXT NOT NULL,
            author_id TEXT NOT NULL,target_sent_at INTEGER NOT NULL,
            actor_id TEXT NOT NULL,emoji TEXT NOT NULL,action_ts INTEGER NOT NULL,
            PRIMARY KEY(conversation_type,conversation_id,author_id,target_sent_at,actor_id)
        );
        CREATE TRIGGER delete_message_reaction_selection AFTER DELETE ON messages
        BEGIN
            DELETE FROM reaction_actor_selections WHERE conversation_id=OLD.conversation_id
            AND conversation_type=OLD.conversation_type AND author_id=OLD.sender_id
            AND target_sent_at=OLD.sent_at;
        END;
        "#
    }
    fn apply(&self, conn: &Connection) -> Result<(), StorageError> {
        conn.execute_batch(self.up())?;
        crate::storage::repositories::unified_message_repo::reaction_normalization::normalize(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{domain::message::ReactionCount, storage::serialization::db_deserialize};

    #[test]
    fn single_reaction_migration_keeps_latest_vote_and_rebuilds_both_caches() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE settings(key TEXT,value TEXT);
            CREATE TABLE reaction_scope_unresolved(conversation_id TEXT,conversation_type TEXT,sender_id TEXT,sent_at_ns INTEGER);
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
        assert_eq!(counts.iter().map(|r| r.count).sum::<u32>(), 1);
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM reaction_scoped_quarantine", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert!(db.execute("INSERT INTO unified_reactions VALUES('extra','scope','channel','author',100,'me','😂','now')",[]).is_err());
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
