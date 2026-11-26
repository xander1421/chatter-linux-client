use super::Migration;

pub struct V147PrivateChannelAttachments;

impl Migration for V147PrivateChannelAttachments {
    fn version(&self) -> u32 {
        147
    }

    fn description(&self) -> &'static str {
        "Retain private-channel attachment routing"
    }

    fn up(&self) -> &'static str {
        "ALTER TABLE attachments ADD COLUMN private_channel TEXT;
         ALTER TABLE mailbox_tokens ADD COLUMN attachment BOOLEAN NOT NULL DEFAULT 0;"
    }
}
