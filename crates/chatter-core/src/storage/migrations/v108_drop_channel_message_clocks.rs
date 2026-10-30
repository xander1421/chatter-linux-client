//! Drop `channel_message_clocks` and its insert guard (v086). The table mapped
//! an authenticated DataMessage clock onto a channel row stored under the
//! server transport clock by an older deferred receive. Every current channel
//! insert path stores the authenticated clock, so nothing writes or reads it.
use super::Migration;

pub struct V108DropChannelMessageClocks;
impl Migration for V108DropChannelMessageClocks {
    fn version(&self) -> u32 {
        108
    }
    fn description(&self) -> &'static str {
        "Drop the legacy channel clock aliases"
    }
    fn up(&self) -> &'static str {
        "DROP TRIGGER IF EXISTS guard_channel_clock_collision;
         DROP TABLE IF EXISTS channel_message_clocks;"
    }
}
