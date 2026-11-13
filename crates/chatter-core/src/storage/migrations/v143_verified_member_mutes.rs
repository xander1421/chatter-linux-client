//! V143: mutes move into the signed membership state (`StateMember.muted_until`,
//! canonical format 2). Every stored state was hashed under format 1 and no
//! longer matches its hash, so the verified states and their logs are dropped
//! and re-bootstrapped from the server (pre-release: no data is migrated).

use super::Migration;

pub struct V143VerifiedMemberMutes;

impl Migration for V143VerifiedMemberMutes {
    fn version(&self) -> u32 {
        143
    }

    fn description(&self) -> &'static str {
        "Signed member mutes in the verified channel state"
    }

    fn up(&self) -> &'static str {
        r#"
        DELETE FROM channel_changes;
        DELETE FROM verified_channel_exclusions;
        DELETE FROM verified_channel_invites;
        DELETE FROM verified_channel_members;
        DELETE FROM verified_channel_state;
        ALTER TABLE verified_channel_members ADD COLUMN muted_until INTEGER NOT NULL DEFAULT 0;
        "#
    }
}
