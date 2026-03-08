//! Migration v034: Add profile fields to anon_group_members.
//!
//! Adds columns to store the member's profile key, display name, and avatar for anonymous groups.

use super::Migration;

pub struct V034AnonGroupMemberProfiles;

impl Migration for V034AnonGroupMemberProfiles {
    fn version(&self) -> u32 {
        34
    }

    fn description(&self) -> &'static str {
        "Add profile fields to anonymous group members"
    }

    fn up(&self) -> &'static str {
        r#"
        ALTER TABLE anon_group_members ADD COLUMN profile_key BLOB;
        ALTER TABLE anon_group_members ADD COLUMN display_name TEXT;
        ALTER TABLE anon_group_members ADD COLUMN avatar BLOB;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
