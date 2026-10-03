//! V082: verified channel membership (signed, hash-chained change log).
//!
//! One row per channel in `verified_channel_state` (revision, hash, policy,
//! timer, local verification status), its members and invite capabilities,
//! the users excluded from key distribution by an unsigned server removal,
//! and every accepted `ChannelChange` with the state hash it produced (for
//! fork checks against a peer at an older revision).
//!
//! u64 values (revision, expires_at) are stored bit-cast to INTEGER.

use super::Migration;

pub struct V082VerifiedChannelMembership;

impl Migration for V082VerifiedChannelMembership {
    fn version(&self) -> u32 {
        82
    }

    fn description(&self) -> &'static str {
        "Verified channel membership state and change log"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE verified_channel_state (
            channel_id TEXT PRIMARY KEY,
            revision INTEGER NOT NULL,
            state_hash BLOB NOT NULL,
            policy_add_member INTEGER NOT NULL,
            policy_remove_member INTEGER NOT NULL,
            policy_set_role INTEGER NOT NULL,
            policy_manage_invites INTEGER NOT NULL,
            policy_set_disappearing INTEGER NOT NULL,
            policy_join INTEGER NOT NULL,
            disappearing_seconds INTEGER NOT NULL,
            status INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE verified_channel_members (
            channel_id TEXT NOT NULL REFERENCES verified_channel_state(channel_id) ON DELETE CASCADE,
            user_id TEXT NOT NULL,
            role INTEGER NOT NULL,
            status INTEGER NOT NULL,
            identity_key BLOB NOT NULL,
            PRIMARY KEY (channel_id, user_id)
        ) WITHOUT ROWID;
        CREATE TABLE verified_channel_invites (
            channel_id TEXT NOT NULL REFERENCES verified_channel_state(channel_id) ON DELETE CASCADE,
            invite_pub BLOB NOT NULL,
            role INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            max_uses INTEGER NOT NULL,
            uses INTEGER NOT NULL,
            needs_approval INTEGER NOT NULL,
            PRIMARY KEY (channel_id, invite_pub)
        ) WITHOUT ROWID;
        CREATE TABLE verified_channel_exclusions (
            channel_id TEXT NOT NULL REFERENCES verified_channel_state(channel_id) ON DELETE CASCADE,
            user_id TEXT NOT NULL,
            PRIMARY KEY (channel_id, user_id)
        ) WITHOUT ROWID;
        CREATE TABLE channel_changes (
            channel_id TEXT NOT NULL REFERENCES verified_channel_state(channel_id) ON DELETE CASCADE,
            revision INTEGER NOT NULL,
            actions BLOB NOT NULL,
            signature BLOB NOT NULL,
            invite_proof BLOB NOT NULL,
            state_hash BLOB NOT NULL,
            PRIMARY KEY (channel_id, revision)
        ) WITHOUT ROWID;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP TABLE channel_changes;
            DROP TABLE verified_channel_exclusions;
            DROP TABLE verified_channel_invites;
            DROP TABLE verified_channel_members;
            DROP TABLE verified_channel_state;
            "#,
        )
    }
}
