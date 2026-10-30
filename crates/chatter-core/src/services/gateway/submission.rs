//! Anonymous deposits cannot be retried by re-encrypting a parked plaintext.
//! Retain the first ciphertext in SQLCipher before attempting deposit.
use crate::{error::StorageError, storage::database::Database};
use rusqlite::params;
use sha2::{Digest, Sha256};

/// Refresh after every pending-state write. If that read fails, retain an
/// immediate hint so the next healthy mailbox cycle retries the SQL read.
/// The lock prevents a concurrent refresh from overwriting a newer write.
fn schedule_changed(db: &Database, owner: &str) {
    let mut due = db.retained_retry_due.lock();
    due.insert(owner.to_owned(), read_next_due(db, owner).unwrap_or(Some(0)));
}

fn read_next_due(db: &Database, owner: &str) -> Result<Option<i64>, StorageError> {
    let conn = db.read_connection()?;
    conn.query_row(
        "SELECT MIN(CASE WHEN json_extract(value,'$.next_attempt') > json_extract(value,'$.lease_until') THEN json_extract(value,'$.next_attempt') ELSE json_extract(value,'$.lease_until') END) FROM settings WHERE key LIKE 'send.anonymous.v1:%' AND json_extract(value,'$.owner')=?1 AND json_extract(value,'$.ciphertext') IS NOT NULL AND json_extract(value,'$.confirmed')=0",
        [owner],
        |row| row.get(0),
    ).map_err(Into::into)
}

fn retry_is_due(db: &Database, owner: &str) -> Result<bool, StorageError> {
    let mut due = db.retained_retry_due.lock();
    let next = match due.get(owner) {
        Some(next) => *next,
        None => {
            let next = read_next_due(db, owner)?;
            due.insert(owner.to_owned(), next);
            next
        }
    };
    Ok(next.is_some_and(|time| time <= chrono::Utc::now().timestamp()))
}

fn refresh_retry_due(db: &Database, owner: &str) -> Result<(), StorageError> {
    let mut due = db.retained_retry_due.lock();
    due.insert(owner.to_owned(), read_next_due(db, owner)?);
    Ok(())
}

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
        Some(Content::DataMessage(data)) if data.delete.is_none() && !crate::services::disappearing::is_timer_update(data)
    )
}

pub(crate) fn prepare(
    db: &Database,
    owner: &str,
    peer: &str,
    plaintext: &[u8],
) -> Result<Admission, StorageError> {
    let mut hash = Sha256::new();
    hash.update(owner.as_bytes());
    hash.update([0]);
    hash.update(peer.as_bytes());
    hash.update([0]);
    hash.update(plaintext);
    let key = format!("send.anonymous.v1:{}", hex::encode(hash.finalize()));
    let value = serde_json::json!({"ciphertext":null,"confirmed":false,"owner":owner,"peer":peer,"next_attempt":0,"lease_until":0,"attempts":0}).to_string();
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
    Ok(if confirmed {
        Admission::Confirmed
    } else {
        Admission::Unconfirmed
    })
}
/// Persist the destination and completion identity with the original sealed bytes.
pub(crate) fn retain_deposit(
    db: &Database,
    key: &str,
    ciphertext_bytes: &[u8],
    mailbox: &[u8; 32],
    content: &crate::proto::content_v1::Content,
) -> Result<(), StorageError> {
    use crate::proto::content_v1::content::Content;
    let (conversation, kind, inner) = match content.content.as_ref() {
        Some(Content::AnonDmMessage(m)) => (
            uuid::Uuid::from_slice(&m.conversation_id),
            "anon_dm",
            m.inner_content.as_deref(),
        ),
        Some(Content::AnonGroupMessage(m)) => (
            uuid::Uuid::from_slice(&m.group_id),
            "anon_group",
            m.inner_content.as_deref(),
        ),
        _ => return Err(StorageError::InvalidData("Invalid retained post".into())),
    };
    let conversation = conversation
        .map_err(|_| StorageError::InvalidData("Invalid retained conversation".into()))?
        .to_string();
    let inner = inner.ok_or_else(|| StorageError::InvalidData("Missing retained post".into()))?;
    let Some(Content::DataMessage(data)) = inner.content.as_ref() else {
        return Err(StorageError::InvalidData(
            "Invalid retained user post".into(),
        ));
    };
    let conn = db.connection()?;
    let owner: String = conn.query_row(
        "SELECT json_extract(value,'$.owner') FROM settings WHERE key=?1",
        [key],
        |r| r.get(0),
    )?;
    let row: String = conn.query_row("SELECT id FROM messages WHERE conversation_id=?1 AND conversation_type=?2 AND sender_id=?3 AND sent_at=?4 AND is_deleted=0", params![conversation,kind,owner,data.timestamp as i64], |r| r.get(0))?;
    let ledger = (kind == "anon_group")
        .then(|| crate::services::anon_group::send_ledger::key(&owner, &conversation, inner));
    conn.execute("UPDATE settings SET value=json_set(value,'$.mailbox',json(?2),'$.row',?3,'$.ledger',?4,'$.conversation',?5,'$.kind',?6,'$.lease_until',?7) WHERE key=?1 AND json_extract(value,'$.ciphertext') IS NULL AND json_extract(value,'$.confirmed')=0", params![key,serde_json::to_string(mailbox).unwrap(),row,ledger,conversation,kind,chrono::Utc::now().timestamp()+90])?;
    drop(conn);
    schedule_changed(db, &owner);
    // Destination first, bytes last: recovery never sees an envelope without
    // the destination and completion identity needed to replay it.
    ciphertext(db, key, ciphertext_bytes)
}

pub(crate) fn ciphertext(db: &Database, key: &str, bytes: &[u8]) -> Result<(), StorageError> {
    let value = serde_json::to_string(bytes)
        .map_err(|_| StorageError::InvalidData("Cannot retain anonymous ciphertext".into()))?;
    let conn = db.connection()?;
    let owner: String = conn.query_row("SELECT json_extract(value,'$.owner') FROM settings WHERE key=?1", [key], |r| r.get(0))?;
    conn.execute("UPDATE settings SET value=json_set(value,'$.ciphertext',json(?2)) WHERE key=?1 AND json_extract(value,'$.ciphertext') IS NULL AND json_extract(value,'$.confirmed')=0",params![key,value])?;
    drop(conn);
    schedule_changed(db, &owner);
    Ok(())
}

#[derive(Clone, Copy, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RetainedKind {
    AnonDm,
    AnonGroup,
}

#[derive(serde::Deserialize)]
struct RetainedDeposit {
    ciphertext: Vec<u8>,
    mailbox: [u8; 32],
    peer: String,
    conversation: String,
    row: String,
    kind: RetainedKind,
    ledger: Option<String>,
    attempts: u64,
}

/// Retry only durable sealed bytes. A persisted lease also bounds parallel taps,
/// poll hooks, cancellation and process restarts; no crypto or token code runs.
pub(crate) async fn retry_pending(
    db: &std::sync::Arc<Database>,
    pool: &std::sync::Arc<crate::network::grpc::client_pool::GrpcClientPool>,
    owner: &str,
    row: Option<&str>,
    events: Option<&tokio::sync::broadcast::Sender<crate::inbox::handlers::DomainEvent>>,
) -> Result<(), StorageError> {
    if row.is_none() && !retry_is_due(db, owner)? {
        return Ok(());
    }
    let entries = {
        let conn = db.read_connection()?;
        let mut stmt = conn.prepare("SELECT key FROM settings WHERE key LIKE 'send.anonymous.v1:%' AND json_extract(value,'$.owner')=?1 AND json_extract(value,'$.ciphertext') IS NOT NULL AND (?2 IS NULL OR json_extract(value,'$.row')=?2) AND (?2 IS NOT NULL OR json_extract(value,'$.next_attempt')<=?3) AND json_extract(value,'$.lease_until')<=?3 ORDER BY updated_at LIMIT 8")?;
        let entries = stmt
            .query_map(params![owner, row, chrono::Utc::now().timestamp()], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        entries
    };
    for key in entries {
        // A poll belongs to its login session; never replay another account's
        // retained traffic if that session was replaced while it was queued.
        let current: Option<String> =
            crate::storage::repositories::SettingsRepository::new(db.clone())
                .get(crate::storage::settings_keys::CURRENT_USER_ID)?;
        if current.as_deref() != Some(owner) {
            break;
        }
        let raw: Option<String> = {
            use rusqlite::OptionalExtension;
            let conn = db.connection()?;
            conn.query_row("UPDATE settings SET value=json_set(value,'$.lease_until',?2+90,'$.attempts',json_extract(value,'$.attempts')+1) WHERE key=?1 AND json_extract(value,'$.lease_until')<=?2 AND json_extract(value,'$.confirmed')=0 RETURNING value", params![key,chrono::Utc::now().timestamp()], |r|r.get(0)).optional()?
        };
        let Some(raw) = raw else { continue };
        schedule_changed(db, owner);
        let record: RetainedDeposit = serde_json::from_str(&raw)
            .map_err(|_| StorageError::InvalidData("Invalid retained submission".into()))?;
        let mailbox = record.mailbox;
        // Refuse deleted/blocked conversations or changed group membership/key.
        let peer = record
            .peer
            .parse::<crate::domain::UserId>()
            .map_err(|_| StorageError::InvalidData("Invalid retained peer".into()))?;
        let conversation = record.conversation.as_str();
        let id = record.row.as_str();
        let valid = {
            let conn = db.read_connection()?;
            conn.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1 AND sender_id=?2 AND is_deleted=0)",params![id,owner],|r|r.get::<_,bool>(0))?
        };
        if !valid {
            continue;
        }
        if matches!(record.kind, RetainedKind::AnonGroup) {
            let group = conversation
                .parse::<crate::domain::anon_group::AnonGroupId>()
                .map_err(|_| StorageError::InvalidData("Invalid retained group".into()))?;
            let repo = crate::storage::repositories::AnonGroupRepository::new(db.clone());
            let me = owner
                .parse::<crate::domain::UserId>()
                .map_err(|_| StorageError::InvalidData("Invalid retained owner".into()))?;
            if !repo
                .get_group(&group)?
                .is_some_and(|group| group.is_ready())
                || !repo.is_member(&group, &me)?
                || !repo.current_post_recipient(&group, &peer, &mailbox)?
            {
                continue;
            }
        } else {
            let repo = crate::storage::repositories::AnonDmRepository::new(db.clone());
            let dm = conversation
                .parse::<crate::domain::anon_dm::AnonDmId>()
                .map_err(|_| StorageError::InvalidData("Invalid retained chat".into()))?;
            if repo.is_user_blocked(&peer)?
                || !repo.get_conversation(&dm)?.is_some_and(|dm| {
                    dm.is_ready() && dm.peer_id == peer && dm.peer_mailbox_pk == Some(mailbox)
                })
            {
                continue;
            }
        }
        if crate::services::mailbox_manager::MailboxManager::new(pool.clone())
            .send_to_secret_mailbox(
                &mailbox,
                record.ciphertext,
                crate::proto::mailbox_v1::EnvelopeType::SealedSender,
            )
            .await
            .is_err()
        {
            let attempts = record.attempts.min(5);
            db.connection()?.execute("UPDATE settings SET value=json_set(value,'$.next_attempt',?2,'$.lease_until',0) WHERE key=?1 AND json_extract(value,'$.confirmed')=0",params![key,chrono::Utc::now().timestamp()+ (10i64 << attempts)])?;
            schedule_changed(db, owner);
            break; // one failing route per health notification, never a busy loop
        }
        // Commit recipient receipt and row settlement before pruning ciphertext.
        let complete = match (record.kind, record.ledger.as_deref()) {
            (RetainedKind::AnonGroup, Some(ledger)) => {
                crate::services::anon_group::send_ledger::record(
                    db,
                    ledger,
                    &peer.to_string(),
                    "confirmed",
                )?
                .is_some()
            }
            (RetainedKind::AnonDm, None) => true,
            _ => {
                return Err(StorageError::InvalidData(
                    "Invalid retained completion identity".into(),
                ));
            }
        };
        if complete {
            db.connection()?.execute("UPDATE messages SET delivery_status=CASE WHEN delivery_status IN ('delivered','read') THEN delivery_status ELSE 'sent' END,failure_reason=NULL WHERE id=?1",[id])?;
            if let (Some(events), Ok(message_id), Ok(channel_id)) =
                (events, id.parse(), conversation.parse())
            {
                let _ = events.send(
                    crate::inbox::handlers::DomainEvent::MessageDeliveryChanged {
                        message_id,
                        channel_id,
                        status: "sent".into(),
                    },
                );
            }
        }
        confirm(db, &key)?;
    }
    refresh_retry_due(db, owner)
}

/// The original request completed without a receipt. Its ciphertext stays
/// immutable, but a tap may now take the lease immediately.
pub(crate) fn retry_later(db: &Database, key: &str) -> Result<(), StorageError> {
    let conn = db.connection()?;
    let owner: String = conn.query_row("SELECT json_extract(value,'$.owner') FROM settings WHERE key=?1", [key], |r| r.get(0))?;
    conn.execute("UPDATE settings SET value=json_set(value,'$.lease_until',0,'$.next_attempt',?2) WHERE key=?1 AND json_extract(value,'$.confirmed')=0",params![key,chrono::Utc::now().timestamp()+20])?;
    drop(conn);
    schedule_changed(db, &owner);
    Ok(())
}

pub(crate) fn retained_row(db: &Database, owner: &str, row: &str) -> bool {
    db.read_connection().and_then(|conn|conn.query_row("SELECT EXISTS(SELECT 1 FROM settings WHERE key LIKE 'send.anonymous.v1:%' AND json_extract(value,'$.owner')=?1 AND json_extract(value,'$.row')=?2 AND json_extract(value,'$.ciphertext') IS NOT NULL)",params![owner,row],|r|r.get(0)).map_err(Into::into)).unwrap_or(false)
}
pub(crate) fn confirm(db: &Database, key: &str) -> Result<(), StorageError> {
    let conn = db.connection()?;
    let owner: Option<String> = conn.query_row("SELECT json_extract(value,'$.owner') FROM settings WHERE key=?1", [key], |r| r.get(0))?;
    conn.execute(
        "UPDATE settings SET value=json_object('confirmed',json('true')) WHERE key=?1",
        params![key],
    )?;
    drop(conn);
    if let Some(owner) = owner {
        schedule_changed(db, &owner);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_deposit_with_lost_receipt_never_reencrypts_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("submission.db");
        let db = Database::open(&path, "test-only").unwrap();
        db.run_migrations()
            .expect("migrate throwaway test database");
        let db = std::sync::Arc::new(db);
        let owner = crate::domain::UserId::new();
        let peer = crate::domain::UserId::new();
        let conversation = crate::domain::anon_dm::AnonDmId::new();
        let inner = crate::services::gateway::builders::build_text_content("post", 7);
        let content = crate::proto::content_v1::Content {
            content: Some(crate::proto::content_v1::content::Content::AnonDmMessage(
                Box::new(crate::proto::content_v1::AnonDmMessage {
                    conversation_id: conversation.to_vec(),
                    inner_content: Some(Box::new(inner)),
                }),
            )),
            timestamp: 8,
            padding: vec![9],
            ephemeral_reply_to: None,
        };
        assert!(user_post(&content));
        let mut control = content.clone();
        if let Some(crate::proto::content_v1::content::Content::AnonDmMessage(message)) =
            control.content.as_mut()
        {
            if let Some(crate::proto::content_v1::content::Content::DataMessage(data)) =
                message.inner_content.as_mut().unwrap().content.as_mut()
            {
                data.delete = Some(Default::default());
            }
        }
        assert!(
            !user_post(&control),
            "delete controls have no outgoing post row"
        );
        let message = crate::domain::message::Message::new_text(
            conversation.into(),
            crate::domain::message::ConversationType::AnonDm,
            Some(owner),
            "post".into(),
            7,
        );
        crate::storage::MessageRepository::new(db.clone())
            .insert_outgoing(&message)
            .unwrap();
        let Admission::First(key) = prepare(
            &db,
            &owner.to_string(),
            &peer.to_string(),
            b"exact-padded-envelope",
        )
        .unwrap() else {
            panic!()
        };
        retain_deposit(&db, &key, b"original-signal-ciphertext", &[3; 32], &content).unwrap();
        // A second retention call cannot replace bytes or destination.
        retain_deposit(&db, &key, b"different-ciphertext", &[4; 32], &content).unwrap();
        // Server accepted the deposit, then the receipt was lost. A parked
        // plaintext retry is refused BEFORE another ratchet advance.
        assert!(matches!(
            prepare(
                &db,
                &owner.to_string(),
                &peer.to_string(),
                b"exact-padded-envelope"
            )
            .unwrap(),
            Admission::Unconfirmed
        ));
        drop(db);
        let db = Database::open(&path, "test-only").unwrap();
        db.run_migrations()
            .expect("migrate throwaway test database");
        assert!(matches!(
            prepare(
                &db,
                &owner.to_string(),
                &peer.to_string(),
                b"exact-padded-envelope"
            )
            .unwrap(),
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
        assert_eq!(stored["mailbox"], serde_json::json!([3u8; 32].as_slice()));
        assert_eq!(stored["row"], message.id.to_string());
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
            prepare(
                &db,
                &owner.to_string(),
                &peer.to_string(),
                b"exact-padded-envelope"
            )
            .unwrap(),
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
/// Carries the original future's admission boundary across the Signal actor.
/// Start only after every exact envelope fits, before the first durable write.
#[derive(Clone)]
pub(crate) struct SubmissionPermit {
    boundary: Option<std::sync::Arc<SendBoundary>>,
    session: Option<SendSession>,
}
impl SubmissionPermit {
    pub(crate) fn capture() -> Self {
        Self {
            boundary: SEND_BOUNDARY.try_with(Clone::clone).ok(),
            session: SEND_SESSION.try_with(Clone::clone).ok(),
        }
    }
    pub(crate) fn start(&self) -> Result<(), StorageError> {
        if let Some(session) = &self.session {
            session.check()?;
        }
        if let Some(boundary) = &self.boundary {
            boundary.start()?;
        }
        Ok(())
    }
}

pub(crate) fn mark_submission_started() -> Result<(), StorageError> {
    check_send_session()?;
    SEND_BOUNDARY
        .try_with(|boundary| boundary.start())
        .unwrap_or(Ok(()))
}
/// Whether the current send's boundary started (`false` outside any send).
pub(crate) fn submission_started() -> bool {
    SEND_BOUNDARY
        .try_with(|boundary| boundary.started.load(std::sync::atomic::Ordering::SeqCst))
        .unwrap_or(false)
}
// An existing unconfirmed envelope stays ambiguous even after session expiry.
// Best effort durably: its own admission record already refuses a re-encrypt.
pub(crate) fn mark_existing_submission() {
    let _ = SEND_BOUNDARY.try_with(|boundary| {
        if boundary.start().is_err() {
            boundary
                .started
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    });
}

pub(crate) struct SendBoundary {
    started: std::sync::atomic::AtomicBool,
    row: std::sync::Mutex<Option<String>>,
    journal: Option<(std::sync::Arc<Database>, String, String)>,
}
impl SendBoundary {
    /// Durable before in-memory, and both before the first ratchet commit or
    /// request byte: a journal a dead process left with `started = false`
    /// provably never reached encryption, so it can be handed back instead of
    /// blocking its conversation (`send_attempt::ensure_send_available`).
    fn start(&self) -> Result<(), StorageError> {
        if !self.started.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some((db, key, attempt)) = &self.journal {
                db.connection()?.execute("UPDATE settings SET value=json_set(value,'$.started',json('true')) WHERE key=?1 AND json_extract(value,'$.attempt_id')=?2", params![key, attempt])?;
            }
        }
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

pub(crate) fn bind_original_row(id: String) -> Result<(), StorageError> {
    SEND_BOUNDARY.try_with(|boundary| {
        // Keep the exact row visible to in-process failure settlement even if
        // updating the durable journal below fails after the insert.
        *boundary.row.lock().unwrap() = Some(id.clone());
        if let Some((db, key, attempt)) = &boundary.journal {
            let conn = db.connection()?;
            conn.execute("UPDATE settings SET value=json_set(value,'$.message_id',?2) WHERE key=?1 AND json_extract(value,'$.attempt_id')=?3", params![key, id, attempt])?;
        }
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
    async fn started_boundary_is_durable_so_a_dead_process_claim_is_classified() {
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(
            Database::open(&dir.path().join("started.db"), "test-only").unwrap(),
        );
        db.run_migrations().unwrap();
        let started = |db: &Database| -> bool {
            db.read_connection()
                .unwrap()
                .query_row(
                    "SELECT json_extract(value,'$.started') FROM settings WHERE key='claim'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
        };
        db.connection()
            .unwrap()
            .execute(
                "INSERT INTO settings(key,value,updated_at) VALUES ('claim',?1,'now')",
                params![serde_json::json!({"attempt_id":"a","started":false}).to_string()],
            )
            .unwrap();
        let journal = Some((db.clone(), "claim".into(), "a".into()));
        track_submission(async {}, journal.clone()).await;
        assert!(!started(&db), "preparation alone never marks the claim");
        let permit = track_submission(async { SubmissionPermit::capture() }, journal)
            .await
            .0;
        assert!(!started(&db));
        // The actor's permit, outside the task, still writes the claim first.
        let (_, in_memory, _) = track_submission(
            async move {
                std::thread::spawn(move || permit.start().unwrap())
                    .join()
                    .unwrap()
            },
            None,
        )
        .await;
        assert!(started(&db));
        assert!(!in_memory, "only the captured send's boundary moved");
    }

    #[tokio::test]
    async fn cancellation_after_original_row_binding_keeps_durable_exact_identity() {
        let dir = tempfile::tempdir().unwrap();
        let db =
            std::sync::Arc::new(Database::open(&dir.path().join("bound.db"), "test-only").unwrap());
        db.run_migrations()
            .expect("migrate throwaway test database");
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
