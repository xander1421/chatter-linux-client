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
        "ALTER TABLE attachments ADD COLUMN private_channel INTEGER NOT NULL DEFAULT 0;
         UPDATE attachments SET private_channel = 1
         WHERE id IN (
             SELECT sc.attachment_id FROM shared_content sc
             LEFT JOIN channel_type_pins p ON p.channel_id = sc.conversation_id
             WHERE sc.attachment_id != '' AND sc.conversation_type = 'channel'
               AND (p.channel_type IS NULL OR p.channel_type = 1)
         );"
    }
}
