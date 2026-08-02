//! Migration v063: Record a peer's unaccepted identity-key change.
//!
//! A peer reinstall changes their Signal identity key. libsignal refuses the
//! new key (identity keys are pinned per account) and, critically, only calls
//! `save_identity` AFTER the trust check passes — so nothing in the protocol
//! can re-pin it. The change has to be parked somewhere durable until a human
//! accepts it.
//!
//! It also has to be durable for the SEND half of the wedge: with a stale
//! session still on disk, `ensure_session` short-circuits and never re-checks
//! the identity, so we would keep encrypting under a key the peer no longer
//! holds. This row is what `ensure_session` consults to refuse that.

use super::Migration;

pub struct V063PendingIdentityChanges;

impl Migration for V063PendingIdentityChanges {
    fn version(&self) -> u32 {
        63
    }

    fn description(&self) -> &'static str {
        "Add pending_identity_changes table for unaccepted peer identity keys"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE pending_identity_changes (
            peer_user_id TEXT PRIMARY KEY,
            new_identity_key BLOB NOT NULL,
            source TEXT NOT NULL,
            device_id INTEGER NOT NULL,
            first_seen_at TEXT NOT NULL
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS pending_identity_changes;")
    }
}
