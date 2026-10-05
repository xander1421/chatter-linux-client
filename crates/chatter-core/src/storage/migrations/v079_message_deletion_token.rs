//! Sealed-sender deletion token for our own sent messages.
//!
//! A sealed-sender channel message is stored on the server with no author; it
//! carries SHA-256 of a random 32-byte token the sending device generated. That
//! token is the only proof that can delete the server copy, so it lives with the
//! sent message row. NULL for every message we did not send as sealed sender.
use super::Migration;
pub struct V079MessageDeletionToken;
impl Migration for V079MessageDeletionToken {
    fn version(&self) -> u32 {
        79
    }
    fn description(&self) -> &'static str {
        "Store sealed-sender deletion token on sent messages"
    }
    fn up(&self) -> &'static str {
        "ALTER TABLE messages ADD COLUMN deletion_token BLOB;"
    }
    fn down(&self) -> Option<&'static str> {
        None
    }
}
