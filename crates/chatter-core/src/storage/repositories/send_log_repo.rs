//! Message send log: the exact plaintext `Content` bytes of every resendable
//! 1:1 deposit, so a device that could not decrypt one can have those bytes
//! again (migration v078).
//!
//! Signal-Desktop's MSL (`ts/sql/Server.node.ts` `sendLogPayloads` /
//! `sendLogRecipients`, read by `ts/util/handleRetry.preload.ts`
//! `onRetryRequest`): one payload row per send, one recipient row per device
//! it reached. The recipient key here is the deposit's SERVER timestamp — the
//! value a `DecryptionErrorMessage` from that device names, because the
//! receiver never saw the content's own timestamp (`dm_resend.rs`).

use std::sync::Arc;

use rusqlite::{OptionalExtension, params};

use crate::domain::UserId;
use crate::error::StorageError;
use crate::storage::Database;

/// Payloads kept at most. Past this the oldest go first; a retry request for
/// one is answered with a `NullMessage`, which still heals the session.
pub const MAX_PAYLOADS: i64 = 5_000;

/// One logged send, as handed back for a resend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentPayload {
    /// The serialized `content_v1::Content`, byte for byte as encrypted.
    pub content: Vec<u8>,
    /// `sent_at` of our own stored message this payload carries (a message,
    /// its `SyncSent` copy) or edits; `None` when it carries none.
    pub message_ts: Option<u64>,
}

pub struct SendLogRepository {
    db: Arc<Database>,
}

impl SendLogRepository {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Log `content` as deposited for `peer` on each of `deposits`
    /// (`(device, server timestamp in epoch nanos)`), resendable until
    /// `expires_at` (epoch nanos). Drops what has expired and what is over
    /// [`MAX_PAYLOADS`] in the same transaction.
    pub fn record(
        &self,
        peer: &UserId,
        content: &[u8],
        message_ts: Option<u64>,
        now: u64,
        expires_at: u64,
        deposits: &[(u32, u64)],
    ) -> Result<(), StorageError> {
        if deposits.is_empty() {
            return Ok(());
        }
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO send_log_payloads (content, message_ts, expires_at) VALUES (?1, ?2, ?3)",
            params![content, message_ts.map(|t| t as i64), expires_at as i64],
        )?;
        let payload_id = tx.last_insert_rowid();
        for &(device, server_ts_ns) in deposits {
            tx.execute(
                "INSERT OR REPLACE INTO send_log_recipients (payload_id, peer_id, device_id, server_ts_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![payload_id, peer.to_string(), device as i64, (server_ts_ns / 1_000_000) as i64],
            )?;
        }
        Self::prune(&tx, now)?;
        tx.commit()?;
        Ok(())
    }

    /// The payload deposited for `(peer, device)` at `server_ts_ms`, if it
    /// has not expired — consumed: one resend per deposit, so a peer cannot
    /// make us send the same bytes without end. The payload itself goes once
    /// no device it reached can still ask for it.
    pub fn take(
        &self,
        peer: &UserId,
        device: u32,
        server_ts_ms: u64,
        now: u64,
    ) -> Result<Option<SentPayload>, StorageError> {
        let conn = self.db.connection()?;
        let tx = conn.unchecked_transaction()?;
        let key = params![peer.to_string(), device as i64, server_ts_ms as i64];
        let found = tx
            .query_row(
                "SELECT p.id, p.content, p.message_ts, p.expires_at
                 FROM send_log_recipients r JOIN send_log_payloads p ON p.id = r.payload_id
                 WHERE r.peer_id = ?1 AND r.device_id = ?2 AND r.server_ts_ms = ?3",
                key,
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((payload_id, content, message_ts, expires_at)) = found else {
            return Ok(None);
        };
        tx.execute(
            "DELETE FROM send_log_recipients WHERE peer_id = ?1 AND device_id = ?2 AND server_ts_ms = ?3",
            key,
        )?;
        tx.execute(
            "DELETE FROM send_log_payloads WHERE id = ?1
               AND NOT EXISTS (SELECT 1 FROM send_log_recipients WHERE payload_id = ?1)",
            params![payload_id],
        )?;
        Self::prune(&tx, now)?;
        tx.commit()?;
        if (expires_at as u64) <= now {
            return Ok(None);
        }
        Ok(Some(SentPayload {
            content,
            message_ts: message_ts.map(|t| t as u64),
        }))
    }

    /// Expired payloads, then everything past the newest [`MAX_PAYLOADS`].
    /// Recipient rows go with their payload (`ON DELETE CASCADE`).
    fn prune(conn: &rusqlite::Connection, now: u64) -> Result<(), StorageError> {
        conn.execute(
            "DELETE FROM send_log_payloads WHERE expires_at <= ?1",
            params![now as i64],
        )?;
        conn.execute(
            "DELETE FROM send_log_payloads WHERE id <=
               (SELECT id FROM send_log_payloads ORDER BY id DESC LIMIT 1 OFFSET ?1)",
            params![MAX_PAYLOADS],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> (tempfile::TempDir, SendLogRepository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("s.db"), "testpass").expect("open"));
        db.run_migrations().expect("migrations");
        (dir, SendLogRepository::new(db))
    }

    #[test]
    fn a_deposit_is_taken_once_by_the_device_it_went_to() {
        let (_dir, log) = repo();
        let (peer, other) = (UserId::new(), UserId::new());
        let now = 1_000_000_000_000;
        log.record(
            &peer,
            b"bytes",
            Some(7),
            now,
            now + 10,
            &[(1, 5_000_000), (2, 6_000_000)],
        )
        .unwrap();

        assert_eq!(
            log.take(&other, 1, 5, now).unwrap(),
            None,
            "another peer naming our deposit"
        );
        assert_eq!(
            log.take(&peer, 2, 5, now).unwrap(),
            None,
            "device 2's deposit has its own timestamp"
        );
        let got = log
            .take(&peer, 1, 5, now)
            .unwrap()
            .expect("device 1's deposit");
        assert_eq!(
            got,
            SentPayload {
                content: b"bytes".to_vec(),
                message_ts: Some(7)
            }
        );
        assert_eq!(
            log.take(&peer, 1, 5, now).unwrap(),
            None,
            "one resend per deposit"
        );
        assert!(
            log.take(&peer, 2, 6, now).unwrap().is_some(),
            "the payload outlives its first taker"
        );
    }

    #[test]
    fn expired_and_overflowing_payloads_are_dropped() {
        let (_dir, log) = repo();
        let peer = UserId::new();
        let now = 1_000_000_000_000;
        log.record(&peer, b"old", None, now, now + 5, &[(1, 1_000_000)])
            .unwrap();
        assert_eq!(
            log.take(&peer, 1, 1, now + 5).unwrap(),
            None,
            "past its expiry"
        );

        for i in 0..=MAX_PAYLOADS as u64 {
            log.record(
                &peer,
                b"m",
                None,
                now,
                now + 100,
                &[(1, (i + 10) * 1_000_000)],
            )
            .unwrap();
        }
        assert_eq!(
            log.take(&peer, 1, 10, now).unwrap(),
            None,
            "the oldest went over the cap"
        );
        assert!(log.take(&peer, 1, 11, now).unwrap().is_some());
    }
}
