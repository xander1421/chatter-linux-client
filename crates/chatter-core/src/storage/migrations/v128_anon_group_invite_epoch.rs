use super::Migration;

pub struct V128AnonGroupInviteEpoch;

impl Migration for V128AnonGroupInviteEpoch {
    fn version(&self) -> u32 { 128 }
    fn description(&self) -> &'static str { "Bind pending anonymous invitations to admission epochs" }
    fn up(&self) -> &'static str {
        "DROP TABLE pending_anon_group_invites;
         CREATE TABLE pending_anon_group_invites (
            group_id TEXT PRIMARY KEY,
            group_name TEXT NOT NULL,
            inviter_id TEXT NOT NULL,
            inviter_device_id INTEGER NOT NULL,
            inviter_mailbox_pk BLOB NOT NULL,
            profile_key BLOB,
            inviter_display_name TEXT,
            inviter_avatar BLOB,
            invite_epoch INTEGER NOT NULL CHECK(invite_epoch > 0),
            received_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
         );"
    }
}
