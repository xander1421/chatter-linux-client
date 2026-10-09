//! When a sealed DM's stored envelope was sealed, so a replay never outlives
//! the sender certificate inside it.
use super::Migration;

pub struct V098DmOutboxSealedAt;
impl Migration for V098DmOutboxSealedAt {
    fn version(&self) -> u32 {
        98
    }
    fn description(&self) -> &'static str {
        "Sealed DM outbox envelope seal time"
    }
    fn up(&self) -> &'static str {
        // Unix seconds the first stored `request` was sealed. NULL with
        // `request`; a replay past the certificate window re-seals instead.
        "ALTER TABLE dm_outbox ADD COLUMN sealed_at INTEGER;"
    }
}
