//! A logged pairwise send remembers the sealed-sender group id it went out
//! with, so the resend a `DecryptionErrorMessage` asks for names the same
//! channel (OPEN-ITEMS §5): a copy that fails again is then marked in that
//! channel, not in the DM with the sender. NULL for a 1:1 DM.
use super::Migration;

pub struct V112SendLogGroupId;
impl Migration for V112SendLogGroupId {
    fn version(&self) -> u32 {
        112
    }
    fn description(&self) -> &'static str {
        "Send log keeps the sealed-sender group id of each payload"
    }
    fn up(&self) -> &'static str {
        "ALTER TABLE send_log_payloads ADD COLUMN group_id BLOB;"
    }
}
