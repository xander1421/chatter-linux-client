//! Secret chats are bound to ONE peer device. `peer_device_id` is that device
//! (NULL until known: our own request not yet sealed, or a Shield ECIES request
//! not yet accepted); `request_submitted` tells a request of ours that may
//! have left this device from one that provably never did. Anonymous-group
//! members get the same binding: `device_id` is the device that owns their
//! group mailbox.
use super::Migration;

pub struct V103SecretChatPeerDevice;
impl Migration for V103SecretChatPeerDevice {
    fn version(&self) -> u32 {
        103
    }
    fn description(&self) -> &'static str {
        "Bind secret chats (anon DMs, anon-group members) to one peer device"
    }
    fn up(&self) -> &'static str {
        "ALTER TABLE anon_dm_conversations ADD COLUMN peer_device_id INTEGER;
         ALTER TABLE anon_dm_conversations ADD COLUMN request_submitted INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE anon_group_members ADD COLUMN device_id INTEGER;"
    }
}
