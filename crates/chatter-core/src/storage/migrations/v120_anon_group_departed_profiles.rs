use super::Migration;

pub struct V120AnonGroupDepartedProfiles;

impl Migration for V120AnonGroupDepartedProfiles {
    fn version(&self) -> u32 {
        120
    }

    fn description(&self) -> &'static str {
        "Retain departed anonymous group profiles"
    }

    fn up(&self) -> &'static str {
        "ALTER TABLE anon_group_members ADD COLUMN is_active INTEGER NOT NULL DEFAULT 1;"
    }
}
