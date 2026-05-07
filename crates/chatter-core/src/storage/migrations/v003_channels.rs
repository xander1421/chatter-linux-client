//! Channel tables migration.
//!
//! Creates tables for channels and memberships.

use super::Migration;

/// Channel tables migration (v003).
///
/// Creates the channel-related tables:
/// - `channels`: Channel metadata with flattened config
/// - `memberships`: User memberships in channels with roles and preferences
pub struct V003Channels;

impl Migration for V003Channels {
    fn version(&self) -> u32 {
        3
    }

    fn description(&self) -> &'static str {
        "Channel tables - channels, memberships"
    }

    fn up(&self) -> &'static str {
        r#"
        -- Channels
        CREATE TABLE channels (
            id TEXT PRIMARY KEY,
            channel_type INTEGER NOT NULL,
            name TEXT NOT NULL,
            description TEXT,
            -- Historical column. v054 renames + retypes this to `icon BLOB`.
            -- Keep the old name here so upgrade paths don't double-create
            -- the column.
            image_url TEXT,
            owner_id TEXT NOT NULL,
            -- Config fields (flattened for simplicity)
            persistence INTEGER NOT NULL DEFAULT 0,
            max_members INTEGER,
            default_ttl_seconds INTEGER,
            allow_reactions INTEGER NOT NULL DEFAULT 1,
            allow_threads INTEGER NOT NULL DEFAULT 1,
            allow_attachments INTEGER NOT NULL DEFAULT 1,
            require_encryption INTEGER NOT NULL DEFAULT 1,
            -- Timestamps and state
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            archived INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX idx_channels_owner ON channels(owner_id);

        -- Channel memberships
        CREATE TABLE memberships (
            channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            user_id TEXT NOT NULL,
            role INTEGER NOT NULL DEFAULT 0,
            joined_at TEXT NOT NULL,
            last_read_message_id TEXT,
            is_muted INTEGER NOT NULL DEFAULT 0,
            muted_until TEXT,
            notification_preference INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (channel_id, user_id)
        );
        CREATE INDEX idx_memberships_user ON memberships(user_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_memberships_user;
            DROP TABLE IF EXISTS memberships;
            DROP INDEX IF EXISTS idx_channels_owner;
            DROP TABLE IF EXISTS channels;
            "#,
        )
    }
}
