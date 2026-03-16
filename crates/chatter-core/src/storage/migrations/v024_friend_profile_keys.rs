//! Migration v024: Friend profile keys.
//!
//! Adds columns to store friend profile keys for E2E profile decryption,
//! and a flag to track whether profile sharing is enabled per friendship.

use super::Migration;

pub struct V024FriendProfileKeys;

impl Migration for V024FriendProfileKeys {
    fn version(&self) -> u32 {
        24
    }

    fn description(&self) -> &'static str {
        "Add friend_profile_key and profile_sharing to friendships"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Friend's profile key for decrypting their encrypted display_name
        -- 32 bytes, received via E2E message when they share it with us
        ALTER TABLE friendships ADD COLUMN friend_profile_key BLOB;

        -- Whether we share our profile key with this friend
        -- 0 = not sharing, 1 = sharing (auto-enabled on first message)
        ALTER TABLE friendships ADD COLUMN profile_sharing INTEGER NOT NULL DEFAULT 0;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        // SQLite doesn't support DROP COLUMN in older versions,
        // but newer SQLite (3.35.0+) does. For safety, we recreate the table.
        Some(
            r#"
            -- Create temporary table without the new columns
            CREATE TABLE friendships_backup (
                user_id TEXT NOT NULL,
                friend_id TEXT NOT NULL,
                status INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (user_id, friend_id)
            );

            -- Copy data
            INSERT INTO friendships_backup SELECT user_id, friend_id, status, created_at FROM friendships;

            -- Drop original table
            DROP TABLE friendships;

            -- Rename backup to original
            ALTER TABLE friendships_backup RENAME TO friendships;
            "#,
        )
    }
}
