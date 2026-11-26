//! Transparency heads shared by contacts, and the split views they exposed
//! (`services::signal::kt_anchor::gossip`).

use super::KeyStore;
use super::peer_verification::KtRoute;
use crate::error::StorageError;
use rusqlite::{OptionalExtension, params};

#[derive(Debug)]
pub(crate) struct PeerKtHead {
    pub peer: String,
    pub route: KtRoute,
    /// Encoded `content_v1::KtHeadGossip`.
    pub head: Vec<u8>,
    pub attempts: u32,
}

impl KeyStore {
    /// Keep the newest head a contact sent on `route`. A head already checked
    /// for that contact, or one from our own account, queues nothing.
    pub(crate) fn record_peer_kt_head(
        &self,
        peer: &str,
        route: KtRoute,
        head: &[u8],
    ) -> Result<(), StorageError> {
        let queued = self.db.connection()?.execute(
            "INSERT INTO peer_kt_heads (account,route,head)
             SELECT ?1,?2,?3 WHERE NOT EXISTS (
                 SELECT 1 FROM settings WHERE key='auth.current_user_id' AND value=json_quote(?1))
             ON CONFLICT(account,route) DO UPDATE SET head=excluded.head,attempts=0,due_at=0
             WHERE peer_kt_heads.head!=excluded.head
               AND peer_kt_heads.checked IS NOT excluded.head",
            params![peer, route as i64, head],
        )? == 1;
        if queued {
            self.kt_work.notify_one();
        }
        Ok(())
    }

    pub(crate) fn due_peer_kt_heads(
        &self,
        route: KtRoute,
        now: i64,
    ) -> Result<Vec<PeerKtHead>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut query = conn.prepare(
            "SELECT account,head,attempts FROM peer_kt_heads
             WHERE route=?1 AND due_at<=?2 AND checked IS NOT head
             ORDER BY due_at,rowid LIMIT 8",
        )?;
        Ok(query
            .query_map(params![route as i64, now], |row| {
                Ok(PeerKtHead {
                    peer: row.get(0)?,
                    route,
                    head: row.get(1)?,
                    attempts: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Back off like a key verification job. Returns the new attempt count.
    pub(crate) fn delay_peer_kt_head(&self, job: &PeerKtHead, now: i64) -> Result<u32, StorageError> {
        let attempts = job.attempts.saturating_add(1).min(8);
        let delay = (2_i64 << attempts).min(300);
        self.db.connection()?.execute(
            "UPDATE peer_kt_heads SET attempts=?3,due_at=?4 WHERE account=?1 AND route=?2",
            params![job.peer, job.route as i64, attempts, now + delay],
        )?;
        Ok(attempts)
    }

    /// Mark `job`'s head checked; a newer head from the peer stays queued.
    /// Returns true when this verdict newly records a split for the peer.
    pub(crate) fn settle_peer_kt_head(&self, job: &PeerKtHead, split: bool) -> Result<bool, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE peer_kt_heads SET checked=?3,attempts=0,due_at=0 WHERE account=?1 AND route=?2",
            params![job.peer, job.route as i64, job.head],
        )?;
        let recorded = split
            && tx.execute(
                "INSERT OR IGNORE INTO peer_kt_split_views VALUES (?1,?2)",
                params![job.peer, chrono::Utc::now().timestamp()],
            )? == 1;
        tx.commit()?;
        Ok(recorded)
    }

    pub(crate) fn peer_kt_split(&self, peer: &str) -> Result<bool, StorageError> {
        Ok(self
            .db
            .read_connection()?
            .query_row("SELECT 1 FROM peer_kt_split_views WHERE account=?1", [peer], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// The user's explicit decision to trust the conversation again.
    pub(crate) fn clear_peer_kt_split(&self, peer: &str) -> Result<bool, StorageError> {
        Ok(self
            .db
            .connection()?
            .execute("DELETE FROM peer_kt_split_views WHERE account=?1", [peer])?
            == 1)
    }
}
