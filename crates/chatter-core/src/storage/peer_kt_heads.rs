//! Transparency heads shared by contacts and our own devices, and the blocks
//! their checks recorded (`services::signal::kt_anchor::gossip`).

use super::KeyStore;
use super::peer_verification::KtRoute;
use crate::error::StorageError;
use rusqlite::{OptionalExtension, params};

/// Withheld checks on one route after which sends to the peer fail closed.
pub(crate) const WITHHELD_LIMIT: u32 = 3;
/// Consecutive consistent checks on the blocking route that clear an
/// unverified block, or any block on our own account.
pub(crate) const CLEAR_AFTER: u32 = 3;
/// An undecided streak this long, while identified KT requests succeed,
/// raises a warning. It never blocks sends: the streak is measured against
/// the account route, not the route that failed (docs/OPEN-ITEMS.md).
pub(crate) const UNDECIDED_LIMIT: u32 = 6;
pub(crate) const UNDECIDED_WINDOW_SECS: i64 = 24 * 60 * 60;

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
    /// The connection answered routine requests before and after the check
    /// but not the check.
    Withheld,
    /// The route did not answer a routine request. `identified_at` is when an
    /// identified KT request last verified, in Unix seconds.
    Undecided { identified_at: Option<i64> },
}

/// Why sends to a peer are blocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KtBlock {
    Split,
    Unverified,
}

/// What the message an actor command encrypts may do with a "send anyway
/// once" permit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermitUse {
    /// No message, such as session setup: allowed while a permit exists, and
    /// nothing is spent.
    Setup,
    /// Receipts, typing, sync, group sends: never covered.
    Other,
    /// A message the user wrote, by digest: spends the permit.
    Message([u8; 32]),
}

impl KeyStore {
    /// Keep the newest head an account sent on `route`. A pending row keeps its
    /// due time, so a busy conversation cannot postpone its check, and keeps
    /// its counts, so a new head cannot reset them.
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

    /// Retry later without a verdict: the route has no client right now.
    pub(crate) fn delay_peer_kt_head(&self, job: &PeerKtHead, now: i64) -> Result<(), StorageError> {
        let attempts = job.attempts.saturating_add(1).min(8);
        self.db.connection()?.execute(
            "UPDATE peer_kt_heads SET attempts=?3,due_at=?4 WHERE account=?1 AND route=?2",
            params![job.peer, job.route as i64, attempts, now + retry_delay(attempts)],
        )?;
        Ok(())
    }

    /// Record a verdict for `job`'s head. Returns true when the peer's block
    /// changed: a new split, a new unverified block, or a cleared one.
    pub(crate) fn settle_peer_kt_head(
        &self,
        job: &PeerKtHead,
        outcome: HeadSettle,
        now: i64,
    ) -> Result<bool, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let (peer, route) = (&job.peer, job.route as i64);
        let attempts = job.attempts.saturating_add(1).min(8);
        let retry_at = now + retry_delay(attempts);
        let mut withheld = false;
        let changed = match outcome {
            HeadSettle::Consistent => {
                tx.execute(
                    "UPDATE peer_kt_heads SET checked=?3,attempts=0,due_at=0,
                         consistent=consistent+1,undecided=0,undecided_since=NULL
                     WHERE account=?1 AND route=?2",
                    params![peer, route, job.head],
                )?;
                let consistent: u32 = tx
                    .query_row(
                        "SELECT consistent FROM peer_kt_heads WHERE account=?1 AND route=?2",
                        params![peer, route],
                        |row| row.get(0),
                    )
                    .optional()?
                    .unwrap_or(0);
                if consistent >= CLEAR_AFTER {
                    tx.execute(
                        "UPDATE peer_kt_heads SET withheld=0 WHERE account=?1 AND route=?2",
                        params![peer, route],
                    )?;
                    // A split on our own account has no accept control: we
                    // cannot vouch for ourselves against the server. It clears
                    // like an unverified block.
                    let cleared = tx.execute(
                        "DELETE FROM peer_kt_split_views
                         WHERE account=?1 AND route=?2 AND (kind IN ('unverified','suspect') OR EXISTS (
                             SELECT 1 FROM settings
                             WHERE key='auth.current_user_id' AND value=json_quote(?1)))",
                        params![peer, route],
                    )? == 1;
                    if cleared {
                        self.kt_permits.lock().remove(peer.as_str());
                    }
                    cleared
                } else {
                    false
                }
            }
            HeadSettle::Invalid => {
                tx.execute(
                    "UPDATE peer_kt_heads SET checked=?3,attempts=0,due_at=0,
                         undecided=0,undecided_since=NULL
                     WHERE account=?1 AND route=?2",
                    params![peer, route, job.head],
                )?;
                false
            }
            HeadSettle::Split => {
                tx.execute(
                    "UPDATE peer_kt_heads SET checked=?3,attempts=0,due_at=0,
                         consistent=0,undecided=0,undecided_since=NULL
                     WHERE account=?1 AND route=?2",
                    params![peer, route, job.head],
                )?;
                tx.execute(
                    "INSERT INTO peer_kt_split_views (account,kind,route,detected_at)
                     VALUES (?1,'split',?2,?3)
                     ON CONFLICT(account) DO UPDATE SET kind='split',route=?2,detected_at=?3
                     WHERE peer_kt_split_views.kind!='split'",
                    params![peer, route, now],
                )? == 1
            }
            HeadSettle::Withheld => {
                withheld = true;
                false
            }
            HeadSettle::Undecided { identified_at } => {
                tx.execute(
                    "UPDATE peer_kt_heads SET attempts=?3,due_at=?4,undecided=undecided+1,
                         undecided_since=COALESCE(undecided_since,?5)
                     WHERE account=?1 AND route=?2",
                    params![peer, route, attempts, retry_at, now],
                )?;
                let streak: Option<(u32, Option<i64>)> = tx
                    .query_row(
                        "SELECT undecided,undecided_since FROM peer_kt_heads
                         WHERE account=?1 AND route=?2",
                        params![peer, route],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                // Head checks that never get an answer while identified KT
                // requests keep verifying look like withholding. The account
                // route is not the route that failed, so this only warns.
                let suspect = streak.is_some_and(|(count, since)| {
                    since.is_some_and(|since| {
                        count >= UNDECIDED_LIMIT
                            && now - since >= UNDECIDED_WINDOW_SECS
                            && identified_at.is_some_and(|at| at > since)
                    })
                });
                if suspect {
                    tx.execute(
                        "UPDATE peer_kt_heads SET undecided=0,undecided_since=NULL
                         WHERE account=?1 AND route=?2",
                        params![peer, route],
                    )?;
                    tx.execute(
                        "INSERT OR IGNORE INTO peer_kt_split_views (account,kind,route,detected_at)
                         VALUES (?1,'suspect',?2,?3)",
                        params![peer, route, now],
                    )? == 1
                } else {
                    false
                }
            }
        };
        let changed = if withheld {
            tx.execute(
                "UPDATE peer_kt_heads SET withheld=withheld+1,consistent=0,attempts=?3,due_at=?4,
                     undecided=0,undecided_since=NULL
                 WHERE account=?1 AND route=?2",
                params![peer, route, attempts, retry_at],
            )?;
            let count: u32 = tx
                .query_row(
                    "SELECT withheld FROM peer_kt_heads WHERE account=?1 AND route=?2",
                    params![peer, route],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);
            count >= WITHHELD_LIMIT
                && tx.execute(
                    "INSERT INTO peer_kt_split_views (account,kind,route,detected_at)
                     VALUES (?1,'unverified',?2,?3)
                     ON CONFLICT(account) DO UPDATE SET kind='unverified',route=?2,detected_at=?3
                     WHERE peer_kt_split_views.kind='suspect'",
                    params![peer, route, now],
                )? == 1
        } else {
            changed
        };
        tx.commit()?;
        Ok(changed)
    }

    /// The block recorded for `peer`, if any.
    pub(crate) fn peer_kt_block(&self, peer: &str) -> Result<Option<KtBlock>, StorageError> {
        let kind: Option<String> = self
            .db
            .read_connection()?
            .query_row(
                "SELECT kind FROM peer_kt_split_views WHERE account=?1",
                [peer],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match kind.as_deref() {
            Some("split") => Some(KtBlock::Split),
            Some("unverified") => Some(KtBlock::Unverified),
            _ => None,
        })
    }

    /// Whether any account's head checks raised the warn-only streak.
    pub(crate) fn kt_suspect_exists(&self) -> Result<bool, StorageError> {
        Ok(self
            .db
            .read_connection()?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM peer_kt_split_views WHERE kind='suspect')",
                [],
                |row| row.get(0),
            )?)
    }

    /// The user's explicit decision on the block. A split is cleared. An
    /// unverified block stays; the user gets one in-memory permit for the
    /// next message.
    pub(crate) fn accept_peer_kt_block(&self, peer: &str) -> Result<bool, StorageError> {
        let cleared = self.db.connection()?.execute(
            "DELETE FROM peer_kt_split_views WHERE account=?1 AND kind='split'",
            [peer],
        )? == 1;
        if cleared {
            return Ok(true);
        }
        if self.peer_kt_block(peer)? == Some(KtBlock::Unverified) {
            self.kt_permits.lock().insert(peer.to_owned(), None);
            return Ok(true);
        }
        Ok(false)
    }

    /// Whether the "send anyway once" permit covers this send. Only a message
    /// the user wrote spends it, and then it covers only that message's other
    /// devices and retries.
    pub(crate) fn use_kt_permit(&self, peer: &str, send: PermitUse) -> bool {
        let mut permits = self.kt_permits.lock();
        let Some(permit) = permits.get_mut(peer) else {
            return false;
        };
        match (send, *permit) {
            (PermitUse::Setup, _) => true,
            (PermitUse::Other, _) => false,
            (PermitUse::Message(digest), None) => {
                *permit = Some(digest);
                true
            }
            (PermitUse::Message(digest), Some(spent)) => digest == spent,
        }
    }
}

/// Exponential backoff from 5 minutes to a 6-hour cap.
fn retry_delay(attempts: u32) -> i64 {
    (300_i64 << attempts.saturating_sub(1).min(7)).min(6 * 60 * 60)
}
