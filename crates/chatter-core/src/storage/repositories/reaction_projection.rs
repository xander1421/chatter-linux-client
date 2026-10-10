//! Rebuild both reaction projections on every stored message from the vote
//! table. Used after a backup restore merges votes into existing history.
use crate::{
    domain::message::ReactionCount, error::StorageError, storage::serialization::db_serialize,
};
use rusqlite::{Connection, OptionalExtension, params};

/// Include targets with zero surviving votes, clearing stale caches.
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
    let mut stmt =
        conn.prepare("SELECT id,conversation_id,conversation_type,sender_id,sent_at FROM messages")?;
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
