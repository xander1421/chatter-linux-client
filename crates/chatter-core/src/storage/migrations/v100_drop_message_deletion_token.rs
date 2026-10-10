//! Drop `messages.deletion_token` (v079). Channel posts are always authored
//! now and deleted by their author; nothing seals a channel post or keeps a
//! token to delete one.
use super::Migration;

pub struct V100DropMessageDeletionToken;
impl Migration for V100DropMessageDeletionToken {
    fn version(&self) -> u32 {
        100
    }
    fn description(&self) -> &'static str {
        "Drop the unused sealed-sender deletion token column"
    }
    fn up(&self) -> &'static str {
        "ALTER TABLE messages DROP COLUMN deletion_token;"
    }
}
