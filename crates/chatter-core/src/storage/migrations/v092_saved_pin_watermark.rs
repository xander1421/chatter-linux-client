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
        );
        INSERT INTO saved_pin_watermarks (message_id, changed_at)
        SELECT id, MAX(COALESCE(pinned_at, sent_at), sent_at, 1)
        FROM messages
        WHERE conversation_type = 'saved_messages' AND conversation_id = sender_id
          AND is_pinned = 1 AND is_deleted = 0;
        UPDATE messages SET pinned_at = (
            SELECT changed_at FROM saved_pin_watermarks WHERE message_id = messages.id
        ) WHERE id IN (SELECT message_id FROM saved_pin_watermarks);"
    }
}
