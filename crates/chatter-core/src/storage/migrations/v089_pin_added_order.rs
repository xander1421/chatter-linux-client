//! Preserve historical pins without inventing a pin-added time.
use super::Migration;
pub struct V089PinAddedOrder;
impl Migration for V089PinAddedOrder {
    fn version(&self) -> u32 {
        89
    }
    fn description(&self) -> &'static str {
        "Track pin-added time without changing message data"
    }
    fn up(&self) -> &'static str {
        "ALTER TABLE messages ADD COLUMN pinned_at INTEGER;"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_pin_time_stays_unknown_and_data_is_preserved() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE messages(id TEXT, is_pinned INTEGER, content_json TEXT); INSERT INTO messages VALUES('old',1,'keep body'); CREATE TABLE drafts(body TEXT); INSERT INTO drafts VALUES('keep draft');").unwrap();
        conn.execute_batch(V089PinAddedOrder.up()).unwrap();
        let row: (String, Option<i64>) = conn
            .query_row(
                "SELECT content_json,pinned_at FROM messages WHERE id='old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("keep body".into(), None));
        assert_eq!(
            conn.query_row("SELECT body FROM drafts", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "keep draft"
        );
    }
}
