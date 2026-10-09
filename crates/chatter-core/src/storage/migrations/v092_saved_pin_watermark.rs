//! Durable pin clock for Saved records synced between our own devices.
use super::Migration;
pub struct V092SavedPinWatermark;
impl Migration for V092SavedPinWatermark {
    fn version(&self) -> u32 {
        92
    }
    fn description(&self) -> &'static str {
        "Saved pin/unpin watermark surviving unpin (SavedMessageSync ordering)"
    }
    fn up(&self) -> &'static str {
        // `messages.pinned_at` is cleared on unpin, so it cannot order a
        // late pin against a newer unpin. This row outlives both: the clock
        // of the last pin change applied to the record, from this device or
        // a sibling's snapshot. Never pruned; one short row per Saved record
        // whose pin state ever changed.
        "CREATE TABLE saved_pin_watermarks (
            message_id TEXT PRIMARY KEY,
            changed_at INTEGER NOT NULL
        );"
    }
}
