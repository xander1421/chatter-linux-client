//! Our own other devices at send time, kept apart from receipt targets so they
//! never count toward a message's delivered/read aggregate.
use super::Migration;
pub struct V076OwnDeviceSendTargets;
impl Migration for V076OwnDeviceSendTargets {
    fn version(&self) -> u32 {
        76
    }
    fn description(&self) -> &'static str {
        "Snapshot own other devices per send"
    }
    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE own_device_send_targets (
            conversation_id TEXT NOT NULL,
            conversation_type TEXT NOT NULL,
            author_id TEXT NOT NULL,
            sent_at INTEGER NOT NULL,
            device_id INTEGER NOT NULL CHECK (device_id > 0),
            PRIMARY KEY (conversation_id, conversation_type, author_id, sent_at, device_id)
        );
        CREATE INDEX own_device_send_targets_sent_at ON own_device_send_targets(sent_at);
        "#
    }
    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE own_device_send_targets;")
    }
}
