//! Migration v016: Add sent_at for Signal-style deduplication.
//!
//! NOTE: This column is now included in v004_messages. This migration
//! is kept as a no-op for version number continuity.

use super::Migration;

pub struct V016MessageSentAt;

impl Migration for V016MessageSentAt {
    fn version(&self) -> u32 {
        16
    }

    fn description(&self) -> &'static str {
        "Add sent_at column and unique index for Signal-style deduplication"
    }

    fn up(&self) -> &'static str {
        // No-op: sent_at is now included in v004_messages base schema
        "SELECT 1;"
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
