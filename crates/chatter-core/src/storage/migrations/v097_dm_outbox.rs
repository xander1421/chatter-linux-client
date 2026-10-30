//! Durable exact-envelope outbox for sealed friend DMs, and the sender's
//! disappearing clock started by confirmed submission rather than by insert.
use super::Migration;

pub struct V097DmOutbox;
impl Migration for V097DmOutbox {
    fn version(&self) -> u32 {
        97
    }
    fn description(&self) -> &'static str {
        "Sealed DM outbox and send-confirmed disappearing countdown"
    }
    fn up(&self) -> &'static str {
        // `content` is the encoded Content (needed to encrypt once, to log the
        // send for DecryptionErrorMessage resends and to self-sync). `request`
        // is the encoded SendSealedMessageRequest carrying the exact per-device
        // ciphertexts; once set, delivery only ever replays it, with `uak`, the
        // access key it was first sealed with. `devices` (JSON) narrows a
        // manual retry to the original send's devices.
        //
        // The trigger is Signal-Desktop's expirationStartTimestamp rule: an own
        // message's TTL, stamped when it was composed, starts counting only
        // when a send is confirmed, whichever path confirms it.
        "CREATE TABLE dm_outbox (
            owner_id TEXT NOT NULL,
            message_id TEXT NOT NULL,
            peer_id TEXT NOT NULL,
            device_id INTEGER NOT NULL,
            request_id TEXT NOT NULL,
            content BLOB NOT NULL,
            request BLOB,
            uak BLOB,
            devices TEXT,
            generation TEXT NOT NULL,
            attempts INTEGER NOT NULL DEFAULT 0,
            next_attempt_at INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
            PRIMARY KEY(owner_id, message_id)
        );
        CREATE INDEX dm_outbox_due ON dm_outbox(owner_id, next_attempt_at, created_at);
        CREATE TRIGGER ephemeral_countdown_on_sent AFTER UPDATE OF delivery_status ON messages
        WHEN NEW.delivery_status IN ('sent','delivered','read') AND NEW.is_read = 1
        BEGIN
            UPDATE ephemeral_messages SET countdown_started = 1,
                read_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                expires_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+' || ttl_seconds || ' seconds')
            WHERE message_id = NEW.id AND countdown_started = 0;
        END;"
    }
}
