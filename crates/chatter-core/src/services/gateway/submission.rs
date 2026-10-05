//! Anonymous deposits cannot be retried by re-encrypting a parked plaintext.
//! Retain the first plaintext/ciphertext in SQLCipher before attempting deposit.
use crate::{error::StorageError, storage::database::Database};
use rusqlite::params;
use sha2::{Digest, Sha256};

pub(crate) enum Admission {
    First(String),
    Confirmed,
    Unconfirmed,
}

pub(crate) fn user_post(content: &crate::proto::content_v1::Content) -> bool {
    use crate::proto::content_v1::content::Content;
    let inner = match content.content.as_ref() {
        Some(Content::AnonDmMessage(message)) => message.inner_content.as_deref(),
        Some(Content::AnonGroupMessage(message)) => message.inner_content.as_deref(),
        _ => None,
    };
    matches!(
        inner.and_then(|c| c.content.as_ref()),
        Some(Content::DataMessage(_))
    )
}

fn scrub_legacy_content(db: &Database) -> Result<(), StorageError> {
    let mut conn = db.connection()?;
    let done: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM settings WHERE key='send.anonymous.content-scrub.v1')",
        [],
        |row| row.get(0),
    )?;
    if done {
        return Ok(());
    }
    let tx = conn.transaction()?;
    tx.execute("UPDATE settings SET value=CASE WHEN json_extract(value,'$.confirmed')=1 THEN json_object('confirmed',json('true')) ELSE json_remove(value,'$.plaintext') END WHERE key LIKE 'send.anonymous.v1:%'", [])?;
    tx.execute("INSERT OR IGNORE INTO settings(key,value,updated_at) VALUES ('send.anonymous.content-scrub.v1','done',?1)", params![chrono::Utc::now().to_rfc3339()])?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn prepare(
    db: &Database,
    owner: &str,
    peer: &str,
    plaintext: &[u8],
) -> Result<Admission, StorageError> {
    scrub_legacy_content(db)?;
    let mut hash = Sha256::new();
    hash.update(owner.as_bytes());
    hash.update([0]);
    hash.update(peer.as_bytes());
    hash.update([0]);
    hash.update(plaintext);
    let key = format!("send.anonymous.v1:{}", hex::encode(hash.finalize()));
    let value = serde_json::json!({"ciphertext":null,"confirmed":false}).to_string();
    let conn = db.connection()?;
    if conn.execute(
        "INSERT OR IGNORE INTO settings(key,value,updated_at) VALUES (?1,?2,?3)",
        params![key, value, chrono::Utc::now().to_rfc3339()],
    )? == 1
    {
        return Ok(Admission::First(key));
    }
    let confirmed: bool = conn.query_row(
        "SELECT json_extract(value,'$.confirmed') FROM settings WHERE key=?1",
        params![key],
        |row| row.get(0),
    )?;
    if confirmed {
        // Also scrub a legacy confirmed record when its identity is reopened.
        conn.execute(
            "UPDATE settings SET value=json_object('confirmed',json('true')) WHERE key=?1",
            params![key],
        )?;
        return Ok(Admission::Confirmed);
    }
    conn.execute(
        "UPDATE settings SET value=json_remove(value,'$.plaintext') WHERE key=?1",
        params![key],
    )?;
    Ok(Admission::Unconfirmed)
}
pub(crate) fn ciphertext(db: &Database, key: &str, ciphertext: &[u8]) -> Result<(), StorageError> {
    let conn = db.connection()?;
    let value = serde_json::to_string(ciphertext)
        .map_err(|_| StorageError::InvalidData("Cannot retain anonymous ciphertext".into()))?;
    conn.execute(
        "UPDATE settings SET value=json_set(json_remove(value,'$.plaintext'),'$.ciphertext',json(?2)) WHERE key=?1 AND json_extract(value,'$.confirmed')=0",
        params![key, value],
    )?;
    Ok(())
}
pub(crate) fn confirm(db: &Database, key: &str) -> Result<(), StorageError> {
    let conn = db.connection()?;
    conn.execute(
        "UPDATE settings SET value=json_object('confirmed',json('true')) WHERE key=?1",
        params![key],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_cleanup_keeps_unknown_ciphertext_and_redacts_all_confirmed_content() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("legacy.db"), "test-only").unwrap();
        db.connection()
            .unwrap()
            .execute(
                "INSERT INTO settings(key,value,updated_at) VALUES (?1,?2,'now')",
                params![
                    "send.anonymous.v1:confirmed",
                    serde_json::json!({"plaintext":[1,2],"ciphertext":[3,4],"confirmed":true})
                        .to_string()
                ],
            )
            .unwrap();
        db.connection()
            .unwrap()
            .execute(
                "INSERT INTO settings(key,value,updated_at) VALUES (?1,?2,'now')",
                params![
                    "send.anonymous.v1:unknown",
                    serde_json::json!({"plaintext":[5,6],"ciphertext":[7,8],"confirmed":false})
                        .to_string()
                ],
            )
            .unwrap();
        scrub_legacy_content(&db).unwrap();
        for (key, expected) in [
            (
                "send.anonymous.v1:confirmed",
                serde_json::json!({"confirmed":true}),
            ),
            (
                "send.anonymous.v1:unknown",
                serde_json::json!({"ciphertext":[7,8],"confirmed":false}),
            ),
        ] {
            let raw: String = db
                .read_connection()
                .unwrap()
                .query_row(
                    "SELECT value FROM settings WHERE key=?1",
                    params![key],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&raw).unwrap(),
                expected
            );
        }
        scrub_legacy_content(&db).unwrap(); // durable idempotent cleanup
    }

    #[test]
    fn accepted_deposit_with_lost_receipt_never_reencrypts_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("submission.db");
        let db = Database::open(&path, "test-only").unwrap();
        let Admission::First(key) =
            prepare(&db, "owner", "peer", b"exact-padded-envelope").unwrap()
        else {
            panic!()
        };
        ciphertext(&db, &key, b"original-signal-ciphertext").unwrap();
        // Server accepted the deposit, then the receipt was lost. A parked
        // plaintext retry is refused BEFORE another ratchet advance.
        assert!(matches!(
            prepare(&db, "owner", "peer", b"exact-padded-envelope").unwrap(),
            Admission::Unconfirmed
        ));
        drop(db);
        let db = Database::open(&path, "test-only").unwrap();
        assert!(matches!(
            prepare(&db, "owner", "peer", b"exact-padded-envelope").unwrap(),
            Admission::Unconfirmed
        ));
        let raw: String = db
            .read_connection()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key=?1",
                params![key],
                |r| r.get(0),
            )
            .unwrap();
        let stored: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            stored["ciphertext"],
            serde_json::json!(b"original-signal-ciphertext".as_slice())
        );
        assert!(stored.get("plaintext").is_none());
        confirm(&db, &key).unwrap();
        let raw: String = db
            .read_connection()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key=?1",
                params![key],
                |r| r.get(0),
            )
            .unwrap();
        let tombstone: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(tombstone.get("plaintext").is_none());
        assert!(tombstone.get("ciphertext").is_none());
        assert_eq!(tombstone, serde_json::json!({"confirmed":true}));
        assert!(matches!(
            prepare(&db, "owner", "peer", b"exact-padded-envelope").unwrap(),
            Admission::Confirmed
        ));
        assert!(matches!(
            prepare(&db, "other-owner", "peer", b"exact-padded-envelope").unwrap(),
            Admission::First(_)
        ));
    }
}

// Only callers still strictly before encryption may discard a claim. Never
// release after an encryption error, cancellation, deposit or lost receipt.
pub(crate) fn discard_before_encryption(db: &Database, key: &str) {
    if let Ok(conn) = db.connection() {
        let _=conn.execute("DELETE FROM settings WHERE key=?1 AND json_extract(value,'$.ciphertext') IS NULL AND json_extract(value,'$.confirmed')=0",params![key]);
    }
}

// Scoped to one original core future (not a global flag). Once any route can
// encrypt, deposit, distribute a key or enqueue delivery, failure is ambiguous.
tokio::task_local! {
    pub(crate) static SEND_BOUNDARY: std::sync::Arc<SendBoundary>;
}

#[derive(Clone)]
pub(crate) struct SendSession {
    state: crate::state::AuthState,
    owner: Option<crate::domain::ids::UserId>,
    revision: u64,
}
impl SendSession {
    pub(crate) fn capture(state: &crate::state::AuthState) -> Self {
        let snapshot = state.get();
        Self {
            state: state.clone(),
            owner: snapshot.current_user_id,
            revision: snapshot.session_revision,
        }
    }
    fn check(&self) -> Result<(), StorageError> {
        let current = self.state.get();
        if self.owner.is_none()
            || current.current_user_id != self.owner
            || current.session_revision != self.revision
        {
            return Err(StorageError::InvalidData(
                "Send session expired before submission".into(),
            ));
        }
        Ok(())
    }
}
tokio::task_local! {
    pub(crate) static SEND_SESSION: SendSession;
}
pub(crate) fn check_send_session() -> Result<(), StorageError> {
    SEND_SESSION.try_with(SendSession::check).unwrap_or(Ok(()))
}
pub(crate) fn mark_submission_started() -> Result<(), StorageError> {
    check_send_session()?;
    mark_existing_submission();
    Ok(())
}
// An existing unconfirmed envelope stays ambiguous even after session expiry.
pub(crate) fn mark_existing_submission() {
    let _ = SEND_BOUNDARY.try_with(|started| {
        started
            .started
            .store(true, std::sync::atomic::Ordering::SeqCst);
    });
}

pub(crate) struct SendBoundary {
    started: std::sync::atomic::AtomicBool,
    row: std::sync::Mutex<Option<String>>,
    journal: Option<(std::sync::Arc<Database>, String, String)>,
}

pub(crate) fn bind_original_row(id: String) -> Result<(), StorageError> {
    SEND_BOUNDARY.try_with(|boundary| {
        if let Some((db, key, attempt)) = &boundary.journal {
            let conn = db.connection()?;
            conn.execute("UPDATE settings SET value=json_set(value,'$.message_id',?2) WHERE key=?1 AND json_extract(value,'$.attempt_id')=?3", params![key, id, attempt])?;
        }
        *boundary.row.lock().unwrap() = Some(id);
        Ok(())
    }).unwrap_or(Ok(()))
}

/// Preserve one future and report its delivery boundary and exact stored row.
pub(crate) async fn track_submission<F: std::future::Future>(
    send: F,
    journal: Option<(std::sync::Arc<Database>, String, String)>,
) -> (F::Output, bool, Option<String>) {
    let boundary = std::sync::Arc::new(SendBoundary {
        started: std::sync::atomic::AtomicBool::new(false),
        row: std::sync::Mutex::new(None),
        journal,
    });
    let result = SEND_BOUNDARY.scope(boundary.clone(), send).await;
    let row = boundary.row.lock().unwrap().clone();
    (
        result,
        boundary.started.load(std::sync::atomic::Ordering::SeqCst),
        row,
    )
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[tokio::test]
    async fn account_switch_during_upload_refuses_old_text_before_network() {
        use crate::{domain::ids::UserId, state::AuthState};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let state = AuthState::new();
        state.set_logged_in(UserId::new(), 1, "old".into());
        let session = SendSession::capture(&state);
        let calls = Arc::new(AtomicUsize::new(0));
        let (upload_tx, upload_rx) = tokio::sync::oneshot::channel();
        let counter = calls.clone();
        let send = tokio::spawn(SEND_SESSION.scope(
            session,
            track_submission(
                async move {
                    upload_rx.await.unwrap();
                    check_send_session()?; // exact post-upload guard in core
                    mark_submission_started()?;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, StorageError>(())
                },
                None,
            ),
        ));
        state.set_logged_in(UserId::new(), 1, "new".into());
        upload_tx.send(()).unwrap();
        let (result, started, _) = send.await.unwrap();
        assert!(result.is_err());
        assert!(!started);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn parallel_preparation_and_partial_fanout_do_not_share_boundary_state() {
        let local = track_submission(async { "rate-limit expiry pending" }, None);
        let partial = track_submission(
            async {
                mark_submission_started().unwrap();
                tokio::task::yield_now().await;
                "first deposit accepted, second failed"
            },
            None,
        );
        let ((_, local_started, _), (_, partial_started, _)) = tokio::join!(local, partial);
        assert!(!local_started);
        assert!(partial_started);
    }

    #[tokio::test]
    async fn cancellation_after_original_row_binding_keeps_durable_exact_identity() {
        let dir = tempfile::tempdir().unwrap();
        let db =
            std::sync::Arc::new(Database::open(&dir.path().join("bound.db"), "test-only").unwrap());
        db.connection()
            .unwrap()
            .execute(
                "INSERT INTO settings(key,value,updated_at) VALUES ('claim',?1,'now')",
                params![serde_json::json!({"attempt_id":"original","message_id":null}).to_string()],
            )
            .unwrap();
        let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
        let send = tokio::spawn(track_submission(
            async move {
                bind_original_row("exact-original-row".into()).unwrap();
                mark_submission_started().unwrap();
                bound_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            },
            Some((db.clone(), "claim".into(), "original".into())),
        ));
        bound_rx.await.unwrap();
        send.abort();
        assert!(send.await.unwrap_err().is_cancelled());
        let id: String = db
            .read_connection()
            .unwrap()
            .query_row(
                "SELECT json_extract(value,'$.message_id') FROM settings WHERE key='claim'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(id, "exact-original-row");
    }
}
