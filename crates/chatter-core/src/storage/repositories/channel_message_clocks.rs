//! Compatibility is learned only from an authenticated channel message with an
//! exact server UUID, channel, kind and author. The retained row clock, ledger,
//! and already encrypted outgoing controls remain immutable.
use super::*;

impl UnifiedMessageRepository {
    pub(crate) fn bind_authenticated_channel_clock(
        &self,
        message: &Message,
    ) -> Result<(), StorageError> {
        if !matches!(
            message.conversation_type,
            ConversationType::Channel | ConversationType::AnonGroup
        ) {
            return Err(StorageError::InvalidData(
                "Non-channel clock binding".into(),
            ));
        }
        let author = message
            .sender_id
            .ok_or_else(|| StorageError::InvalidData("Clock binding requires author".into()))?;
        if message.sent_at == 0 || message.sent_at > i64::MAX as u64 {
            return Err(StorageError::InvalidData(
                "Invalid authenticated channel clock".into(),
            ));
        }
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let existing = tx
            .query_row(
                &format!(
                    "SELECT {} FROM messages WHERE id=?1",
                    message_columns("messages")
                ),
                [message.id.to_string()],
                row_to_message,
            )
            .optional()?
            .transpose()?;
        let Some(existing) = existing else {
            return Ok(());
        };
        if existing.conversation_id != message.conversation_id
            || existing.conversation_type != message.conversation_type
            || existing.sender_id != Some(author)
            || existing.thread_id != message.thread_id
            || existing.is_deleted
        {
            return Err(StorageError::InvalidData(
                "Authenticated channel clock identity mismatch".into(),
            ));
        }
        let scope = ReactionScope::of(&existing);
        let canonical = message.sent_at;
        let bound: Option<i64> = tx
            .query_row(
                "SELECT authenticated_sent_at FROM channel_message_clocks WHERE message_id=?1",
                [message.id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        if bound.is_some_and(|clock| clock as u64 != canonical) {
            return Err(StorageError::InvalidData(
                "Authenticated channel clock is immutable".into(),
            ));
        }
        let collision: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND id<>?5)",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),canonical as i64,message.id.to_string()],|r|r.get(0))?;
        if collision {
            return Err(StorageError::InvalidData(
                "Ambiguous authenticated channel clock".into(),
            ));
        }
        tx.execute("INSERT INTO channel_message_clocks VALUES(?1,?2,?3,?4,?5) ON CONFLICT(message_id) DO NOTHING",params![message.id.to_string(),scope.id.to_string(),scope.kind.as_str(),author.to_string(),canonical as i64])?;
        // A reaction arriving before this exact mapping remains durable. Merge
        // it into the retained clock by action version, without touching any
        // outgoing ciphertext or manufacturing a target for an unknown UUID.
        if canonical != existing.sent_at {
            tx.execute("INSERT INTO pending_scoped_reactions SELECT conversation_id,conversation_type,author_id,?5,actor_id,emoji,remove,action_ts,received_at FROM pending_scoped_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4
                ON CONFLICT(conversation_type,conversation_id,author_id,target_sent_at,actor_id,emoji) DO UPDATE SET remove=excluded.remove,action_ts=excluded.action_ts,received_at=excluded.received_at WHERE excluded.action_ts>pending_scoped_reactions.action_ts OR (excluded.action_ts=pending_scoped_reactions.action_ts AND excluded.remove>pending_scoped_reactions.remove)",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),canonical as i64,existing.sent_at as i64])?;
            tx.execute("DELETE FROM pending_scoped_reactions WHERE conversation_id=?1 AND conversation_type=?2 AND author_id=?3 AND target_sent_at=?4",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),canonical as i64])?;
        }
        self.apply_held_reaction_scope_with_conn(&tx, &scope, &author, existing.sent_at)?;
        tx.commit()?;
        Ok(())
    }

    /// Map only a recorded exact authenticated clock, in this exact scope.
    pub fn resolve_channel_reaction_clock(
        &self,
        scope: &ReactionScope,
        author: &UserId,
        clock: u64,
    ) -> Result<u64, StorageError> {
        let conn = self.db.read_connection()?;
        Self::resolve_channel_reaction_clock_with_conn(&conn, scope, author, clock)
    }

    pub(super) fn resolve_channel_reaction_clock_with_conn(
        conn: &rusqlite::Connection,
        scope: &ReactionScope,
        author: &UserId,
        clock: u64,
    ) -> Result<u64, StorageError> {
        let mapped: Option<i64> = conn.query_row("SELECT m.sent_at FROM channel_message_clocks a JOIN messages m ON m.id=a.message_id AND m.conversation_id=a.conversation_id AND m.conversation_type=a.conversation_type AND m.sender_id=a.author_id WHERE a.conversation_id=?1 AND a.conversation_type=?2 AND a.author_id=?3 AND a.authenticated_sent_at=?4 AND m.is_deleted=0",params![scope.id.to_string(),scope.kind.as_str(),author.to_string(),clock as i64],|r|r.get(0)).optional()?;
        Ok(mapped.map(|v| v as u64).unwrap_or(clock))
    }

    /// New controls use the authenticated clock even for a retained legacy row.
    pub fn authenticated_channel_reaction_clock(
        &self,
        message: &Message,
    ) -> Result<u64, StorageError> {
        let conn = self.db.read_connection()?;
        let clock: Option<i64> = conn.query_row("SELECT authenticated_sent_at FROM channel_message_clocks WHERE message_id=?1 AND conversation_id=?2 AND conversation_type=?3 AND author_id=?4",params![message.id.to_string(),message.conversation_id.to_string(),message.conversation_type.as_str(),message.sender_id.map(|id|id.to_string())],|r|r.get(0)).optional()?;
        Ok(clock.map(|v| v as u64).unwrap_or(message.sent_at))
    }
}
