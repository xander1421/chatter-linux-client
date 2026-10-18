use super::*;

impl UnifiedMessageRepository {
    // ========================================================================
    // Internal helpers
    // ========================================================================

    /// Rebuild `reaction_counts_blob` and `current_user_reactions_blob` on the
    /// message row from the `unified_reactions` table (source of truth).
    /// Resolves the message row via (sender_id, sent_at) in the messages table.
    pub(super) fn rebuild_reaction_blobs_with_conn(
        &self,
        conn: &rusqlite::Connection,
        scope: &ReactionScope,
        sender_id: &UserId,
        sent_at_ns: u64,
        current_user_id: &UserId,
    ) -> Result<(), StorageError> {
        let sid = sender_id.to_string();
        let uid = current_user_id.to_string();

        // Reaction counts
        let mut stmt = conn.prepare(
            "SELECT emoji, COUNT(*) FROM unified_reactions
             WHERE sender_id = ?1 AND sent_at_ns = ?2 AND conversation_id=?3 AND conversation_type=?4 GROUP BY emoji",
        )?;
        let mut rows = stmt.query(params![
            &sid,
            sent_at_ns as i64,
            scope.id.to_string(),
            scope.kind.as_str()
        ])?;
        let mut counts: Vec<ReactionCount> = Vec::new();
        while let Some(row) = rows.next()? {
            let emoji_str: String = row.get(0)?;
            let count: u32 = row.get(1)?;
            counts.push(ReactionCount::new(emoji_str, count));
        }
        drop(rows);
        drop(stmt);

        // Current user's reactions
        let mut stmt2 = conn.prepare(
            "SELECT emoji FROM unified_reactions
             WHERE sender_id = ?1 AND sent_at_ns = ?2 AND user_id = ?3 AND conversation_id=?4 AND conversation_type=?5",
        )?;
        let mut rows2 = stmt2.query(params![
            &sid,
            sent_at_ns as i64,
            &uid,
            scope.id.to_string(),
            scope.kind.as_str()
        ])?;
        let mut user_reactions: Vec<String> = Vec::new();
        while let Some(row) = rows2.next()? {
            user_reactions.push(row.get(0)?);
        }
        drop(rows2);
        drop(stmt2);

        let counts_blob = db_serialize(&counts)?;
        let user_blob = db_serialize(&user_reactions)?;

        // Update the message row, resolving via (sender_id, sent_at)
        conn.execute(
            "UPDATE messages SET reaction_counts_blob = ?1, current_user_reactions_blob = ?2, updated_at = ?3
             WHERE sender_id = ?4 AND sent_at = ?5 AND is_deleted = 0 AND conversation_id=?6 AND conversation_type=?7",
            params![counts_blob, user_blob, Utc::now().to_rfc3339(), &sid, sent_at_ns as i64, scope.id.to_string(), scope.kind.as_str()],
        )?;

        Ok(())
    }
}

