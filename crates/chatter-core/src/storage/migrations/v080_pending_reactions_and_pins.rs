//! Reactions and pins that arrived before their target join the edits and
//! deletes in `pending_message_modifiers` (`message_modifier_repo`).
//!
//! A reaction is held per reactor and emoji and a pin per pinner, so the table
//! is keyed by actor and emoji too; `edit_ts` becomes `action_ts`, the time of
//! whichever action a row holds. Rows are disposable (each waits at most 30
//! days for a message that may never come), so the table is rebuilt.
use super::Migration;
pub struct V080PendingReactionsAndPins;
impl Migration for V080PendingReactionsAndPins {
    fn version(&self) -> u32 {
        80
    }
    fn description(&self) -> &'static str {
        "Hold early reactions and pins with pending modifiers"
    }
    fn up(&self) -> &'static str {
        r#"
        DROP TABLE pending_message_modifiers;
        CREATE TABLE pending_message_modifiers (
            author_id TEXT NOT NULL,
            target_sent_at INTEGER NOT NULL,
            kind TEXT NOT NULL CHECK (kind IN ('edit', 'delete', 'reaction', 'pin')),
            actor_id TEXT NOT NULL DEFAULT '',
            emoji TEXT NOT NULL DEFAULT '',
            conversation_id TEXT,
            body TEXT,
            action_ts INTEGER NOT NULL DEFAULT 0,
            remove INTEGER NOT NULL DEFAULT 0,
            received_at INTEGER NOT NULL,
            PRIMARY KEY (author_id, target_sent_at, kind, actor_id, emoji)
        );
        CREATE INDEX pending_message_modifiers_received ON pending_message_modifiers(received_at);
        "#
    }
    fn down(&self) -> Option<&'static str> {
        None
    }
}
