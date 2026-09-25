//! V069: A peer mailbox records the device that owns it.
//!
//! The mailbox conversation is one-peer-device: the peer device that answered
//! the bootstrap made the mailbox and is the only one that polls it. Without
//! that device on record the live send sealed a copy for EVERY peer device into
//! it, and the owner pulled its siblings' ciphertexts and reported each as a
//! failed message. Clean break: rows from before this have no owner and are
//! dropped, which is the same state as an expired peer mailbox — the next send
//! re-bootstraps.

use super::Migration;

pub struct V069PeerMailboxDevice;

impl Migration for V069PeerMailboxDevice {
    fn version(&self) -> u32 {
        69
    }

    fn description(&self) -> &'static str {
        "Peer mailboxes record their owning device"
    }

    fn up(&self) -> &'static str {
        r#"
        DROP INDEX IF EXISTS idx_dm_peer_mailboxes_conversation;
        DROP TABLE dm_peer_mailboxes;
        CREATE TABLE dm_peer_mailboxes (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            peer_mailbox_public_key BLOB NOT NULL,
            peer_device_id INTEGER NOT NULL,
            received_at TEXT NOT NULL,
            expires_at TEXT,
            UNIQUE(conversation_id, peer_mailbox_public_key)
        );
        CREATE INDEX idx_dm_peer_mailboxes_conversation ON dm_peer_mailboxes(conversation_id);
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("ALTER TABLE dm_peer_mailboxes DROP COLUMN peer_device_id;")
    }
}
