//! Drop the pre-v084 reaction evidence tables. They held unscoped votes from
//! databases and backups older than scoped reactions; nothing writes them on a
//! current install, so they are always empty and every reader of them was dead
//! weight. The two message-delete triggers that touched them are recreated
//! without those statements.
use super::Migration;

pub struct V105DropLegacyReactionEvidence;
impl Migration for V105DropLegacyReactionEvidence {
    fn version(&self) -> u32 {
        105
    }
    fn description(&self) -> &'static str {
        "Drop legacy unscoped reaction evidence and quarantine tables"
    }
    fn up(&self) -> &'static str {
        r#"
        DROP TRIGGER IF EXISTS mark_legacy_reaction_ambiguity;
        DROP TRIGGER IF EXISTS delete_scoped_message_reactions;
        DROP TRIGGER IF EXISTS delete_message_reaction_quarantine;
        DROP TABLE IF EXISTS reaction_legacy_unscoped;
        DROP TABLE IF EXISTS reaction_legacy_pending;
        DROP TABLE IF EXISTS reaction_scope_unresolved;
        DROP TABLE IF EXISTS reaction_scoped_quarantine;
        CREATE TRIGGER delete_scoped_message_reactions AFTER DELETE ON messages
        BEGIN
            DELETE FROM unified_reactions WHERE conversation_id=OLD.conversation_id AND conversation_type=OLD.conversation_type AND sender_id=OLD.sender_id AND sent_at_ns=OLD.sent_at;
            DELETE FROM pending_scoped_reactions WHERE conversation_id=OLD.conversation_id AND conversation_type=OLD.conversation_type AND author_id=OLD.sender_id AND target_sent_at=OLD.sent_at;
            DELETE FROM pending_reaction_add_selections WHERE conversation_id=OLD.conversation_id AND conversation_type=OLD.conversation_type AND author_id=OLD.sender_id AND target_sent_at=OLD.sent_at;
        END;
        "#
    }
}
