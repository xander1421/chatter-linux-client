//! Drop the anonymous call-room token tables (v036). Calls are disabled in
//! secret chats (owner decision 2026-10-10): LiveKit media would carry the
//! IP outside Tor, so nothing mints or spends call-room tokens any more.
use super::Migration;

pub struct V102DropCallRoomTokens;
impl Migration for V102DropCallRoomTokens {
    fn version(&self) -> u32 {
        102
    }
    fn description(&self) -> &'static str {
        "Drop anonymous call-room token tables"
    }
    fn up(&self) -> &'static str {
        "DROP TABLE IF EXISTS call_room_tokens;
         DROP TABLE IF EXISTS call_room_pending_blinds;
         DROP TABLE IF EXISTS call_room_quota;"
    }
}
