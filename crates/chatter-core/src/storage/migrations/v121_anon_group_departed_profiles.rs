use super::Migration;

pub struct V121AnonGroupDepartedProfiles;

impl Migration for V121AnonGroupDepartedProfiles {
    fn version(&self) -> u32 {
        121
    }

    fn description(&self) -> &'static str {
        "Retain departed anonymous group profiles"
    }

    fn up(&self) -> &'static str {
        "ALTER TABLE anon_group_members ADD COLUMN is_active INTEGER NOT NULL DEFAULT 1;"
    }
}
