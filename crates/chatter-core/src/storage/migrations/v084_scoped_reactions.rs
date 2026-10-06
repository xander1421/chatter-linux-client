//! Preserve known reaction ownership and quarantine historical ambiguity.
use super::Migration;
pub struct V084ScopedReactions;
impl Migration for V084ScopedReactions {
    fn version(&self) -> u32 {
        84
    }
    fn description(&self) -> &'static str {
        "Scope reaction ledgers and pending admissions without clearing messages"
    }
    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE unified_reactions RENAME TO reaction_legacy_unscoped;
        DROP INDEX IF EXISTS idx_ur_sender_sent;
        DROP INDEX IF EXISTS idx_ur_user;
        CREATE TABLE unified_reactions (
            id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, conversation_type TEXT NOT NULL,
            sender_id TEXT NOT NULL, sent_at_ns INTEGER NOT NULL,
            user_id TEXT NOT NULL, emoji TEXT NOT NULL, created_at TEXT NOT NULL,
            UNIQUE(conversation_type, conversation_id, sender_id, sent_at_ns, user_id, emoji)
        );
        INSERT INTO unified_reactions
        SELECT r.id, m.conversation_id, m.conversation_type, r.sender_id, r.sent_at_ns, r.user_id, r.emoji, r.created_at
        FROM reaction_legacy_unscoped r JOIN messages m ON m.sender_id=r.sender_id AND m.sent_at=r.sent_at_ns
        WHERE (SELECT count(*) FROM messages x WHERE x.sender_id=r.sender_id AND x.sent_at=r.sent_at_ns)=1;
        DELETE FROM reaction_legacy_unscoped WHERE id IN (SELECT id FROM unified_reactions);
        CREATE INDEX idx_ur_scope ON unified_reactions(conversation_type,conversation_id,sender_id,sent_at_ns);
        CREATE TABLE reaction_scope_unresolved (
            conversation_id TEXT NOT NULL, conversation_type TEXT NOT NULL,
            sender_id TEXT NOT NULL, sent_at_ns INTEGER NOT NULL,
            PRIMARY KEY(conversation_type,conversation_id,sender_id,sent_at_ns)
        );
        INSERT INTO reaction_scope_unresolved
        SELECT DISTINCT m.conversation_id,m.conversation_type,m.sender_id,m.sent_at
        FROM messages m JOIN reaction_legacy_unscoped r ON r.sender_id=m.sender_id AND r.sent_at_ns=m.sent_at;
        CREATE TRIGGER mark_legacy_reaction_ambiguity AFTER INSERT ON messages
        WHEN EXISTS(SELECT 1 FROM reaction_legacy_unscoped r WHERE r.sender_id=NEW.sender_id AND r.sent_at_ns=NEW.sent_at)
        BEGIN INSERT OR IGNORE INTO reaction_scope_unresolved VALUES(NEW.conversation_id,NEW.conversation_type,NEW.sender_id,NEW.sent_at); END;
        -- Keep all old pending reactions as evidence; never apply wildcard ownership.
        CREATE TABLE reaction_legacy_pending AS SELECT * FROM pending_message_modifiers WHERE kind='reaction';
        CREATE TABLE pending_scoped_reactions (
            conversation_id TEXT NOT NULL, conversation_type TEXT NOT NULL,
            author_id TEXT NOT NULL, target_sent_at INTEGER NOT NULL,
            actor_id TEXT NOT NULL, emoji TEXT NOT NULL, remove INTEGER NOT NULL,
            action_ts INTEGER NOT NULL, received_at INTEGER NOT NULL,
            PRIMARY KEY(conversation_type,conversation_id,author_id,target_sent_at,actor_id,emoji)
        );
        CREATE TABLE reaction_action_versions (
            conversation_id TEXT NOT NULL, conversation_type TEXT NOT NULL, author_id TEXT NOT NULL,
            target_sent_at INTEGER NOT NULL, actor_id TEXT NOT NULL, emoji TEXT NOT NULL, action_ts INTEGER NOT NULL, remove INTEGER NOT NULL,
            PRIMARY KEY(conversation_type,conversation_id,author_id,target_sent_at,actor_id,emoji)
        );
        INSERT INTO pending_scoped_reactions
        SELECT m.conversation_id,m.conversation_type,r.author_id,r.target_sent_at,r.actor_id,r.emoji,r.remove,r.action_ts,r.received_at
        FROM reaction_legacy_pending r JOIN messages m ON m.sender_id=r.author_id AND m.sent_at=r.target_sent_at
        AND (r.conversation_id IS NULL OR r.conversation_id=m.conversation_id)
        WHERE (SELECT count(*) FROM messages x WHERE x.sender_id=r.author_id AND x.sent_at=r.target_sent_at
            AND (r.conversation_id IS NULL OR r.conversation_id=x.conversation_id))=1;
        DELETE FROM pending_message_modifiers WHERE kind='reaction';
        CREATE TRIGGER delete_scoped_message_reactions AFTER DELETE ON messages
        BEGIN
            DELETE FROM unified_reactions WHERE conversation_id=OLD.conversation_id AND conversation_type=OLD.conversation_type AND sender_id=OLD.sender_id AND sent_at_ns=OLD.sent_at;
            DELETE FROM reaction_scope_unresolved WHERE conversation_id=OLD.conversation_id AND conversation_type=OLD.conversation_type AND sender_id=OLD.sender_id AND sent_at_ns=OLD.sent_at;
            DELETE FROM pending_scoped_reactions WHERE conversation_id=OLD.conversation_id AND conversation_type=OLD.conversation_type AND author_id=OLD.sender_id AND target_sent_at=OLD.sent_at;
        END;
    "#
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_preserves_history_and_quarantines_ambiguous_ownership() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE messages(id TEXT,sender_id TEXT,sent_at INTEGER,conversation_id TEXT,conversation_type TEXT);
            CREATE TABLE unified_reactions(id TEXT PRIMARY KEY,sender_id TEXT,sent_at_ns INTEGER,user_id TEXT,emoji TEXT,created_at TEXT);
            CREATE TABLE pending_message_modifiers(author_id TEXT,target_sent_at INTEGER,kind TEXT,actor_id TEXT,emoji TEXT,conversation_id TEXT,body TEXT,action_ts INTEGER,remove INTEGER,received_at INTEGER);
            CREATE TABLE drafts(body TEXT); INSERT INTO drafts VALUES('keep draft');
            INSERT INTO messages VALUES('one','author',10,'a','server_dm'),('two','author',20,'a','channel'),('three','author',20,'b','anon_group');
            INSERT INTO unified_reactions VALUES('known','author',10,'user','👍','now'),('ambiguous','author',20,'user','👍','now'),('orphan','author',30,'user','👍','now');
            INSERT INTO pending_message_modifiers VALUES('author',10,'reaction','user','👍',NULL,NULL,1,0,1),('author',20,'reaction','user','👍',NULL,NULL,1,0,1);").unwrap();
        db.execute_batch(V084ScopedReactions.up()).unwrap();
        let count = |table: &str| {
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };
        assert_eq!(count("messages"), 3);
        assert_eq!(count("drafts"), 1);
        assert_eq!(count("unified_reactions"), 1);
        assert_eq!(count("reaction_legacy_unscoped"), 2);
        assert_eq!(count("reaction_scope_unresolved"), 2);
        assert_eq!(count("pending_scoped_reactions"), 1);
        assert_eq!(count("reaction_legacy_pending"), 2);
        let kind: String = db
            .query_row(
                "SELECT conversation_type FROM unified_reactions WHERE id='known'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "server_dm");
        db.execute_batch("INSERT INTO messages VALUES('later','author',30,'c','anon_dm')")
            .unwrap();
        assert_eq!(
            count("reaction_scope_unresolved"),
            3,
            "orphan ambiguity remains quarantined on later arrival"
        );
    }
}
