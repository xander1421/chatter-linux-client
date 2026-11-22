use super::KeyStore;
use crate::error::StorageError;
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i64)]
pub(crate) enum KtRoute {
    Normal = 0,
    AnonymousTor = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PeerVerification {
    Legacy,
    LegacyUnlisted,
    UnverifiedInbound,
    Verified,
    Rejected,
}

#[derive(Debug)]
pub(crate) struct KtJob {
    pub peer: String,
    pub key: Vec<u8>,
    pub route: KtRoute,
    pub attempts: u32,
    pub archived: bool,
}

pub(crate) enum KtVerdict {
    Verified,
    NotInLog,
    Mismatch(Vec<u8>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KtUpdate {
    Ready,
    Refused,
    Changed,
    Stale,
}

impl KeyStore {
    pub(crate) fn peer_verification(
        &self,
        peer: &str,
        key: &[u8],
    ) -> Result<PeerVerification, StorageError> {
        let value: Option<String> = self
            .db
            .read_connection()?
            .query_row(
                "SELECT state FROM peer_key_transparency WHERE account=?1 AND identity_key=?2",
                params![peer, key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match value.as_deref() {
            Some("legacy") => PeerVerification::Legacy,
            Some("legacy-unlisted") => PeerVerification::LegacyUnlisted,
            Some("verified") => PeerVerification::Verified,
            Some("rejected") => PeerVerification::Rejected,
            _ => PeerVerification::UnverifiedInbound,
        })
    }

    pub(super) fn enqueue_kt_in(
        conn: &Connection,
        peer: &str,
        key: &[u8],
        route: KtRoute,
    ) -> Result<bool, StorageError> {
        Ok(conn.execute(
            "INSERT OR IGNORE INTO peer_kt_jobs (account,identity_key,route)
             SELECT ?1,?2,?3 WHERE NOT EXISTS (
                 SELECT 1 FROM peer_key_transparency WHERE account=?1 AND identity_key=?2
                 AND state IN ('verified','legacy-unlisted','rejected')
             ) AND NOT EXISTS (
                 SELECT 1 FROM pending_identity_changes WHERE peer_user_id=?1
             )",
            params![peer, key, route as i64],
        )? == 1)
    }

    pub(crate) fn enqueue_kt(
        &self,
        peer: &str,
        key: &[u8],
        route: KtRoute,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        if Self::enqueue_kt_in(&conn, peer, key, route)? {
            self.kt_work.notify_one();
        }
        Ok(())
    }

    pub(super) fn enqueue_inbound_kt_in(
        conn: &Connection,
        peer: &str,
        key: &[u8],
        route: KtRoute,
    ) -> Result<bool, StorageError> {
        let current: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM remote_identities WHERE address=?1 AND identity_key=?2)
             AND NOT EXISTS(SELECT 1 FROM settings WHERE key='auth.current_user_id' AND value=json_quote(?1))",
            params![peer, key], |row| row.get(0),
        )?;
        if current {
            Self::enqueue_kt_in(conn, peer, key, route)
        } else {
            Ok(false)
        }
    }

    pub(crate) fn enqueue_inbound_kt(
        &self,
        peer: &str,
        key: &[u8],
        route: KtRoute,
    ) -> Result<(), StorageError> {
        let conn = self.db.connection()?;
        if Self::enqueue_inbound_kt_in(&conn, peer, key, route)? {
            self.kt_work.notify_one();
        }
        Ok(())
    }

    pub(super) fn enqueue_archived_kt_in(
        conn: &Connection,
        peer: &str,
        pinned: &[u8],
        archived: &[u8],
        route: KtRoute,
    ) -> Result<bool, StorageError> {
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO peer_kt_archived_receives VALUES (?1,?2,?3,?4)",
            params![peer, pinned, archived, route as i64],
        )? == 1;
        if inserted {
            conn.execute(
                "INSERT INTO peer_kt_jobs (account,identity_key,route,archived)
                 VALUES (?1,?2,?3,1) ON CONFLICT(account,identity_key,route) DO UPDATE SET archived=1",
                params![peer, archived, route as i64],
            )?;
        }
        Ok(inserted)
    }

    pub(crate) fn enqueue_archived_kt(
        &self,
        peer: &str,
        pinned: &[u8],
        archived: &[u8],
        route: KtRoute,
    ) -> Result<(), StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let queued = Self::enqueue_archived_kt_in(&tx, peer, pinned, archived, route)?;
        tx.commit()?;
        if queued {
            self.kt_work.notify_one();
        }
        Ok(())
    }

    pub(crate) fn has_kt_job(&self, peer: &str, route: KtRoute) -> Result<bool, StorageError> {
        Ok(self.db.read_connection()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM peer_kt_jobs WHERE account=?1 AND route=?2)",
            params![peer, route as i64],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn kt_work(&self) -> &tokio::sync::Notify {
        &self.kt_work
    }

    /// A sealed inbox message can commit its identity before the plaintext
    /// classifies the route. After a crash, recover those keys over Tor.
    pub(crate) fn recover_kt_jobs(&self, owner: &str) -> Result<(), StorageError> {
        self.db.connection()?.execute(
            "INSERT OR IGNORE INTO peer_kt_jobs (account,identity_key,route)
             SELECT k.account,k.identity_key,?2 FROM peer_key_transparency k
               JOIN remote_identities r ON r.address=k.account AND r.identity_key=k.identity_key
             WHERE k.state='unverified-inbound' AND k.account!=?1
             AND NOT EXISTS (SELECT 1 FROM peer_kt_jobs j
                WHERE j.account=k.account AND j.identity_key=k.identity_key)
             AND NOT EXISTS (SELECT 1 FROM pending_identity_changes p WHERE p.peer_user_id=k.account)",
            params![owner, KtRoute::AnonymousTor as i64],
        )?;
        Ok(())
    }

    pub(crate) fn due_kt_jobs(&self, route: KtRoute, now: i64) -> Result<Vec<KtJob>, StorageError> {
        let conn = self.db.read_connection()?;
        let mut query = conn.prepare(
            "SELECT account,identity_key,attempts,archived FROM peer_kt_jobs
             WHERE route=?1 AND due_at<=?2 ORDER BY due_at,rowid LIMIT 8",
        )?;
        Ok(query
            .query_map(params![route as i64, now], |row| {
                Ok(KtJob {
                    peer: row.get(0)?,
                    key: row.get(1)?,
                    route,
                    attempts: row.get(2)?,
                    archived: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub(crate) fn delay_kt_job(&self, job: &KtJob, now: i64) -> Result<(), StorageError> {
        let attempts = job.attempts.saturating_add(1).min(8);
        let delay = (2_i64 << attempts).min(300);
        self.db.connection()?.execute(
            "UPDATE peer_kt_jobs SET attempts=?4,due_at=?5
             WHERE account=?1 AND identity_key=?2 AND route=?3",
            params![job.peer, job.key, job.route as i64, attempts, now + delay],
        )?;
        Ok(())
    }

    pub(crate) fn complete_kt_job(
        &self,
        owner: &str,
        job: &KtJob,
        verdict: KtVerdict,
    ) -> Result<KtUpdate, StorageError> {
        let mut conn = self.db.connection()?;
        let tx = conn.transaction()?;
        let valid: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM peer_kt_jobs WHERE account=?1 AND identity_key=?2 AND route=?3)
             AND EXISTS(SELECT 1 FROM settings WHERE key='auth.current_user_id' AND value=?4)",
            params![job.peer, job.key, job.route as i64, serde_json::to_string(owner)
                .map_err(|e| StorageError::InvalidData(e.to_string()))?],
            |row| row.get(0),
        )?;
        if !valid {
            return Ok(KtUpdate::Stale);
        }
        let stored: Option<Vec<u8>> = tx
            .query_row(
                "SELECT identity_key FROM remote_identities WHERE address=?1",
                [&job.peer],
                |row| row.get(0),
            )
            .optional()?;
        let already_changed: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pending_identity_changes WHERE peer_user_id=?1)",
            [&job.peer],
            |row| row.get(0),
        )?;
        let set_state = |key: &[u8], state: &str| -> Result<(), StorageError> {
            tx.execute(
                "INSERT INTO peer_key_transparency VALUES (?1,?2,?3)
                 ON CONFLICT(account,identity_key) DO UPDATE SET state=excluded.state",
                params![job.peer, key, state],
            )?;
            Ok(())
        };
        let verified_key = match &verdict {
            KtVerdict::Verified => Some(job.key.as_slice()),
            KtVerdict::Mismatch(actual) => Some(actual.as_slice()),
            KtVerdict::NotInLog => None,
        };
        let mut update = if already_changed {
            KtUpdate::Stale
        } else if let Some(verified) = verified_key {
            set_state(verified, "verified")?;
            if verified != job.key && !job.archived {
                set_state(&job.key, "rejected")?;
            }
            if stored.as_deref().is_some_and(|old| old != verified) {
                tx.execute(
                    "INSERT INTO pending_identity_changes
                     (peer_user_id,new_identity_key,source,device_id,first_seen_at)
                     VALUES (?1,?2,?3,0,?4)",
                    params![
                        job.peer,
                        verified,
                        crate::error::IdentityChangeSource::ServerClaimed.as_db_str(),
                        chrono::Utc::now().to_rfc3339()
                    ],
                )?;
                KtUpdate::Changed
            } else if verified == job.key {
                KtUpdate::Ready
            } else if job.archived {
                KtUpdate::Stale
            } else {
                KtUpdate::Refused
            }
        } else {
            let is_legacy = tx.execute(
                "UPDATE peer_key_transparency SET state='legacy-unlisted'
                 WHERE account=?1 AND identity_key=?2 AND state='legacy'",
                params![job.peer, job.key],
            )? == 1;
            if is_legacy {
                KtUpdate::Ready
            } else {
                // GetProof's NotFound is unsigned. A newly presented key must
                // never gain trust from it. Retry slowly in case the log catches up.
                tx.execute(
                    "UPDATE peer_kt_jobs SET attempts=8,due_at=?4
                     WHERE account=?1 AND identity_key=?2 AND route=?3",
                    params![
                        job.peer,
                        job.key,
                        job.route as i64,
                        chrono::Utc::now().timestamp() + 300
                    ],
                )?;
                tx.commit()?;
                return Ok(if job.attempts == 8 {
                    KtUpdate::Stale
                } else {
                    KtUpdate::Refused
                });
            }
        };
        if already_changed || update == KtUpdate::Changed {
            tx.execute("DELETE FROM peer_kt_jobs WHERE account=?1", [&job.peer])?;
        } else {
            tx.execute(
                "DELETE FROM peer_kt_jobs WHERE account=?1 AND identity_key=?2",
                params![job.peer, job.key],
            )?;
        }
        if !already_changed && update == KtUpdate::Refused && stored.as_deref() == verified_key {
            // The offered bundle was wrong. A job for the genuine stored identity
            // can now wake its own callers without re-querying.
            tx.execute(
                "DELETE FROM peer_kt_jobs WHERE account=?1 AND identity_key=?2",
                params![job.peer, verified_key],
            )?;
            update = KtUpdate::Ready;
        }
        tx.commit()?;
        Ok(update)
    }

    #[cfg(test)]
    pub(crate) fn mark_peer_verified(&self, peer: &str, key: &[u8]) -> Result<(), StorageError> {
        self.db.connection()?.execute(
            "INSERT INTO peer_key_transparency VALUES (?1,?2,'verified')
             ON CONFLICT(account,identity_key) DO UPDATE SET state='verified'",
            params![peer, key],
        )?;
        Ok(())
    }
}
