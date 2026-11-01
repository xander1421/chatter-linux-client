use super::*;

impl UnifiedMessageRepository {
    /// The next main-list page after a cursor in this conversation. Match the
    /// existing older-page order exactly (sent timestamp, then sender ID).
    pub fn list_newer_for_channel_asc(
        &self,
        conversation: &ChannelId,
        kind: &ConversationType,
        after: &MessageId,
        limit: u32,
    ) -> Result<Vec<Message>, StorageError> {
        let conn = self.db.read_connection()?;
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1 AND conversation_id=?2 AND conversation_type=?3 AND thread_id IS NULL)",
            params![after.to_string(),conversation.to_string(),kind.as_str()], |row| row.get(0))?;
        if !exists {
            return Err(StorageError::NotFound("history cursor".into()));
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM messages m
             WHERE m.conversation_id=?1 AND m.conversation_type=?2 AND m.thread_id IS NULL
               AND (m.sent_at,m.sender_id) >
                 (SELECT sent_at,sender_id FROM messages
                  WHERE id=?3 AND conversation_id=?1 AND conversation_type=?2 AND thread_id IS NULL)
             ORDER BY m.sent_at ASC,m.sender_id ASC LIMIT ?4",
            message_columns("m")
        ))?;
        let mut rows = stmt.query(params![
            conversation.to_string(),
            kind.as_str(),
            after.to_string(),
            limit
        ])?;
        collect_messages(&mut rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn newer_pages_match_older_order_and_reject_foreign_or_missing_cursors() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("history.db"), "fixture-only").unwrap());
        db.run_migrations().unwrap();
        let repo = UnifiedMessageRepository::new(db);
        let conversation = ChannelId::new();
        let kind = ConversationType::Channel;
        for sent_at in [100, 200, 200, 300, 400] {
            repo.insert(&Message::new_text(
                conversation,
                kind.clone(),
                Some(UserId::new()),
                "row".into(),
                sent_at,
            ))
            .unwrap();
        }
        let all = repo
            .list_for_channel_asc(&conversation, &kind, None, 20)
            .unwrap();
        let mut reply = Message::new_text(
            conversation,
            kind.clone(),
            Some(UserId::new()),
            "reply".into(),
            250,
        );
        reply.thread_id = Some(all[0].id);
        repo.insert(&reply).unwrap();
        let page = repo
            .list_newer_for_channel_asc(&conversation, &kind, &all[1].id, 2)
            .unwrap();
        assert_eq!(
            page.iter().map(|m| m.id).collect::<Vec<_>>(),
            all[2..4].iter().map(|m| m.id).collect::<Vec<_>>()
        );
        assert!(
            repo.list_newer_for_channel_asc(&ChannelId::new(), &kind, &all[1].id, 2)
                .is_err()
        );
        assert!(
            repo.list_newer_for_channel_asc(&conversation, &kind, &MessageId::new(), 2)
                .is_err()
        );
        assert!(
            repo.list_newer_for_channel_asc(&conversation, &kind, &reply.id, 2)
                .is_err()
        );
    }
}
