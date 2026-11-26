use super::Migration;

pub struct V145DropPendingBlinds;

impl Migration for V145DropPendingBlinds {
    fn version(&self) -> u32 {
        145
    }

    fn description(&self) -> &'static str {
        "Drop obsolete mailbox pending blinds"
    }

    fn up(&self) -> &'static str {
        "DROP TABLE IF EXISTS mailbox_pending_blinds;"
    }
}
