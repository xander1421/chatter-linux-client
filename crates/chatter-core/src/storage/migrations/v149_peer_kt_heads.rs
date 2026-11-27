use super::Migration;

pub struct V149PeerKtHeads;

impl Migration for V149PeerKtHeads {
    fn version(&self) -> u32 {
        149
    }

    fn description(&self) -> &'static str {
        "Track transparency heads shared by contacts and detected split views"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE peer_kt_heads (
            account TEXT NOT NULL,
            route INTEGER NOT NULL CHECK (route IN (0, 1)),
            head BLOB NOT NULL,
            checked BLOB,
            attempts INTEGER NOT NULL DEFAULT 0,
            withheld INTEGER NOT NULL DEFAULT 0,
            consistent INTEGER NOT NULL DEFAULT 0,
            undecided INTEGER NOT NULL DEFAULT 0,
            undecided_since INTEGER,
            due_at INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (account, route)
        );
        CREATE TABLE peer_kt_split_views (
            account TEXT PRIMARY KEY,
            kind TEXT NOT NULL CHECK (kind IN ('split', 'unverified')),
            route INTEGER NOT NULL CHECK (route IN (0, 1)),
            detected_at INTEGER NOT NULL
        );
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some("DROP TABLE IF EXISTS peer_kt_split_views; DROP TABLE IF EXISTS peer_kt_heads;")
    }
}
