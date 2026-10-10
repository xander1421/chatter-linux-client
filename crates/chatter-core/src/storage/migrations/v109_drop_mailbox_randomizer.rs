//! Drop the blind-signature message randomizer columns. Tokens are blinded
//! deterministically (`blind_key` passes `include_randomizer = false`), so no
//! randomizer ever exists: every row held NULL and nothing verified with it.
use super::Migration;

pub struct V109DropMailboxRandomizer;
impl Migration for V109DropMailboxRandomizer {
    fn version(&self) -> u32 {
        109
    }
    fn description(&self) -> &'static str {
        "Drop the always-empty mailbox token randomizer columns"
    }
    fn up(&self) -> &'static str {
        "ALTER TABLE mailbox_pending_blinds DROP COLUMN message_randomizer;
         ALTER TABLE mailbox_tokens DROP COLUMN message_randomizer;
         ALTER TABLE dm_mailboxes DROP COLUMN randomizer;
         ALTER TABLE group_mailboxes DROP COLUMN randomizer;"
    }
}
