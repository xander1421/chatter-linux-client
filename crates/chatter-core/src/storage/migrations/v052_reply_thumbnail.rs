//! Persist reply-quote thumbnail bytes + MIME type.
//!
//! Adds columns so a reply's quoted-image preview survives channel reload
//! without needing to look up the original attachment row.

use super::Migration;

pub struct V052ReplyThumbnail;

impl Migration for V052ReplyThumbnail {
    fn version(&self) -> u32 {
        52
    }

    fn description(&self) -> &'static str {
        "Add reply_to_thumbnail and reply_to_thumbnail_mime columns to messages"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE messages ADD COLUMN reply_to_thumbnail BLOB;
        ALTER TABLE messages ADD COLUMN reply_to_thumbnail_mime TEXT;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
