//! Transparency heads shared by contacts and our own devices, and the blocks
//! their checks recorded (`services::signal::kt_anchor::gossip`).

use super::KeyStore;
use super::peer_verification::KtRoute;
use crate::error::StorageError;
use rusqlite::{OptionalExtension, params};

/// Withheld checks after which sends to the peer fail closed.
pub(crate) const WITHHELD_LIMIT: u32 = 3;

#[derive(Debug)]
pub(crate) struct PeerKtHead {
    pub peer: String,
    pub route: KtRoute,
    /// Encoded `content_v1::KtHeadGossip`.
    pub head: Vec<u8>,
    pub attempts: u32,
}

/// What a check of one shared head established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeadSettle {
    /// Our pin and the head are prefixes of one signed head.
    Consistent,
    /// Two operator signatures that cannot belong to one log.
    Split,
    /// The head carries no valid operator signature.
    Invalid,
    /// The route answered a routine request but not the check.
    Withheld,
}

/// Why sends to a peer are blocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KtBlock {
    Split,
    Unverified,
}

impl KeyStore {
    /// Keep the newest head an account sent on `route`. A pending row keeps its
    /// due time, so a busy conversation cannot postpone its check, and keeps
    /// its withheld count, so a new head cannot reset the fail-closed count.
    pub(crate) fn record_peer_kt_head(
        &self,
        peer: &str,
        route: KtRoute,
        head: &[u8],
        due_at: i64,
    ) -> Result<(), StorageError> {
        self.db.connection()?.execute(
            "INSERT INTO peer_kt_heads (account,route,head,due_at) VALUES (?1,?2,?3,?4)
             ON CONFLICT(account,route) DO UPDATE SET
                 due_at=CASE WHEN peer_kt_heads.checked IS NOT peer_kt_heads.head
                             THEN peer_kt_heads.due_at ELSE excluded.due_at END,
                 head=excluded.head
             WHERE peer_kt_heads.head!=excluded.head
               AND peer_kt_heads.checked IS NOT excluded.head",
            params![peer, route as i64, head, due_at],
        )?;
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

    /// Retry an undecided check later. It counts toward nothing.
    pub(crate) fn delay_peer_kt_head(&self, job: &PeerKtHead, now: i64) -> Result<(), StorageError> {
        let attempts = job.attempts.saturating_add(1).min(8);
        self.db.connection()?.execute(
            "UPDATE peer_kt_heads SET attempts=?3,due_at=?4 WHERE account=?1 AND route=?2",
            params![job.peer, job.route as i64, attempts, now + retry_delay(attempts)],
        )?;
        Ok(())
    }

    /// Record a verdict for `job`'s head. Returns true when the peer's block
    /// changed: a new split, a new or renewed unverified block, or an
    /// unverified block cleared by a consistent check.
    pub(crate) fn settle_peer_kt_head(
        &self,
        job: &PeerKtHead,
        outcome: HeadSettle,
        now: i64,
    ) -> Result<bool, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let (peer, route) = (&job.peer, job.route as i64);
        let changed = match outcome {
            HeadSettle::Consistent => {
                tx.execute(
                    "UPDATE peer_kt_heads SET checked=?3,attempts=0,withheld=0,due_at=0
                     WHERE account=?1 AND route=?2",
                    params![peer, route, job.head],
                )?;
                tx.execute(
                    "DELETE FROM peer_kt_split_views WHERE account=?1 AND kind='unverified'",
                    [peer],
                )? == 1
            }
            HeadSettle::Invalid => {
                tx.execute(
                    "UPDATE peer_kt_heads SET checked=?3,attempts=0,due_at=0
                     WHERE account=?1 AND route=?2",
                    params![peer, route, job.head],
                )?;
                false
            }
            HeadSettle::Split => {
                tx.execute(
                    "UPDATE peer_kt_heads SET checked=?3,attempts=0,due_at=0
                     WHERE account=?1 AND route=?2",
                    params![peer, route, job.head],
                )?;
                tx.execute(
                    "INSERT INTO peer_kt_split_views (account,kind,allowed,detected_at)
                     VALUES (?1,'split',0,?2)
                     ON CONFLICT(account) DO UPDATE SET kind='split',allowed=0,detected_at=?2
                     WHERE peer_kt_split_views.kind!='split'",
                    params![peer, now],
                )? == 1
            }
            HeadSettle::Withheld => {
                let attempts = job.attempts.saturating_add(1).min(8);
                tx.execute(
                    "UPDATE peer_kt_heads SET withheld=withheld+1,attempts=?3,due_at=?4
                     WHERE account=?1 AND route=?2",
                    params![peer, route, attempts, now + retry_delay(attempts)],
                )?;
                let withheld: u32 = tx
                    .query_row(
                        "SELECT withheld FROM peer_kt_heads WHERE account=?1 AND route=?2",
                        params![peer, route],
                        |row| row.get(0),
                    )
                    .optional()?
                    .unwrap_or(0);
                // A permission to send anyway lasts until the next withheld check.
                withheld >= WITHHELD_LIMIT
                    && tx.execute(
                        "INSERT INTO peer_kt_split_views (account,kind,allowed,detected_at)
                         VALUES (?1,'unverified',0,?2)
                         ON CONFLICT(account) DO UPDATE SET allowed=0
                         WHERE peer_kt_split_views.kind='unverified'
                           AND peer_kt_split_views.allowed=1",
                        params![peer, now],
                    )? == 1
            }
        };
        tx.commit()?;
        Ok(changed)
    }

    /// The block in force for `peer`. An unverified block the user chose to
    /// send through is not in force.
    pub(crate) fn peer_kt_block(&self, peer: &str) -> Result<Option<KtBlock>, StorageError> {
        let kind: Option<String> = self
            .db
            .read_connection()?
            .query_row(
                "SELECT kind FROM peer_kt_split_views WHERE account=?1 AND allowed=0",
                [peer],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match kind.as_deref() {
            Some("split") => Some(KtBlock::Split),
            Some(_) => Some(KtBlock::Unverified),
            None => None,
        })
    }

    /// The user's explicit decision on the block in force. A split is cleared;
    /// an unverified block lets sends through until the next withheld check.
    pub(crate) fn accept_peer_kt_block(&self, peer: &str) -> Result<bool, StorageError> {
        let conn = self.db.connection()?;
        let cleared = conn.execute(
            "DELETE FROM peer_kt_split_views WHERE account=?1 AND kind='split'",
            [peer],
        )?;
        let allowed = conn.execute(
            "UPDATE peer_kt_split_views SET allowed=1
             WHERE account=?1 AND kind='unverified' AND allowed=0",
            [peer],
        )?;
        Ok(cleared + allowed > 0)
    }
}

fn retry_delay(attempts: u32) -> i64 {
    (2_i64 << attempts).min(300)
}
