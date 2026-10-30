//! V072: Profile keys are held per peer, not per friendship.
//!
//! Signal keeps a profile key on the recipient, whatever the relationship: a
//! group member who is not a contact still delivers theirs with the group
//! (`handleDataMessage` stores any sender's key). Keyed on the friendship row,
//! a channel co-member's key had nowhere to land, so non-friend members only
//! ever rendered as "User <id>". Clean break: the friendship column goes.

use super::Migration;

pub struct V072PeerProfileKeys;

impl Migration for V072PeerProfileKeys {
    fn version(&self) -> u32 {
        72
    }

    fn description(&self) -> &'static str {
        "Store peer profile keys per user, not per friendship"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE peer_profile_keys (
            user_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            profile_key BLOB NOT NULL,
            PRIMARY KEY (user_id, peer_id)
        );
        ALTER TABLE friendships DROP COLUMN friend_profile_key;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            ALTER TABLE friendships ADD COLUMN friend_profile_key BLOB;
            DROP TABLE IF EXISTS peer_profile_keys;
            "#,
        )
    }
}
