//! Durable fail-closed admission for composed sends. An absent response is not
//! evidence of non-delivery. Until exact-envelope retries exist for every
//! transport, an unresolved attempt must not be replaced or encrypted again.
//! A claim stops holding its conversation once its row is gone, once the
//! sealed DM outbox (`dm_outbox`) or the channel queue (`channel_send`) holds
//! the row's exact request, or when a dead process left it before the
//! submission boundary.
use super::{ChatterClient, SendOptions};
use crate::domain::ids::MessageId;
use crate::error::StorageError;
use crate::services::gateway::{QuoteIntent, SendError};
use crate::state::{ActiveConversation, chat_state::PendingAttachment};
use crate::storage::database::Database;
use prost::Message as _;
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct PendingSend {
    attempt_id: String,
    text: String,
    attachments: Vec<serde_json::Value>,
    quote: Option<serde_json::Value>,
    options: Option<String>,
    message_id: Option<String>,
    #[serde(default)]
    original_row: Option<serde_json::Value>,
    #[serde(default)]
    fresh_post: bool,
    /// `Some(false)` until the submission boundary durably starts
    /// (`SendBoundary::start`). `None` is a journal written before this field
    /// existed and is treated as started: fail closed.
    #[serde(default)]
    started: Option<bool>,
    /// The process that claimed it. A claim from another process that never
    /// started was abandoned by a dead process, not by a send still running.
    #[serde(default)]
    process: Option<String>,
}

static PROCESS: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| uuid::Uuid::new_v4().to_string());

fn abandoned_before_submission(pending: &PendingSend) -> bool {
    pending.started == Some(false) && pending.process.as_deref() != Some(PROCESS.as_str())
}

fn confirmation_row_id(pending: &PendingSend) -> Option<MessageId> {
    let id = pending.message_id.as_ref()?;
    // A late receipt for the old failed public-channel row cannot prove the
    // fresh retry post was accepted. Only its exact returned row can reconcile.
    if pending.fresh_post
        && pending
            .original_row
            .as_ref()
            .and_then(|row| row.get("id"))
            .and_then(serde_json::Value::as_str)
            == Some(id.as_str())
    {
        return None;
    }
    id.parse().ok()
}

fn pending_key(owner: &str, conversation: ActiveConversation) -> String {
    format!("send.unresolved.v1:{owner}:{conversation:?}")
}
fn key_owner(key: &str) -> String {
    key.trim_start_matches("send.unresolved.v1:")
        .split(':')
        .next()
        .unwrap_or_default()
        .to_owned()
}
fn storage_error(error: impl Into<StorageError>) -> SendError {
    crate::services::message::MessageError::Storage(error.into()).into()
}
fn read_pending(db: &Database, key: &str) -> Result<Option<PendingSend>, SendError> {
    let conn = db.read_connection().map_err(storage_error)?;
    let raw: Result<String, _> = conn.query_row(
        "SELECT value FROM settings WHERE key=?1",
        params![key],
        |row| row.get(0),
    );
    match raw {
        Ok(raw) => serde_json::from_str(&raw).map(Some).map_err(|_| {
            storage_error(StorageError::InvalidData(
                "Invalid pending send journal".into(),
            ))
        }),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(storage_error(StorageError::Database(error))),
    }
}
fn claim(db: &Database, key: &str, pending: &PendingSend) -> Result<(), SendError> {
    let raw = serde_json::to_string(pending).map_err(|_| {
        storage_error(StorageError::InvalidData(
            "Cannot retain pending send".into(),
        ))
    })?;
    let conn = db.connection().map_err(storage_error)?;
    let completed: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM settings WHERE key=?1)",
            params![format!("{key}:confirmed:{}", pending.attempt_id)],
            |row| row.get(0),
        )
        .map_err(|e| storage_error(StorageError::Database(e)))?;
    if completed {
        return Err(SendError::AttemptAlreadyConfirmed);
    }
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO settings (key,value,updated_at) VALUES (?1,?2,?3)",
            params![key, raw, chrono::Utc::now().to_rfc3339()],
        )
        .map_err(|e| storage_error(StorageError::Database(e)))?;
    if inserted != 1 {
        let original: String = conn
            .query_row(
                "SELECT json_extract(value,'$.attempt_id') FROM settings WHERE key=?1",
                params![key],
                |row| row.get(0),
            )
            .map_err(|e| storage_error(StorageError::Database(e)))?;
        return Err(if original == pending.attempt_id {
            SendError::DeliveryUnconfirmed
        } else {
            SendError::SendInProgress
        });
    }
    Ok(())
}
fn discard_preparation(db: &Database, key: &str, attempt: &str) -> Result<(), SendError> {
    let conn = db.connection().map_err(storage_error)?;
    conn.execute(
        "DELETE FROM settings WHERE key=?1 AND json_extract(value,'$.attempt_id')=?2",
        params![key, attempt],
    )
    .map_err(|e| storage_error(StorageError::Database(e)))?;
    Ok(())
}

pub(super) fn settle_failure(
    db: &Database,
    key: &str,
    attempt: uuid::Uuid,
    started: bool,
    row: Option<String>,
    error: SendError,
) -> Result<SendError, SendError> {
    if !started {
        if let Some(id) = row.as_ref() {
            let conn = db.connection().map_err(storage_error)?;
            conn.execute("UPDATE messages SET delivery_status='failed',failure_reason='Message not submitted' WHERE id=?1 AND delivery_status NOT IN ('sent','delivered','read')", params![id])
                .map_err(|e| storage_error(StorageError::Database(e)))?;
        }
        discard_preparation(db, key, &attempt.to_string())?;
        return Ok(match error {
            SendError::BeforeSubmission(original) => *original,
            original => original,
        });
    }
    if let Some(id) = row {
        let conn = db.connection().map_err(storage_error)?;
        conn.execute("UPDATE settings SET value=json_set(value,'$.message_id',?2) WHERE key=?1 AND json_extract(value,'$.attempt_id')=?3", params![key, id, attempt.to_string()])
            .map_err(|e| storage_error(StorageError::Database(e)))?;
        // The row stays "sending" (retry stays locked) but says why, so its
        // bubble reads "not confirmed" rather than "sending" for good.
        conn.execute(
            "UPDATE messages SET failure_reason=?2 WHERE id=?1 AND delivery_status='sending'",
            params![id, crate::services::message::core::DELIVERY_UNCONFIRMED],
        )
        .map_err(|e| storage_error(StorageError::Database(e)))?;
    }
    Ok(SendError::DeliveryUnconfirmed)
}

fn release(db: &Database, key: &str, attempt: &str) -> Result<(), SendError> {
    let mut conn = db.connection().map_err(storage_error)?;
    let tx = conn
        .transaction()
        .map_err(|e| storage_error(StorageError::Database(e)))?;
    // Remember completed identities without deleting another in-flight claim.
    tx.execute("INSERT OR IGNORE INTO settings(key,value,updated_at) SELECT ?3,json_object('attempt_id',json_extract(value,'$.attempt_id'),'message_id',json_extract(value,'$.message_id')),updated_at FROM settings WHERE key=?1 AND json_extract(value,'$.attempt_id')=?2",params![key,attempt,format!("{key}:confirmed:{attempt}")]).map_err(|e|storage_error(StorageError::Database(e)))?;
    tx.execute(
        "DELETE FROM settings WHERE key=?1 AND json_extract(value,'$.attempt_id')=?2",
        params![key, attempt],
    )
    .map_err(|e| storage_error(StorageError::Database(e)))?;
    tx.commit()
        .map_err(|e| storage_error(StorageError::Database(e)))?;
    Ok(())
}

impl ChatterClient {
    fn send_attempt_key(&self, conversation: ActiveConversation) -> Result<String, SendError> {
        let owner = self
            .auth_state()
            .get()
            .current_user_id
            .ok_or(SendError::NotLoggedIn)?;
        Ok(pending_key(&owner.to_string(), conversation))
    }

    /// Does not consume the draft/tray. Persists across restarts and logout;
    /// another account cannot inherit or clear this account's pending attempt.
    pub fn ensure_send_available(&self, conversation: ActiveConversation) -> Result<(), SendError> {
        let key = self.send_attempt_key(conversation)?;
        if let Some(pending) = read_pending(&self.services.database, &key)? {
            if let Some(id) = confirmation_row_id(&pending) {
                let db = &self.services.database;
                let row = self.messages().get_message_local(&id)?;
                // The sealed DM outbox or the durable channel queue holds this
                // row's exact request and replays only that, so a new send
                // cannot duplicate it.
                let (owner, row_id) = (key_owner(&key), id.to_string());
                let owned = crate::services::message::dm_outbox::owns(db, &owner, &row_id)
                    .map_err(storage_error)?
                    || crate::services::message::channel_send_owns(db, &owner, &row_id)
                        .map_err(storage_error)?;
                // Deleted (by the user or the disappearing reaper): no retry
                // of that row can exist any more, and a new send is not one.
                if owned || row.as_ref().is_none_or(|m| m.is_deleted) {
                    release(db, &key, &pending.attempt_id)?;
                    return Ok(());
                }
                // A claim journalled before `started` existed, on a friend DM
                // the outbox does not hold: nothing will ever settle it, so it
                // would block this conversation for good. Fail closed but
                // recoverable: the row becomes a failed bubble ("Delivery
                // unconfirmed") the user may retry under its original sent_at,
                // which the receiver deduplicates. Never resent on its own.
                if pending.started.is_none()
                    && row.as_ref().is_some_and(|m| {
                        m.conversation_type == crate::domain::message::ConversationType::ServerDm
                            && matches!(m.delivery_status.as_str(), "sending" | "failed")
                    })
                {
                    db.connection().map_err(storage_error)?
                        .execute("UPDATE messages SET delivery_status='failed',failure_reason='Delivery unconfirmed' WHERE id=?1 AND delivery_status='sending'", params![row_id])
                        .map_err(|e| storage_error(StorageError::Database(e)))?;
                    release(db, &key, &pending.attempt_id)?;
                    return Ok(());
                }
            }
            if abandoned_before_submission(&pending) {
                self.recover_abandoned_preparation(conversation, &key, pending)?;
                return Ok(());
            }
            // A bootstrap/receipt can confirm a retained stored row later.
            // A failed/sending row is never treated as proof of non-delivery.
            if let Some(id) = confirmation_row_id(&pending) {
                let group_ready = crate::services::anon_group::send_ledger::row_complete(
                    &self.services.database,
                    &id.to_string(),
                )
                .map_err(storage_error)?
                    != Some(false);
                if group_ready
                    && self.messages().get_message_local(&id)?.is_some_and(|m| {
                        matches!(m.delivery_status.as_str(), "sent" | "delivered" | "read")
                    })
                {
                    release(&self.services.database, &key, &pending.attempt_id)?;
                    return Ok(());
                }
            }
            return Err(SendError::DeliveryUnconfirmed);
        }
        Ok(())
    }

    pub(super) fn retain_send_attempt(
        &self,
        attempt: uuid::Uuid,
        conversation: ActiveConversation,
        text: &str,
        attachments: &[PendingAttachment],
        quote: Option<&QuoteIntent>,
        options: Option<&SendOptions>,
    ) -> Result<String, SendError> {
        if let Err(error) = self.ensure_send_available(conversation) {
            if matches!(&error, SendError::DeliveryUnconfirmed) {
                let key = self.send_attempt_key(conversation)?;
                let original = read_pending(&self.services.database, &key)?;
                return Err(
                    if original
                        .as_ref()
                        .is_some_and(|p| p.attempt_id == attempt.to_string())
                    {
                        SendError::DeliveryUnconfirmed
                    } else {
                        SendError::SendInProgress
                    },
                );
            }
            return Err(error);
        }
        let key = self.send_attempt_key(conversation)?;
        let pending = PendingSend {
            attempt_id: attempt.to_string(), text:text.to_owned(),
            attachments: attachments.iter().map(|a| serde_json::json!({"id":a.id.to_string(),"filename":a.filename,"path":a.path,"size":a.size,"mime_type":a.mime_type,"pointer":a.pointer.as_ref().map(|p|p.encode_to_vec())})).collect(),
            quote: quote.map(|q|serde_json::json!({"author":q.target_author_id.to_string(),"timestamp":q.target_timestamp,"preview":q.target_preview})),
            options:options.map(|o|format!("{o:?}")), message_id:None, original_row:None, fresh_post:false,
            started: Some(false), process: Some(PROCESS.clone()),
        };
        claim(&self.services.database, &key, &pending)?;
        Ok(key)
    }

    /// Claim the original stored retry identity BEFORE transport preparation.
    /// Cancellation/restart retains the exact row and blocks any fresh post.
    pub(super) fn retain_retry_attempt(
        &self,
        conversation: ActiveConversation,
        message: &crate::domain::message::Message,
    ) -> Result<(String, uuid::Uuid), SendError> {
        self.ensure_send_available(conversation)?;
        let key = self.send_attempt_key(conversation)?;
        let attempt = *message.id.as_uuid();
        let original_row = serde_json::to_value(message).map_err(|_| {
            storage_error(StorageError::InvalidData(
                "Cannot retain original retry row".into(),
            ))
        })?;
        claim(
            &self.services.database,
            &key,
            &PendingSend {
                attempt_id: attempt.to_string(),
                text: message.as_text().unwrap_or("").to_owned(),
                attachments: vec![],
                quote: None,
                options: None,
                message_id: Some(message.id.to_string()),
                original_row: Some(original_row),
                fresh_post: message.conversation_type
                    == crate::domain::message::ConversationType::Channel
                    && self
                        .messages()
                        .channel_repo
                        .get(&message.conversation_id)
                        .map_err(storage_error)?
                        .is_some_and(|channel| {
                            matches!(
                                channel.channel_type,
                                crate::domain::channel::ChannelType::Public
                                    | crate::domain::channel::ChannelType::SavedMessages
                            )
                        }),
                started: Some(false),
                process: Some(PROCESS.clone()),
            },
        )?;
        Ok((key, attempt))
    }

    /// A dead process's claim that never reached encryption: nothing was
    /// submitted. A stored row becomes a retryable failure; otherwise the
    /// composed intent (an attachment upload that died, say) is parked for
    /// the composer, then the claim is dropped. Parking first means a crash
    /// in between leaves a duplicate draft, never a lost one.
    fn recover_abandoned_preparation(
        &self,
        conversation: ActiveConversation,
        key: &str,
        pending: PendingSend,
    ) -> Result<(), SendError> {
        let db = &self.services.database;
        if let Some(id) = pending.message_id.as_ref() {
            db.connection().map_err(storage_error)?
                .execute("UPDATE messages SET delivery_status='failed',failure_reason='Message not submitted' WHERE id=?1 AND delivery_status NOT IN ('sent','delivered','read')", params![id])
                .map_err(|e| storage_error(StorageError::Database(e)))?;
        } else if !pending.text.is_empty() || !pending.attachments.is_empty() {
            let reply = pending.quote.as_ref().and_then(|q| {
                let author = q.get("author")?.as_str()?.to_owned();
                let name = author
                    .parse::<crate::domain::ids::UserId>()
                    .map(|id| self.quote_author_name(&id, || author.clone()))
                    .unwrap_or_else(|_| author.clone());
                Some(super::ComposerReply {
                    sender_id: author,
                    timestamp: q.get("timestamp")?.as_u64()?.to_string(),
                    sender_name: name,
                    preview: q
                        .get("preview")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    thumbnail: None,
                })
            });
            let attachments = pending
                .attachments
                .iter()
                .filter_map(|a| {
                    Some(PendingAttachment {
                        id: a.get("id")?.as_str()?.parse().ok()?,
                        filename: a.get("filename")?.as_str()?.to_owned(),
                        path: a.get("path")?.as_str()?.into(),
                        size: a.get("size")?.as_u64()?,
                        mime_type: a.get("mime_type")?.as_str()?.to_owned(),
                        pointer: a
                            .get("pointer")
                            .and_then(|p| serde_json::from_value::<Vec<u8>>(p.clone()).ok())
                            .and_then(|bytes| {
                                crate::proto::content_v1::AttachmentPointer::decode(
                                    bytes.as_slice(),
                                )
                                .ok()
                            }),
                    })
                })
                .collect();
            // Returns the send only when the composer has room, which an
            // empty-composer claim never asserts here: it is always parked.
            let _ = self.recover_composer_send(
                super::ComposerSend {
                    attempt_id: uuid::Uuid::new_v4(),
                    session_revision: self.auth_state().get().session_revision,
                    conversation,
                    text: pending.text.clone(),
                    reply,
                    attachments,
                },
                false,
            );
        }
        tracing::info!(
            ?conversation,
            "send abandoned before submission by an earlier process; claim released"
        );
        discard_preparation(db, key, &pending.attempt_id)
    }

    pub(super) fn finish_send_attempt(
        &self,
        key: &str,
        attempt: uuid::Uuid,
        message_id: Option<String>,
        confirmed: bool,
    ) -> Result<(), SendError> {
        if confirmed {
            if message_id.is_some() {
                return release(&self.services.database, key, &attempt.to_string());
            }
            return discard_preparation(&self.services.database, key, &attempt.to_string());
        }
        // Only associate the exact original row, never a timestamp/content guess.
        if let Some(mut pending) = read_pending(&self.services.database, key)? {
            if pending.attempt_id == attempt.to_string() {
                pending.message_id = message_id;
                let raw = serde_json::to_string(&pending).map_err(|_| {
                    storage_error(StorageError::InvalidData(
                        "Cannot retain pending row identity".into(),
                    ))
                })?;
                let conn = self.services.database.connection().map_err(storage_error)?;
                conn.execute("UPDATE settings SET value=?2 WHERE key=?1 AND json_extract(value,'$.attempt_id')=?3",params![key,raw,attempt.to_string()]).map_err(|e| storage_error(StorageError::Database(e)))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending() -> PendingSend {
        PendingSend {
            attempt_id: uuid::Uuid::new_v4().to_string(),
            text: "original".into(),
            attachments: vec![],
            quote: None,
            options: None,
            message_id: None,
            original_row: None,
            fresh_post: false,
            started: Some(false),
            process: Some(PROCESS.clone()),
        }
    }
    #[test]
    fn accepted_but_response_lost_blocks_new_identity_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attempt.db");
        let first = pending();
        {
            let db = Database::open(&path, "test-only").unwrap();
            db.run_migrations()
                .expect("migrate throwaway test database");
            claim(&db, "owner:channel", &first).unwrap();
            // Simulate remote accept followed by loss/cancellation of response:
            // the caller must not remove its durable claim.
            assert!(matches!(
                claim(&db, "owner:channel", &pending()),
                Err(SendError::SendInProgress)
            ));
        }
        let db = Database::open(&path, "test-only").unwrap();
        db.run_migrations()
            .expect("migrate throwaway test database");
        let stored = read_pending(&db, "owner:channel").unwrap().unwrap();
        assert_eq!(stored.attempt_id, first.attempt_id);
        assert_eq!(stored.text, "original");
        assert!(matches!(
            claim(&db, "owner:channel", &pending()),
            Err(SendError::SendInProgress)
        ));
        release(&db, "owner:channel", "wrong-attempt").unwrap();
        assert!(read_pending(&db, "owner:channel").unwrap().is_some());
        assert!(matches!(
            claim(&db, "owner:channel", &first),
            Err(SendError::DeliveryUnconfirmed)
        ));
        release(&db, "owner:channel", &first.attempt_id).unwrap();
        assert!(matches!(
            claim(&db, "owner:channel", &first),
            Err(SendError::AttemptAlreadyConfirmed)
        ));
        claim(&db, "owner:channel", &pending()).unwrap();
    }
    #[tokio::test]
    async fn local_refusals_recover_but_same_error_after_partial_send_stays_retained() {
        use crate::services::anon_dm::AnonDmError;
        use crate::services::gateway::submission::{mark_submission_started, track_submission};
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("refusals.db"), "test-only").unwrap();
        db.run_migrations()
            .expect("migrate throwaway test database");
        // A blocked check, an exhausted rate window, invalid content and local
        // preparation/storage failure all precede the real submission marker.
        let errors = vec![
            SendError::AnonDm(AnonDmError::ConversationBlocked(
                crate::domain::anon_dm::AnonDmId::new(),
            )),
            SendError::AnonDm(AnonDmError::RateLimitExceeded(60)),
            SendError::Message(crate::services::message::MessageError::InvalidContent(
                "empty".into(),
            )),
            SendError::Message(crate::services::message::MessageError::InvalidContent(
                "message content too large for sealed delivery".into(),
            )),
            storage_error(StorageError::InvalidData("local preparation failed".into())),
        ];
        for error in errors {
            let original = pending();
            claim(&db, "owner:dm", &original).unwrap();
            let (error, started, row) = track_submission(async { error }, None).await;
            let preserved = error.to_string();
            let error = settle_failure(
                &db,
                "owner:dm",
                original.attempt_id.parse().unwrap(),
                started,
                row,
                error,
            )
            .unwrap();
            assert_eq!(error.to_string(), preserved);
            assert!(read_pending(&db, "owner:dm").unwrap().is_none());
            // Unblock/refill/repair allows the same unsent identity to run.
            claim(&db, "owner:dm", &original).unwrap();
            discard_preparation(&db, "owner:dm", &original.attempt_id).unwrap();
            // A new valid send must also be admitted after local refusal.
            let next = pending();
            claim(&db, "owner:dm", &next).unwrap();
            discard_preparation(&db, "owner:dm", &next.attempt_id).unwrap();
        }
        for error in [
            SendError::NotLoggedIn,
            SendError::Message(crate::services::message::MessageError::InvalidContent(
                "message content too large for sealed delivery".into(),
            )),
            SendError::Message(crate::services::message::MessageError::Network(
                crate::error::NetworkError::InvalidRequest("sealed message too large".into()),
            )),
        ] {
            let original = pending();
            claim(&db, "owner:dm", &original).unwrap();
            let (error, started, row) = track_submission(
                async {
                    mark_submission_started().unwrap(); // first recipient may already have accepted
                    error // same cause is unsafe after encryption or partial fanout
                },
                None,
            )
            .await;
            assert!(matches!(
                settle_failure(
                    &db,
                    "owner:dm",
                    original.attempt_id.parse().unwrap(),
                    started,
                    row,
                    error
                )
                .unwrap(),
                SendError::DeliveryUnconfirmed
            ));
            assert!(matches!(
                claim(&db, "owner:dm", &pending()),
                Err(SendError::SendInProgress)
            ));
            discard_preparation(&db, "owner:dm", &original.attempt_id).unwrap();
        }
    }

    #[tokio::test]
    async fn retry_keeps_exact_original_row_on_lost_response_reopen_and_late_receipt() {
        use crate::services::gateway::submission::{
            bind_original_row, mark_submission_started, track_submission,
        };
        for route in [
            "public-channel",
            "private-channel",
            "sealed-dm",
            "shield-dm",
            "anonymous-group",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("retry.db");
            let db = std::sync::Arc::new(Database::open(&path, "test-only").unwrap());
            db.run_migrations()
                .expect("migrate throwaway test database");
            let mut original = pending();
            let row = MessageId::new().to_string();
            original.message_id = Some(row.clone());
            original.original_row = Some(
                serde_json::json!({"id":row,"body":"exact original","attachments":["original-pointer"]}),
            );
            claim(&db, route, &original).unwrap();
            let (error, started, exact_row) = track_submission(
                async {
                    bind_original_row(row.clone()).unwrap();
                    mark_submission_started().unwrap();
                    // Remote acceptance with no response. No fresh retry is safe.
                    SendError::Message(crate::services::message::MessageError::Network(
                        crate::error::NetworkError::Timeout,
                    ))
                },
                Some((db.clone(), route.into(), original.attempt_id.clone())),
            )
            .await;
            settle_failure(
                &db,
                route,
                original.attempt_id.parse().unwrap(),
                started,
                exact_row,
                error,
            )
            .unwrap();
            drop(db);
            let db = Database::open(&path, "test-only").unwrap();
            db.run_migrations()
                .expect("migrate throwaway test database");
            let retained = read_pending(&db, route).unwrap().unwrap();
            assert_eq!(retained.message_id, original.message_id);
            assert_eq!(retained.original_row, original.original_row);
            assert!(matches!(
                claim(&db, route, &original),
                Err(SendError::DeliveryUnconfirmed)
            ));
            release(&db, route, "unrelated-receipt").unwrap();
            assert!(read_pending(&db, route).unwrap().is_some());
            // The late receipt belongs to the retained operation. Its original
            // identity is tombstoned even after the pending claim is released.
            release(&db, route, &original.attempt_id).unwrap();
            assert!(matches!(
                claim(&db, route, &original),
                Err(SendError::AttemptAlreadyConfirmed)
            ));
        }
    }

    #[tokio::test]
    async fn actor_permit_timeout_remains_durable_and_refusal_does_not_downgrade_ack() {
        use crate::domain::{
            ids::{ChannelId, UserId},
            message::{ConversationType, Message},
        };
        use crate::services::gateway::submission::{SubmissionPermit, track_submission};
        use crate::storage::repositories::UnifiedMessageRepository;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bounded-actor.db");
        let db = std::sync::Arc::new(Database::open(&path, "test-only").unwrap());
        db.run_migrations().unwrap();
        let repo = UnifiedMessageRepository::new(db.clone());
        let mut row = Message::new_text(
            ChannelId::new(),
            ConversationType::ServerDm,
            Some(UserId::new()),
            "receipt won".into(),
            1,
        );
        row.delivery_status = "delivered".into();
        repo.insert_outgoing(&row).unwrap();
        let first = pending();
        claim(&db, "bounded", &first).unwrap();
        settle_failure(
            &db,
            "bounded",
            first.attempt_id.parse().unwrap(),
            false,
            Some(row.id.to_string()),
            SendError::Message(crate::services::message::MessageError::InvalidContent(
                "local cap refusal".into(),
            )),
        )
        .unwrap();
        assert_eq!(
            repo.get(&row.id).unwrap().unwrap().delivery_status,
            "delivered"
        );
        let next = pending();
        claim(&db, "bounded", &next).unwrap();
        let (error, started, _) = track_submission(
            async {
                let permit = SubmissionPermit::capture();
                // Actor runs on a different thread: task locals are not inherited.
                std::thread::spawn(move || permit.start().unwrap())
                    .join()
                    .unwrap();
                SendError::Message(crate::services::message::MessageError::Network(
                    crate::error::NetworkError::Timeout,
                ))
            },
            None,
        )
        .await;
        assert!(started);
        assert!(matches!(
            settle_failure(
                &db,
                "bounded",
                next.attempt_id.parse().unwrap(),
                started,
                Some(row.id.to_string()),
                error
            )
            .unwrap(),
            SendError::DeliveryUnconfirmed
        ));
        assert_eq!(
            repo.get(&row.id).unwrap().unwrap().delivery_status,
            "delivered"
        );
        drop(repo);
        drop(db);
        let db = Database::open(&path, "test-only").unwrap();
        db.run_migrations().unwrap();
        assert_eq!(
            read_pending(&db, "bounded").unwrap().unwrap().attempt_id,
            next.attempt_id
        );
        assert!(matches!(
            claim(&db, "bounded", &pending()),
            Err(SendError::SendInProgress)
        ));
        release(&db, "bounded", &next.attempt_id).unwrap();
        assert!(matches!(
            claim(&db, "bounded", &next),
            Err(SendError::AttemptAlreadyConfirmed)
        ));
    }

    #[test]
    fn only_a_dead_process_claim_that_never_started_is_abandoned() {
        let mut claim = pending();
        assert!(
            !abandoned_before_submission(&claim),
            "this process may still upload"
        );
        claim.process = Some("dead-process".into());
        assert!(abandoned_before_submission(&claim));
        claim.started = Some(true);
        assert!(
            !abandoned_before_submission(&claim),
            "may have reached the server"
        );
        claim.started = None;
        assert!(
            !abandoned_before_submission(&claim),
            "pre-flag journal fails closed"
        );
        let owner = uuid::Uuid::new_v4().to_string();
        let key = pending_key(
            &owner,
            ActiveConversation::ServerDm {
                peer_id: crate::domain::ids::UserId::new(),
            },
        );
        assert_eq!(key_owner(&key), owner);
    }

    #[test]
    fn old_public_channel_receipt_does_not_confirm_the_fresh_retry_post() {
        let mut retry = pending();
        let original = MessageId::new();
        retry.message_id = Some(original.to_string());
        retry.original_row = Some(serde_json::json!({"id":original.to_string()}));
        retry.fresh_post = true;
        assert_eq!(confirmation_row_id(&retry), None);
        let actual_retry = MessageId::new();
        retry.message_id = Some(actual_retry.to_string());
        assert_eq!(confirmation_row_id(&retry), Some(actual_retry));
        retry.fresh_post = false;
        retry.message_id = Some(original.to_string());
        assert_eq!(confirmation_row_id(&retry), Some(original));
    }

    #[test]
    fn concurrent_attempts_have_one_winner_and_other_accounts_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(
            Database::open(&dir.path().join("attempt.db"), "test-only").unwrap(),
        );
        db.run_migrations()
            .expect("migrate throwaway test database");
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let db = db.clone();
                std::thread::spawn(move || claim(&db, "account-A:dm", &pending()).is_ok())
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .map(|t| usize::from(t.join().unwrap()))
                .sum::<usize>(),
            1
        );
        claim(&db, "account-B:dm", &pending()).unwrap();
    }
}

/// A UI deadline is a progress notification, never cancellation of a send.
/// The exact same future continues; no new identity or encryption is started.
pub async fn await_send_with_notice<F, N>(
    send: F,
    deadline: std::time::Duration,
    on_slow: N,
) -> F::Output
where
    F: std::future::Future,
    N: FnOnce(),
{
    tokio::pin!(send);
    match tokio::time::timeout(deadline, &mut send).await {
        Ok(result) => result,
        Err(_) => {
            on_slow();
            send.await
        }
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    #[tokio::test]
    async fn accepted_send_survives_deadline_without_second_encryption_or_deposit() {
        for transport in ["channel", "sealed-dm", "shield-dm", "anon-dm", "anon-group"] {
            let encrypted = Arc::new(AtomicUsize::new(0));
            let deposits = Arc::new(AtomicUsize::new(0));
            let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
            let (receipt_tx, receipt_rx) = tokio::sync::oneshot::channel();
            let (slow_tx, slow_rx) = tokio::sync::oneshot::channel();
            let (e, d) = (encrypted.clone(), deposits.clone());
            let handle = tokio::spawn(await_send_with_notice(
                async move {
                    e.fetch_add(1, Ordering::SeqCst);
                    d.fetch_add(1, Ordering::SeqCst);
                    accepted_tx.send(()).unwrap(); // remote accepted; no receipt yet
                    receipt_rx.await.unwrap()
                },
                std::time::Duration::from_millis(1),
                move || {
                    slow_tx.send(()).unwrap();
                },
            ));
            accepted_rx.await.unwrap();
            slow_rx.await.unwrap();
            assert!(
                !handle.is_finished(),
                "{transport}: deadline canceled operation"
            );
            receipt_tx.send("original-message").unwrap();
            assert_eq!(handle.await.unwrap(), "original-message");
            assert_eq!(encrypted.load(Ordering::SeqCst), 1, "{transport}");
            assert_eq!(deposits.load(Ordering::SeqCst), 1, "{transport}");
        }
    }
}
