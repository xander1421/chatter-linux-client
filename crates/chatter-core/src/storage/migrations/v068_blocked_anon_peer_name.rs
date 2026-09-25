//! V068: Keep a blocked anon peer's name after their chat is deleted.
//!
//! An anon peer has no profile: the only place their name lives is the
//! `anon_dm_conversations` row. Deleting a blocked peer's chat (Signal's
//! Delete on a blocked chat) drops that row while the block stays, so the
//! Friends › Blocked row fell back to a placeholder. The block row now carries
//! the last known name, written when the chat is deleted.

use super::Migration;

pub struct V068BlockedAnonPeerName;

impl Migration for V068BlockedAnonPeerName {
    fn version(&self) -> u32 {
        68
    }

    fn description(&self) -> &'static str {
        "Blocked anon peers keep their display name"
    }

    fn up(&self) -> &'static str {
        "ALTER TABLE anon_dm_blocked_users ADD COLUMN display_name TEXT;"
    }

    fn down(&self) -> Option<&'static str> {
        Some("ALTER TABLE anon_dm_blocked_users DROP COLUMN display_name;")
    }
}
