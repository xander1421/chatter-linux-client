//! A secret-chat request of ours has three states, not two: never sent, sent
//! with an unknown outcome, and queued by the server (`RequestState`). The
//! old `1` ("may have left") is the unknown outcome, so values carry over.
use super::Migration;

pub struct V111SecretChatRequestState;
impl Migration for V111SecretChatRequestState {
    fn version(&self) -> u32 {
        111
    }
    fn description(&self) -> &'static str {
        "Secret-chat requests record whether the server queued them"
    }
    fn up(&self) -> &'static str {
        "ALTER TABLE anon_dm_conversations RENAME COLUMN request_submitted TO request_state;"
    }
}
