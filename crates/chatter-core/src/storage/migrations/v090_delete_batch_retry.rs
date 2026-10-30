//! Preserve submitted deletion intent independently of live message rows.
use super::Migration;
pub struct V090DeleteBatchRetry;
impl Migration for V090DeleteBatchRetry {
    fn version(&self) -> u32 {
        90
    }
    fn description(&self) -> &'static str {
        "Durable prepared multi-message deletion retries"
    }
    fn up(&self) -> &'static str {
        "CREATE TABLE delete_batch_retry (
            batch_id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL, scope TEXT NOT NULL,
            selection_key TEXT NOT NULL, prepared_json TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            next_attempt INTEGER NOT NULL DEFAULT 0,
            paused INTEGER NOT NULL DEFAULT 0,
            UNIQUE(account_id, scope, selection_key)
        );"
    }
}
