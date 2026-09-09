//! V053: Local call history table.
//!
//! Client-local record of observed call signaling events (start / accept /
//! decline / end / missed). Not server-synced — the server does not expose a
//! call-history RPC yet. Entries are written by `call_history_recorder`
//! (incoming side) and `ChatterClient::start_call` (outgoing side), and read
//! by the Calls screen via `ChatterClient::call_history_rows`.

use super::Migration;

/// V053: Create call_history table.
pub struct V053CallHistory;

impl Migration for V053CallHistory {
    fn version(&self) -> u32 {
        53
    }

    fn description(&self) -> &'static str {
        "Create call_history table"
    }

    fn up(&self) -> &'static str {
        r#"
        CREATE TABLE call_history (
            -- Stable local ID (typically the call_id from signaling, but any
            -- unique identifier works — deterministic so the same call is
            -- not re-inserted on reconnect).
            id TEXT PRIMARY KEY,
            -- The conversation the call happened in: peer UserId for a server
            -- DM, ChannelId for a channel call, AnonDmId / AnonGroupId for the
            -- anonymous flavours. Same key the sidebar routes on, so a row can
            -- reopen its conversation and redial.
            conversation_id TEXT NOT NULL,
            -- Which flavour `conversation_id` names — `ConversationType::as_str`.
            -- Only call-capable types are ever written; the CHECK lists the full
            -- vocabulary so a mislabelled row fails loudly at insert.
            conversation_type TEXT NOT NULL CHECK (conversation_type IN (
                'channel', 'server_dm', 'mailbox_dm', 'anon_dm', 'anon_group',
                'saved_messages', 'mesh', 'mesh_dm'
            )),
            -- Conversation/peer display name (snapshot at call time; may be
            -- empty for anonymous callers who never shared a name).
            peer_name TEXT NOT NULL DEFAULT '',
            -- When the call started ringing (our clock, RFC3339).
            started_at TEXT NOT NULL,
            -- When the call ended, or NULL while still in-progress.
            ended_at TEXT,
            -- 'incoming' or 'outgoing' relative to this device.
            direction TEXT NOT NULL CHECK (direction IN ('incoming', 'outgoing')),
            -- Outcome: 'completed' (normal hangup with duration),
            -- 'missed' (unanswered / no accept), 'declined' (explicit reject),
            -- 'failed' (setup error or network failure).
            result TEXT NOT NULL CHECK (result IN ('completed', 'missed', 'declined', 'failed')),
            -- Call length in seconds. NULL for calls that never connected.
            duration_secs INTEGER,
            -- Was this a video call (vs voice-only).
            has_video INTEGER NOT NULL DEFAULT 0
        );

        -- Primary query pattern: list recent calls, newest first.
        CREATE INDEX idx_call_history_started_at ON call_history(started_at DESC);

        -- Filter by conversation (e.g. "show calls with Alice").
        CREATE INDEX idx_call_history_peer
            ON call_history(conversation_id, started_at DESC);

        -- Missed-call filter (for the "Missed" tab / badge counts).
        CREATE INDEX idx_call_history_missed
            ON call_history(started_at DESC)
            WHERE result = 'missed';
        "#
    }

    fn down(&self) -> Option<&'static str> {
        Some(
            r#"
            DROP INDEX IF EXISTS idx_call_history_missed;
            DROP INDEX IF EXISTS idx_call_history_peer;
            DROP INDEX IF EXISTS idx_call_history_started_at;
            DROP TABLE IF EXISTS call_history;
            "#,
        )
    }
}
