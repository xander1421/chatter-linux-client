use super::Migration;
/// Exact authenticated clocks for retained channel rows; never populated by clock proximity.
pub struct V086ChannelMessageClocks;
impl Migration for V086ChannelMessageClocks {
    fn version(&self) -> u32 {
        86
    }
    fn description(&self) -> &'static str {
        "Authenticated channel message clock aliases"
    }
    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE channel_message_clocks (
            message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
            conversation_id TEXT NOT NULL, conversation_type TEXT NOT NULL,
            author_id TEXT NOT NULL, authenticated_sent_at INTEGER NOT NULL CHECK(authenticated_sent_at > 0),
            UNIQUE(conversation_id,conversation_type,author_id,authenticated_sent_at)
        );
        CREATE TRIGGER guard_channel_clock_collision BEFORE INSERT ON messages
        WHEN EXISTS(SELECT 1 FROM channel_message_clocks a WHERE a.conversation_id=NEW.conversation_id
            AND a.conversation_type=NEW.conversation_type AND a.author_id=NEW.sender_id
            AND a.authenticated_sent_at=NEW.sent_at AND a.message_id<>NEW.id)
        BEGIN SELECT RAISE(ABORT,'Authenticated channel clock collision'); END;
    "#
    }
}
