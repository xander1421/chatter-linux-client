//! Migration v030: Add peer_avatar to anon_dm_conversations.
//!
//! Adds a column to store the peer's avatar for anonymous DMs.
//! Avatar is shared encrypted during bootstrap and stored locally.

use super::Migration;

pub struct V030AnonDmPeerAvatar;

impl Migration for V030AnonDmPeerAvatar {
    fn version(&self) -> u32 {
        30
    }

    fn description(&self) -> &'static str {
        "Add peer_avatar to anonymous DM conversations"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE anon_dm_conversations ADD COLUMN peer_avatar BLOB;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
