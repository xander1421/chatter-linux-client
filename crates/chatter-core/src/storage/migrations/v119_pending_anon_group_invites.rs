use super::Migration;

pub struct V119PendingAnonGroupInvites;

impl Migration for V119PendingAnonGroupInvites {
    fn version(&self) -> u32 {
        119
    }
    fn description(&self) -> &'static str {
        "Persist pending anonymous group invitations"
    }
    fn up(&self) -> &'static str {
        "CREATE TABLE pending_anon_group_invites (
            group_id TEXT PRIMARY KEY,
            group_name TEXT NOT NULL,
            inviter_id TEXT NOT NULL,
            inviter_device_id INTEGER NOT NULL,
            inviter_mailbox_pk BLOB NOT NULL,
            profile_key BLOB,
            inviter_display_name TEXT,
            inviter_avatar BLOB,
            received_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );"
    }
}
