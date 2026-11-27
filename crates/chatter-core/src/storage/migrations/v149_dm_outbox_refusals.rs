//! Whether a queued sealed DM's envelope can have landed: a UAK the server
//! rejected was refused before anything was queued, so an entry whose every
//! sending attempt was such a refusal fails as definitely not delivered,
//! never "Delivery unconfirmed" (`dm_outbox::attempt_verdict`).
use super::Migration;

pub struct V149DmOutboxRefusals;
impl Migration for V149DmOutboxRefusals {
    fn version(&self) -> u32 {
        149
    }
    fn description(&self) -> &'static str {
        "Sealed DM outbox UAK refusals"
    }
    fn up(&self) -> &'static str {
        "ALTER TABLE dm_outbox ADD COLUMN key_refused INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE dm_outbox ADD COLUMN may_have_landed INTEGER NOT NULL DEFAULT 0;"
    }
}
