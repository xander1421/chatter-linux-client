//! V081: every DM mailbox gets its own X25519 encryption key.
//!
//! Bootstrap envelopes used to be sealed to the mailbox's Ed25519 signing key
//! converted to X25519 — one key for both signing and DH. A mailbox now holds
//! a separate encryption key pair (`crypto::envelope_crypto`), stored here as
//! `mailbox_encryption_private_key`.
//!
//! Clean break: no existing mailbox has an encryption key, so the table is
//! rebuilt empty and the conversation halves built on those mailboxes are
//! dropped with it; every Shield conversation re-bootstraps, as after v070.

use super::Migration;

pub struct V081DmMailboxEncryptionKey;

impl Migration for V081DmMailboxEncryptionKey {
    fn version(&self) -> u32 {
        81
    }

    fn description(&self) -> &'static str {
        "Separate X25519 encryption key per DM mailbox"
    }

    fn up(&self) -> &'static str {
        r#"
        DROP INDEX IF EXISTS idx_dm_mailboxes_expires;
        DROP INDEX IF EXISTS idx_dm_mailboxes_conversation;
        DROP TABLE dm_mailboxes;
        CREATE TABLE dm_mailboxes (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            mailbox_public_key BLOB NOT NULL,
            mailbox_private_key BLOB NOT NULL,
            mailbox_encryption_private_key BLOB NOT NULL,
            signature BLOB NOT NULL,
            randomizer BLOB,
            token_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            last_polled_at TEXT,
            FOREIGN KEY (token_id) REFERENCES mailbox_tokens(id)
        );
        CREATE INDEX idx_dm_mailboxes_conversation ON dm_mailboxes(conversation_id);
        CREATE INDEX idx_dm_mailboxes_expires ON dm_mailboxes(expires_at);

        DELETE FROM dm_peer_mailboxes;
        DELETE FROM dm_conversation_keys;
        "#
    }

    fn down(&self) -> Option<&'static str> {
        None
    }
}
