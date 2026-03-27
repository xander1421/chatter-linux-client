//! Migration v041: Add peer verification state table.
//!
//! Stores whether a user has verified the safety number for each peer.
//! Used by the user profile modal to show verification badges.

use super::Migration;

pub struct V041PeerVerification;

impl Migration for V041PeerVerification {
    fn version(&self) -> u32 {
        41
    }

    fn description(&self) -> &'static str {
        "Add peer_verification table for safety number verification state"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE peer_verification (
            peer_user_id TEXT PRIMARY KEY,
            safety_number TEXT NOT NULL,
            is_verified INTEGER NOT NULL DEFAULT 0,
            verified_at TEXT
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS peer_verification;")
    }
}
