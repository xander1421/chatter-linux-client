//! Settle `attachments.secret_chat` for rows stored before the marker existed
//! (v101 left them NULL). An unknown origin was always routed over Tor, so
//! those rows become secret; from here every row holds 0 or 1.
use super::Migration;

pub struct V110AttachmentSecretChatKnown;
impl Migration for V110AttachmentSecretChatKnown {
    fn version(&self) -> u32 {
        110
    }
    fn description(&self) -> &'static str {
        "Unknown attachment origins become secret"
    }
    fn up(&self) -> &'static str {
        "UPDATE attachments SET secret_chat = 1 WHERE secret_chat IS NULL;"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_origin_becomes_secret_and_known_ones_stay() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE attachments (id TEXT PRIMARY KEY, secret_chat INTEGER);
             INSERT INTO attachments VALUES ('unknown', NULL), ('plain', 0), ('secret', 1);",
        )
        .unwrap();
        conn.execute_batch(V110AttachmentSecretChatKnown.up())
            .unwrap();
        let origin = |id: &str| -> bool {
            conn.query_row(
                "SELECT secret_chat FROM attachments WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(origin("unknown"), "an unknown origin must never route clearnet");
        assert!(!origin("plain"));
        assert!(origin("secret"));
    }
}
