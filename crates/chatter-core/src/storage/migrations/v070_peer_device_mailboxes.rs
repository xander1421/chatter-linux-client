//! V070: One peer mailbox per peer DEVICE.
//!
//! A Shield conversation with a multi-device peer is a set of per-device
//! halves: every peer device that answers the bootstrap owns a mailbox and a
//! conversation ephemeral of its own, so `dm_peer_mailboxes` is keyed by
//! `(conversation_id, peer_device_id)` and carries that device's ephemeral.
//! Our side stays one per peer (one mailbox, one ephemeral, shared by every
//! peer device), so `dm_conversation_keys` keeps only our half; each device's
//! secret is derived from ours and its ephemeral. `dm_device_pending` holds
//! messages already delivered to some peer device and still owed to one that
//! has not answered yet.
//!
//! Clean break: every Shield conversation re-bootstraps, as after v069.

use super::Migration;

pub struct V070PeerDeviceMailboxes;

impl Migration for V070PeerDeviceMailboxes {
    fn version(&self) -> u32 {
        70
    }

    fn description(&self) -> &'static str {
        "Peer mailboxes and conversation secrets per peer device"
    }

    fn up(&self) -> &'static str {
        r#"
        DROP INDEX IF EXISTS idx_dm_peer_mailboxes_conversation;
        DROP TABLE dm_peer_mailboxes;
        CREATE TABLE dm_peer_mailboxes (
            conversation_id TEXT NOT NULL,
            peer_device_id INTEGER NOT NULL,
            peer_mailbox_public_key BLOB NOT NULL,
            their_ephemeral_public BLOB NOT NULL,
            received_at TEXT NOT NULL,
            expires_at TEXT,
            PRIMARY KEY (conversation_id, peer_device_id)
        );

        DROP INDEX IF EXISTS idx_dm_conversation_keys_conversation;
        DROP TABLE dm_conversation_keys;
        CREATE TABLE dm_conversation_keys (
            conversation_id TEXT PRIMARY KEY,
            our_ephemeral_public BLOB NOT NULL,
            our_ephemeral_private BLOB NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE dm_device_pending (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_id TEXT NOT NULL,
            peer_device_id INTEGER NOT NULL,
            content BLOB NOT NULL,
            queued_at TEXT NOT NULL
        );
        CREATE INDEX idx_dm_device_pending_device ON dm_device_pending(conversation_id, peer_device_id);
        "#
    }
}
