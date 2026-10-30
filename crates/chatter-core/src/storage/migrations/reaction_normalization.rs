//! Migration-only (v087/v088) reaction repair, kept as frozen migration history:
//! later migrations drop the quarantine and legacy tables it fills.
//! Votes are projections; adds select an emoji,
//! removals only tombstone that emoji. A vote without an action clock is evidence,
//! not an ordered intent: retain it in exact-scope quarantine, never invent a clock.
use crate::{
    domain::message::ReactionCount, error::StorageError, storage::serialization::db_serialize,
};
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) const QUARANTINE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS reaction_scoped_quarantine (
    id TEXT NOT NULL, conversation_id TEXT NOT NULL, conversation_type TEXT NOT NULL,
    sender_id TEXT NOT NULL, sent_at_ns INTEGER NOT NULL, user_id TEXT NOT NULL,
    emoji TEXT NOT NULL, created_at TEXT NOT NULL,
    PRIMARY KEY(conversation_type,conversation_id,sender_id,sent_at_ns,user_id,emoji,id)
);
CREATE TABLE IF NOT EXISTS pending_reaction_add_selections (
    conversation_id TEXT NOT NULL, conversation_type TEXT NOT NULL,
    author_id TEXT NOT NULL,target_sent_at INTEGER NOT NULL,actor_id TEXT NOT NULL,
    emoji TEXT NOT NULL,action_ts INTEGER NOT NULL,
    PRIMARY KEY(conversation_type,conversation_id,author_id,target_sent_at,actor_id)
);
CREATE TRIGGER IF NOT EXISTS delete_message_reaction_quarantine AFTER DELETE ON messages
BEGIN
    DELETE FROM pending_reaction_add_selections WHERE conversation_id=OLD.conversation_id
    AND conversation_type=OLD.conversation_type AND author_id=OLD.sender_id AND target_sent_at=OLD.sent_at;
    DELETE FROM reaction_scoped_quarantine WHERE conversation_id=OLD.conversation_id
    AND conversation_type=OLD.conversation_type AND sender_id=OLD.sender_id AND sent_at_ns=OLD.sent_at;
END;
"#;

/// Shipped v087 seeded selections from removals. Its runtime did not accept
/// equal-clock removals, so an exact selection/removal match in that format is
/// migration output, not a legitimate retained add. New-format backups tag this
/// distinction because RC60 deliberately permits remove-wins equal-clock ties.
pub(crate) const REPAIR_V087_SELECTIONS: &str = r#"DELETE FROM reaction_actor_selections AS s WHERE EXISTS (
    SELECT 1 FROM reaction_action_versions v WHERE v.conversation_type=s.conversation_type
    AND v.conversation_id=s.conversation_id AND v.author_id=s.author_id
    AND v.target_sent_at=s.target_sent_at AND v.actor_id=s.actor_id
    AND v.emoji=s.emoji AND v.action_ts=s.action_ts AND v.remove=1
);"#;

/// Caller owns a transaction. Keep valid retained selections (including the add
/// clock preceding a same-emoji removal), then merge only add-version evidence.
/// This SQL is also exercised directly by the interpreted SQLite proof script.
pub(crate) const NORMALIZE_SQL: &str = r#"
INSERT INTO reaction_scoped_quarantine
SELECT r.* FROM unified_reactions r WHERE NOT EXISTS (
    SELECT 1 FROM reaction_action_versions v WHERE v.conversation_type=r.conversation_type
    AND v.conversation_id=r.conversation_id AND v.author_id=r.sender_id AND v.target_sent_at=r.sent_at_ns
    AND v.actor_id=r.user_id AND v.emoji=r.emoji AND v.action_ts>0
) ON CONFLICT DO NOTHING;
DELETE FROM unified_reactions AS r WHERE NOT EXISTS (
    SELECT 1 FROM reaction_action_versions v WHERE v.conversation_type=r.conversation_type
    AND v.conversation_id=r.conversation_id AND v.author_id=r.sender_id AND v.target_sent_at=r.sent_at_ns
    AND v.actor_id=r.user_id AND v.emoji=r.emoji AND v.action_ts>0
);
INSERT INTO reaction_actor_selections(conversation_id,conversation_type,author_id,target_sent_at,actor_id,emoji,action_ts)
SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id,emoji,action_ts FROM (
    SELECT v.*,ROW_NUMBER() OVER (
        PARTITION BY conversation_type,conversation_id,author_id,target_sent_at,actor_id
        ORDER BY action_ts DESC,emoji DESC
    ) AS position FROM reaction_action_versions v WHERE remove=0 AND action_ts>0
) WHERE position=1
ON CONFLICT(conversation_type,conversation_id,author_id,target_sent_at,actor_id)
DO UPDATE SET emoji=excluded.emoji,action_ts=excluded.action_ts
WHERE excluded.action_ts>reaction_actor_selections.action_ts
OR (excluded.action_ts=reaction_actor_selections.action_ts AND excluded.emoji>reaction_actor_selections.emoji);
INSERT INTO reaction_scoped_quarantine SELECT r.* FROM unified_reactions AS r WHERE NOT EXISTS (
    SELECT 1 FROM reaction_actor_selections s JOIN reaction_action_versions v
    ON v.conversation_type=s.conversation_type AND v.conversation_id=s.conversation_id
    AND v.author_id=s.author_id AND v.target_sent_at=s.target_sent_at AND v.actor_id=s.actor_id AND v.emoji=s.emoji
    WHERE s.conversation_type=r.conversation_type AND s.conversation_id=r.conversation_id
    AND s.author_id=r.sender_id AND s.target_sent_at=r.sent_at_ns AND s.actor_id=r.user_id
    AND s.emoji=r.emoji AND v.remove=0 AND v.action_ts=s.action_ts
) ON CONFLICT DO NOTHING;
DELETE FROM unified_reactions AS r WHERE NOT EXISTS (
    SELECT 1 FROM reaction_actor_selections s JOIN reaction_action_versions v
    ON v.conversation_type=s.conversation_type AND v.conversation_id=s.conversation_id
    AND v.author_id=s.author_id AND v.target_sent_at=s.target_sent_at AND v.actor_id=s.actor_id AND v.emoji=s.emoji
    WHERE s.conversation_type=r.conversation_type AND s.conversation_id=r.conversation_id
    AND s.author_id=r.sender_id AND s.target_sent_at=r.sent_at_ns AND s.actor_id=r.user_id
    AND s.emoji=r.emoji AND v.remove=0 AND v.action_ts=s.action_ts
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_ur_single_actor ON unified_reactions
    (conversation_type,conversation_id,sender_id,sent_at_ns,user_id);
"#;

pub(crate) fn normalize(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(QUARANTINE_SCHEMA)?;
    conn.execute_batch(NORMALIZE_SQL)?;
    rebuild_all(conn)
}

/// Include resolved targets with zero surviving votes, clearing blob-only stale caches.
/// Unresolved legacy scopes stay masked and keep their raw historical blobs.
/// Change only the two projections: message/draft bodies and timestamps survive.
pub(crate) fn rebuild_all(conn: &Connection) -> Result<(), StorageError> {
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
    let mut stmt = conn
        .prepare("SELECT id,conversation_id,conversation_type,sender_id,sent_at FROM messages m WHERE NOT EXISTS(SELECT 1 FROM reaction_scope_unresolved u WHERE u.conversation_id=m.conversation_id AND u.conversation_type=m.conversation_type AND u.sender_id=m.sender_id AND u.sent_at_ns=m.sent_at)")?;
    let targets = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, scope, kind, author, clock) in targets {
        let mut stmt=conn.prepare("SELECT emoji,COUNT(*) FROM unified_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4 GROUP BY emoji ORDER BY emoji")?;
        let counts = stmt
            .query_map(params![scope, kind, author, clock], |r| {
                Ok(ReactionCount::new(
                    r.get::<_, String>(0)?,
                    r.get::<_, u32>(1)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut stmt=conn.prepare("SELECT emoji FROM unified_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at_ns=?4 AND user_id=?5 ORDER BY emoji")?;
        let emojis = stmt
            .query_map(params![scope, kind, author, clock, own], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        conn.execute("UPDATE messages SET reaction_counts_blob=?1,current_user_reactions_blob=?2 WHERE id=?3",params![db_serialize(&counts)?,db_serialize(&emojis)?,id])?;
    }
    Ok(())
}

/// Older backups have only the compressed per-emoji queue. Seed visible adds,
/// without ever treating a held removal as the actor's latest selection.
pub(crate) fn normalize_pending(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(r#"INSERT INTO pending_reaction_add_selections
        SELECT conversation_id,conversation_type,author_id,target_sent_at,actor_id,emoji,action_ts FROM (
            SELECT *,ROW_NUMBER() OVER (PARTITION BY conversation_type,conversation_id,author_id,target_sent_at,actor_id ORDER BY action_ts DESC,emoji DESC) AS position
            FROM pending_scoped_reactions WHERE remove=0 AND action_ts>0
        ) WHERE position=1 ON CONFLICT(conversation_type,conversation_id,author_id,target_sent_at,actor_id)
        DO UPDATE SET emoji=excluded.emoji,action_ts=excluded.action_ts
        WHERE excluded.action_ts>pending_reaction_add_selections.action_ts
        OR (excluded.action_ts=pending_reaction_add_selections.action_ts AND excluded.emoji>pending_reaction_add_selections.emoji);"#)?;
    Ok(())
}
