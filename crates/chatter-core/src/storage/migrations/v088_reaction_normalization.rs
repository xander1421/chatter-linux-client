//! Repair already-installed v087; changing v087 alone never upgrades a device.
use super::Migration;
use crate::{
    error::StorageError, storage::repositories::unified_message_repo::reaction_normalization,
};
use rusqlite::Connection;

pub struct V088ReactionNormalization;
impl Migration for V088ReactionNormalization {
    fn version(&self) -> u32 {
        88
    }
    fn description(&self) -> &'static str {
        "Repair reaction add selections and quarantine unordered votes"
    }
    fn up(&self) -> &'static str {
        reaction_normalization::REPAIR_V087_SELECTIONS
    }
    fn apply(&self, conn: &Connection) -> Result<(), StorageError> {
        conn.execute_batch(self.up())?;
        reaction_normalization::normalize(conn)?;
        reaction_normalization::normalize_pending(conn)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::message::ReactionCount,
        storage::{
            migrations::{MigrationRunner, V087SingleReaction},
            serialization::db_deserialize,
        },
    };

    fn before_87() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE settings(key TEXT,value TEXT);
            INSERT INTO settings VALUES('auth.current_user_id','\"me\"');
            CREATE TABLE messages(id TEXT PRIMARY KEY,conversation_id TEXT,conversation_type TEXT,sender_id TEXT,sent_at INTEGER,reaction_counts_blob BLOB,current_user_reactions_blob BLOB,content_json TEXT);
            CREATE TABLE unified_reactions(id TEXT PRIMARY KEY,conversation_id TEXT,conversation_type TEXT,sender_id TEXT,sent_at_ns INTEGER,user_id TEXT,emoji TEXT,created_at TEXT);
            CREATE TABLE reaction_action_versions(conversation_id TEXT,conversation_type TEXT,author_id TEXT,target_sent_at INTEGER,actor_id TEXT,emoji TEXT,action_ts INTEGER,remove INTEGER,PRIMARY KEY(conversation_type,conversation_id,author_id,target_sent_at,actor_id,emoji));
            CREATE TABLE pending_scoped_reactions(conversation_id TEXT,conversation_type TEXT,author_id TEXT,target_sent_at INTEGER,actor_id TEXT,emoji TEXT,remove INTEGER,action_ts INTEGER,received_at INTEGER);
            CREATE TABLE reaction_scope_unresolved(conversation_id TEXT,conversation_type TEXT,sender_id TEXT,sent_at_ns INTEGER);
            CREATE TABLE drafts(body TEXT); INSERT INTO drafts VALUES('keep draft');
            INSERT INTO messages VALUES('active','scope','channel','author',100,X'00',X'00','body active'),('empty','scope','channel','author',200,X'00',X'00','body empty'),('clockless','scope','server_dm','author',100,X'00',X'00','body clockless'),('masked','mask','channel','author',300,X'CAFE',X'BEEF','body masked');
            INSERT INTO reaction_scope_unresolved VALUES('mask','channel','author',300);
            INSERT INTO unified_reactions VALUES('b','scope','channel','author',100,'me','B','old'),('unknown','scope','server_dm','author',100,'me','X','old');
            INSERT INTO reaction_action_versions VALUES('scope','channel','author',100,'me','A',30,1),('scope','channel','author',100,'me','B',20,0),('scope','channel','author',200,'me','B',40,1);").unwrap();
        conn
    }
    fn installed_87() -> Connection {
        let conn = before_87();
        // Reproduce shipped schema/data, not the corrected v087 implementation.
        conn.execute_batch("CREATE TABLE reaction_actor_selections(conversation_id TEXT,conversation_type TEXT,author_id TEXT,target_sent_at INTEGER,actor_id TEXT,emoji TEXT,action_ts INTEGER,PRIMARY KEY(conversation_type,conversation_id,author_id,target_sent_at,actor_id));
            INSERT INTO reaction_actor_selections VALUES('scope','channel','author',100,'me','A',30),('scope','channel','author',200,'me','B',40);
            CREATE UNIQUE INDEX idx_ur_single_actor ON unified_reactions(conversation_type,conversation_id,sender_id,sent_at_ns,user_id);
            CREATE TABLE schema_version(version INTEGER PRIMARY KEY,description TEXT NOT NULL,applied_at TEXT NOT NULL);
            INSERT INTO schema_version VALUES(87,'shipped87','before');").unwrap();
        conn
    }
    fn assert_repaired(conn: &Connection) {
        let selection:(String,i64)=conn.query_row("SELECT emoji,action_ts FROM reaction_actor_selections WHERE target_sent_at=100 AND conversation_type='channel'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(selection, ("B".into(), 20));
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM reaction_actor_selections WHERE target_sent_at=200",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0,
            "all-removed state must not fabricate an add selection"
        );
        let (counts,own):(Vec<u8>,Vec<u8>)=conn.query_row("SELECT reaction_counts_blob,current_user_reactions_blob FROM messages WHERE id='active'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(db_deserialize::<Vec<String>>(&own).unwrap(), vec!["B"]);
        assert_eq!(
            db_deserialize::<Vec<ReactionCount>>(&counts).unwrap(),
            vec![ReactionCount::new("B".into(), 1)]
        );
        for id in ["empty", "clockless"] {
            let (counts,own):(Vec<u8>,Vec<u8>)=conn.query_row("SELECT reaction_counts_blob,current_user_reactions_blob FROM messages WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert!(
                db_deserialize::<Vec<ReactionCount>>(&counts)
                    .unwrap()
                    .is_empty()
            );
            assert!(db_deserialize::<Vec<String>>(&own).unwrap().is_empty());
        }
        let masked:(Vec<u8>,Vec<u8>)=conn.query_row("SELECT reaction_counts_blob,current_user_reactions_blob FROM messages WHERE id='masked'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(masked, (vec![0xca, 0xfe], vec![0xbe, 0xef]));
        assert_eq!(conn.query_row("SELECT count(*) FROM reaction_scoped_quarantine WHERE id='unknown' AND conversation_type='server_dm'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(
            conn.query_row("SELECT body FROM drafts", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "keep draft"
        );
        assert_eq!(
            conn.query_row(
                "SELECT content_json FROM messages WHERE id='active'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "body active"
        );
    }
    #[test]
    fn reaction_migration88_repairs_installed87_and_runner_does_not_reapply87() {
        let conn = installed_87();
        let runner = MigrationRunner::new()
            .register(V087SingleReaction)
            .register(V088ReactionNormalization);
        runner.run(&conn).unwrap();
        assert_repaired(&conn);
        runner.run(&conn).unwrap();
        assert_repaired(&conn);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM schema_version WHERE version=88",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        let fresh = before_87();
        MigrationRunner::new()
            .register(V087SingleReaction)
            .register(V088ReactionNormalization)
            .run(&fresh)
            .unwrap();
        assert_repaired(&fresh);
    }
    #[test]
    fn reaction_migration88_failed_cache_rebuild_rolls_back_every_change_and_marker() {
        let conn = installed_87();
        conn.execute_batch("CREATE TRIGGER fail_projection BEFORE UPDATE OF reaction_counts_blob ON messages BEGIN SELECT RAISE(ABORT,'injected cache failure'); END;").unwrap();
        let runner = MigrationRunner::new().register(V088ReactionNormalization);
        assert!(runner.run(&conn).is_err());
        assert_eq!(
            conn.query_row(
                "SELECT emoji FROM reaction_actor_selections WHERE target_sent_at=100",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "A"
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM unified_reactions", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='reaction_scoped_quarantine'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM schema_version WHERE version=88",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        conn.execute_batch("DROP TRIGGER fail_projection").unwrap();
        runner.run(&conn).unwrap();
        assert_repaired(&conn);
    }
    #[test]
    fn reaction_migration88_preserves_add_clock_before_its_later_remove() {
        let conn = installed_87();
        conn.execute(
            "UPDATE reaction_actor_selections SET action_ts=35 WHERE target_sent_at=200",
            [],
        )
        .unwrap();
        V088ReactionNormalization.apply(&conn).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT action_ts FROM reaction_actor_selections WHERE target_sent_at=200",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            35,
            "a legitimate retained add is not erased by its removal"
        );
    }
}
