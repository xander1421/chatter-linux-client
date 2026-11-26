use super::Migration;

pub struct V146SignedChannelType;

impl Migration for V146SignedChannelType {
    fn version(&self) -> u32 {
        146
    }

    fn description(&self) -> &'static str {
        "Bind channel routing to the signed membership state"
    }

    fn up(&self) -> &'static str {
        "ALTER TABLE verified_channel_state ADD COLUMN channel_type INTEGER NOT NULL DEFAULT 0;
         CREATE TABLE channel_type_pins (
             channel_id TEXT PRIMARY KEY NOT NULL,
             channel_type INTEGER NOT NULL CHECK (channel_type IN (0, 1, 3))
         );"
    }
}
