//! V073: Channels we are no longer in stay listed, read-only.
//!
//! Signal keeps a group you left or were removed from in the chat list with
//! its history and the reason line; it goes only when you delete it. Our own
//! membership row is what lists a channel, and a departure deletes it, so the
//! row vanished with it. This table remembers the channels we used to be in
//! without faking a membership; `deleted` marks one its owner deleted.

use super::Migration;

pub struct V073FormerChannels;

impl Migration for V073FormerChannels {
    fn version(&self) -> u32 {
        73
    }

    fn description(&self) -> &'static str {
        "Keep channels we left or were removed from, read-only"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE former_channels (
            channel_id TEXT PRIMARY KEY NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            deleted INTEGER NOT NULL DEFAULT 0
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS former_channels;")
    }
}
