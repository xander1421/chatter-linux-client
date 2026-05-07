//! V054: Channel icon column type swap.
//!
//! Replaces the `channels.image_url` TEXT column (which used to hold a
//! CDN key) with `channels.icon` BLOB (raw image bytes ≤ 64KB).
//!
//! This is a destructive swap — any existing icon URLs are dropped. The
//! previous implementation never actually distributed the per-attachment
//! encryption key, so other members couldn't decrypt the icon anyway,
//! making the data unusable cross-device.

use super::Migration;

pub struct V054ChannelIconBlob;

impl Migration for V054ChannelIconBlob {
    fn version(&self) -> u32 {
        54
    }

    fn description(&self) -> &'static str {
        "Channel icon column: TEXT image_url → BLOB icon"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE channels DROP COLUMN image_url;
        ALTER TABLE channels ADD COLUMN icon BLOB;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            ALTER TABLE channels DROP COLUMN icon;
            ALTER TABLE channels ADD COLUMN image_url TEXT;
            "#,
        )
    }
}
