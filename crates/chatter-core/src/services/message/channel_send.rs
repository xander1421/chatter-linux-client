//! Durable automatic recovery for channel text that has not reached SendMessage.
//! Prepared requests retain their ciphertext and UUID across lost responses.
//! Replay requires the companion server transactionally deduplicating that UUID.
use super::{MessageError, MessageService};
use crate::domain::{
    ids::{ChannelId, IntegrationId, MessageId, UserId},
    message::{ConversationType, Message},
};
use crate::error::{NetworkError, StorageError};
use crate::proto::chat_v1::SendMessageRequest;
use crate::services::{current_user::CurrentUserProvider, gateway::QuoteIntent};
use crate::storage::database::Database;
use crate::storage::repositories::settings_repo::keys;
use prost::Message as _;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const PREFIX: &str = "send.channel.predeposit.v1:";
const LEASE_SECONDS: i64 = 60;
/// Live journals per account (queued through deleting). A full queue refuses
/// a new compose before it is bound, so the composer keeps the draft.
const MAX_PENDING: i64 = 64;
/// Phases that still hold delivery or deletion work.
const LIVE: &str = "('queued','preparing','submitted','accepted','deleting')";
/// A prepared request may be replayed only below sealed-sender freshness.
fn replayable(prepared: &Prepared) -> bool {
    // Tolerate a small backward clock step instead of expiring at once.
    let age = now() - prepared.at;
    (-300..23 * 60 * 60).contains(&age)
}
#[derive(Serialize, Deserialize)]
struct Quote {
    author: UserId,
    timestamp: u64,
    preview: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Entry {
    device: u32,
    row: Message,
    quote: Option<Quote>,
}
#[derive(Serialize, Deserialize)]
struct Prepared {
    request: Vec<u8>,
    deletion_token: Option<[u8; 32]>,
    state_hash: Option<Vec<u8>>,
    at: i64,
}
struct Deposit {
    id: MessageId,
    db: Arc<Database>,
    key: String,
    generation: String,
}
tokio::task_local! { static CHANNEL_DEPOSIT: Deposit; }
fn key(owner: UserId, id: MessageId) -> String {
    format!("{PREFIX}{owner}:{id}")
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn invalid(text: &str) -> StorageError {
    StorageError::InvalidData(text.into())
}

pub(crate) fn owns(db: &Database, owner: &str, id: &str) -> Result<bool, StorageError> {
    db.read_connection()?.query_row("SELECT EXISTS(SELECT 1 FROM settings WHERE key=?1 AND json_extract(value,'$.phase') IN ('queued','preparing','submitted','accepted'))",params![format!("{PREFIX}{owner}:{id}")],|r|r.get(0)).map_err(Into::into)
}

/// Release only the claims bound to this send's own row identities: the
/// parked UUID (`bind_original_row`, a manual retry) or the server id the
/// deposit rebound it to. Both are unique to this message, so a claim for any
/// other compose in the conversation is neither released nor rewritten. Each
/// released attempt is remembered as confirmed, so it cannot be replayed.
fn settle_admission(
    conn: &rusqlite::Connection,
    owner: UserId,
    ids: &[MessageId],
) -> Result<(), StorageError> {
    let pattern = format!("send.unresolved.v1:{owner}:%");
    for id in ids {
        conn.execute("INSERT OR IGNORE INTO settings(key,value,updated_at) SELECT key || ':confirmed:' || json_extract(value,'$.attempt_id'),json_object('attempt_id',json_extract(value,'$.attempt_id'),'message_id',json_extract(value,'$.message_id')),updated_at FROM settings WHERE key LIKE ?1 AND key NOT LIKE '%:confirmed:%' AND json_extract(value,'$.message_id')=?2 AND json_extract(value,'$.attempt_id') IS NOT NULL",params![pattern,id.to_string()])?;
        conn.execute("DELETE FROM settings WHERE key LIKE ?1 AND key NOT LIKE '%:confirmed:%' AND json_extract(value,'$.message_id')=?2 AND json_extract(value,'$.attempt_id') IS NOT NULL",params![pattern,id.to_string()])?;
    }
    Ok(())
}

/// Atomically persist the exact request BEFORE entering the RPC future. A
/// suspended preparer cannot deposit after recovery claimed its generation.
pub(super) fn retain_channel_request(
    mut request: SendMessageRequest,
    token: Option<&[u8; 32]>,
    state_hash: Option<Vec<u8>>,
) -> Result<SendMessageRequest, StorageError> {
    CHANNEL_DEPOSIT.try_with(|deposit| {
        request.client_request_id = deposit.id.to_string();
        let prepared = Prepared {request:request.encode_to_vec(),deletion_token:token.copied(),state_hash,at:now()};
        let raw = serde_json::to_string(&prepared).map_err(|_|invalid("Cannot retain channel request"))?;
        let conn = deposit.db.connection()?;
        let changed = conn.execute("UPDATE settings SET value=json_set(value,'$.phase','submitted','$.prepared',json(?3)) WHERE key=?1 AND json_extract(value,'$.generation')=?2 AND json_extract(value,'$.phase')='preparing' AND EXISTS(SELECT 1 FROM messages WHERE id=json_extract(settings.value,'$.entry.row.id') AND is_deleted=0) AND NOT EXISTS(SELECT 1 FROM deleted_messages WHERE id=json_extract(settings.value,'$.entry.row.id'))", params![deposit.key, deposit.generation,raw])?;
        if changed != 1 { return Err(invalid("Channel send lease expired or message deleted")); }
        Ok(request.clone())
    }).unwrap_or_else(|_|Ok(request))
}

pub(super) fn has_channel_queue() -> bool {
    CHANNEL_DEPOSIT.try_with(|_| ()).is_ok()
}
pub(super) fn retain_channel_response(
    proto: &crate::proto::chat_v1::Message,
) -> Result<(), StorageError> {
    CHANNEL_DEPOSIT.try_with(|deposit| {
        let raw = serde_json::to_string(&proto.encode_to_vec()).map_err(|_|invalid("Cannot retain channel response"))?;
        let changed = deposit.db.connection()?.execute("UPDATE settings SET value=json_set(value,'$.phase','accepted','$.response',json(?3),'$.next_at',0) WHERE key=?1 AND json_extract(value,'$.generation')=?2",params![deposit.key,deposit.generation,raw])?;
        if changed != 1 { return Err(invalid("Channel response lease expired")); }
        Ok(())
    }).unwrap_or(Ok(()))
}

impl MessageService {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn send_durable_channel_text(
        &self,
        channel: &ChannelId,
        text: &str,
        thread: Option<&MessageId>,
        mentions: Vec<IntegrationId>,
        targets_ai_bot: bool,
        quote: Option<&QuoteIntent>,
    ) -> Result<Message, MessageError> {
        if text.trim().is_empty() {
            return Err(MessageError::InvalidContent(
                "message must have text".into(),
            ));
        }
        crate::services::gateway::submission::check_send_session()?;
        let owner = self.current_user_id()?;
        let mut row = Message::new_text(
            *channel,
            ConversationType::Channel,
            Some(owner),
            text.trim().into(),
            crate::util::time::TimestampNanos::now().as_u64(),
        );
        row.thread_id = thread.copied();
        row.mentioned_integrations = mentions;
        row.targets_ai_bot = targets_ai_bot;
        row.delivery_status = "sending".into();
        row.is_read = true;
        let entry = Entry {
            device: self
                .settings_repo
                .get::<u32>(keys::CURRENT_DEVICE_ID)?
                .ok_or(MessageError::NotLoggedIn)?,
            row,
            quote: quote.map(|q| Quote {
                author: q.target_author_id,
                timestamp: q.target_timestamp,
                preview: q.target_preview.clone(),
            }),
        };
        let journal_key = key(owner, entry.row.id);
        let generation = uuid::Uuid::new_v4().to_string();
        let raw = serde_json::json!({"entry":entry,"phase":"preparing","generation":generation,"attempts":0,"next_at":now()+LEASE_SECONDS}).to_string();
        // The intent is durable before the first await, including before local
        // row creation. Recovery can materialize it via tombstone-aware insert.
        let inserted = self.database.connection()?.execute(
            &format!("INSERT INTO settings(key,value,updated_at) SELECT ?1,?2,?3 WHERE (SELECT COUNT(*) FROM settings WHERE key LIKE ?4 AND json_extract(value,'$.phase') IN {LIVE})<?5"),
            params![journal_key, raw, chrono::Utc::now().to_rfc3339(), format!("{PREFIX}{owner}:%"), MAX_PENDING],
        )?;
        if inserted != 1 {
            return Err(MessageError::InvalidContent(
                "Too many channel messages are waiting to send".into(),
            ));
        }
        crate::services::gateway::submission::bind_original_row(entry.row.id.to_string())?;
        // A durable enqueue is itself a submission boundary: generic admission
        // must retain this composer attempt while the queue owns its delivery.
        crate::services::gateway::submission::mark_submission_started()?;
        self.message_repo.insert_outgoing(&entry.row)?;
        self.attempt_channel_entry(&journal_key, &generation, &entry)
            .await
    }

    pub(crate) fn terminal_channel_send(&self, row: &Message) -> Result<bool, StorageError> {
        if row.conversation_type != ConversationType::Channel || row.delivery_status != "failed" {
            return Ok(false);
        }
        let Some(owner) = row.sender_id else {
            return Ok(false);
        };
        self.database.read_connection()?.query_row("SELECT EXISTS(SELECT 1 FROM settings WHERE key=?1 AND json_extract(value,'$.phase')='terminal')",params![key(owner,row.id)],|r|r.get(0)).map_err(Into::into)
    }

    /// Only the old explicit pre-submission marker proves a legacy failed row
    /// was never deposited. NULL/generic failures must not be guessed safe.
    fn adopt_proven_unsent_channels(&self, owner: UserId) -> Result<(), MessageError> {
        let ids: Vec<String> = {
            let conn = self.database.read_connection()?;
            let mut stmt = conn.prepare("SELECT id FROM messages WHERE sender_id=?1 AND conversation_type='channel' AND delivery_status='failed' AND failure_reason='Message not submitted' AND is_deleted=0 AND NOT EXISTS(SELECT 1 FROM settings WHERE key=?2 || messages.id) ORDER BY sent_at LIMIT 4")?;
            let mapped = stmt.query_map(
                params![owner.to_string(), format!("{PREFIX}{owner}:")],
                |r| r.get(0),
            )?;
            mapped.collect::<Result<_, _>>()?
        };
        for id in ids {
            let id = id
                .parse()
                .map_err(|_| invalid("Invalid legacy channel row identity"))?;
            self.adopt_unsent_channel(owner, &id)?;
        }
        Ok(())
    }

    /// Queue one proven-unsent legacy text row under its own UUID. The journal
    /// insert re-checks the marker in the same statement, and manual retry
    /// clears the marker only while no journal exists, so exactly one of the
    /// two owns the row: the queue (stable request id) or a manual send.
    fn adopt_unsent_channel(&self, owner: UserId, id: &MessageId) -> Result<bool, MessageError> {
        let Some(device) = self.settings_repo.get::<u32>(keys::CURRENT_DEVICE_ID)? else {
            return Ok(false);
        };
        let Some(mut row) = self.message_repo.get(id)? else {
            return Ok(false);
        };
        if row.sender_id != Some(owner)
            || row.conversation_type != ConversationType::Channel
            || !row.attachments.is_empty()
            || row.as_text().is_none()
        {
            return Ok(false);
        }
        if !self
            .channel_repo
            .get(&row.conversation_id)?
            .is_some_and(|channel| {
                matches!(
                    channel.channel_type,
                    crate::domain::channel::ChannelType::Public
                        | crate::domain::channel::ChannelType::Private
                )
            })
        {
            return Ok(false);
        }
        let quote = if let Some(author) = row.reply_to_author_id {
            let Some(parent) = row
                .reply_to
                .as_ref()
                .map(|id| self.message_repo.get(id))
                .transpose()?
                .flatten()
            else {
                return Ok(false);
            };
            Some(Quote {
                author,
                timestamp: parent.sent_at,
                preview: row.reply_to_text.clone(),
            })
        } else {
            None
        };
        row.delivery_status = "sending".into();
        row.failure_reason = Some("Waiting for connection".into());
        let entry = Entry { device, row, quote };
        let raw = serde_json::json!({"entry":entry,"phase":"queued","generation":uuid::Uuid::new_v4().to_string(),"attempts":0,"next_at":0}).to_string();
        let adopted = self.database.connection()?.execute(
            &format!("INSERT OR IGNORE INTO settings(key,value,updated_at) SELECT ?1,?2,?3 WHERE EXISTS(SELECT 1 FROM messages WHERE id=?4 AND delivery_status='failed' AND failure_reason='Message not submitted' AND is_deleted=0) AND NOT EXISTS(SELECT 1 FROM deleted_messages WHERE id=?4) AND (SELECT COUNT(*) FROM settings WHERE key LIKE ?5 AND json_extract(value,'$.phase') IN {LIVE})<?6"),
            params![
                key(owner, entry.row.id),
                raw,
                chrono::Utc::now().to_rfc3339(),
                entry.row.id.to_string(),
                format!("{PREFIX}{owner}:%"),
                MAX_PENDING
            ],
        )? == 1;
        if adopted {
            self.finish_retry_row(&entry.row.id, "sending", Some("Waiting for connection"))?;
        }
        Ok(adopted)
    }

    /// Manual retry of a queued/ambiguous identity never creates a fresh post.
    /// Terminal refusals cannot fall through to a fresh post or new ciphertext.
    pub(super) fn durable_channel_retry(
        &self,
        row: &Message,
    ) -> Result<Option<Message>, MessageError> {
        let owner = self.current_user_id()?;
        let journal_key = key(owner, row.id);
        let phase = |conn: &rusqlite::Connection| -> Result<Option<String>, StorageError> {
            conn.query_row(
                "SELECT json_extract(value,'$.phase') FROM settings WHERE key=?1",
                params![journal_key],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
        };
        let mut current = phase(&*self.database.read_connection()?)?;
        if current.is_none() {
            // A proven-unsent legacy row is replayed by the queue under its own
            // UUID. Anything else falls through to the manual send, which
            // first fences adoption by dropping the marker while no journal
            // exists; a racing adoption that won is then read back here.
            if !self.adopt_unsent_channel(owner, &row.id)? {
                self.database.connection()?.execute("UPDATE messages SET failure_reason=NULL WHERE id=?1 AND failure_reason='Message not submitted' AND NOT EXISTS(SELECT 1 FROM settings WHERE key=?2)",params![row.id.to_string(),journal_key])?;
            }
            current = phase(&*self.database.read_connection()?)?;
        }
        match current.as_deref() {
            Some("queued" | "preparing") => {
                // The queue owns this row's exact request, so the manual
                // retry's own claim (bound to this row id) is settled here
                // rather than holding the conversation until delivery.
                let conn = self.database.connection()?;
                conn.execute("UPDATE settings SET value=json_set(value,'$.next_at',0) WHERE key=?1 AND json_extract(value,'$.phase')='queued'",params![journal_key])?;
                settle_admission(&conn, owner, &[row.id])?;
                drop(conn);
                Ok(self
                    .message_repo
                    .get(&row.id)?
                    .or_else(|| Some(row.clone())))
            }
            Some("submitted" | "accepted") => Err(MessageError::InvalidContent(
                "Channel delivery remains unconfirmed; refusing a duplicate post".into(),
            )),
            Some("terminal") => Err(MessageError::InvalidContent(
                "Channel send was refused; automatic recovery stopped".into(),
            )),
            None => Ok(None),
            Some(_) => Err(MessageError::InvalidContent(
                "message cannot be retried".into(),
            )),
        }
    }

    async fn replay_channel_request(
        &self,
        journal_key: &str,
        generation: &str,
        entry: &Entry,
        prepared: &Prepared,
    ) -> Result<Message, MessageError> {
        // Stop comfortably before sealed envelope freshness (24h) and server
        // dedupe retention (7d). Never manufacture a fresh request after expiry.
        if !replayable(prepared) {
            return Err(MessageError::InvalidContent(
                "Queued channel request expired".into(),
            ));
        }
        let owner = self.current_user_id()?;
        let row = &entry.row;
        let members = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.channel_client()
                .get_all_channel_members(&row.conversation_id.to_string()),
        )
        .await
        .unwrap_or(Err(NetworkError::Timeout))?;
        if !members
            .iter()
            .any(|member| member.user_id == owner.to_string())
        {
            return Err(MessageError::NotAuthor);
        }
        if let Some(hash) = prepared.state_hash.as_ref() {
            self.sync_channel_state_if_behind(&row.conversation_id);
            let verified = self.group_gate().for_distribution(&row.conversation_id)?;
            if verified.state_hash.as_slice() != hash.as_slice()
                || !self.group_gate().admits(&row.conversation_id, &owner)
            {
                return Err(MessageError::InvalidContent(
                    "Channel membership changed since request preparation".into(),
                ));
            }
        }
        let request = SendMessageRequest::decode(prepared.request.as_slice())
            .map_err(|_| invalid("Invalid retained channel request protobuf"))?;
        if request.client_request_id != row.id.to_string()
            || request.channel_id != row.conversation_id.to_string()
        {
            return Err(MessageError::InvalidContent(
                "Queued channel identity mismatch".into(),
            ));
        }
        crate::services::gateway::submission::check_send_session()?;
        if row.sender_id != Some(self.current_user_id()?)
            || self.settings_repo.get::<u32>(keys::CURRENT_DEVICE_ID)? != Some(entry.device)
        {
            return Err(MessageError::NotLoggedIn);
        }
        let changed = self.database.connection()?.execute("UPDATE settings SET value=json_set(value,'$.phase','submitted') WHERE key=?1 AND json_extract(value,'$.generation')=?2 AND json_extract(value,'$.phase')='preparing' AND EXISTS(SELECT 1 FROM messages WHERE id=?3 AND is_deleted=0) AND NOT EXISTS(SELECT 1 FROM deleted_messages WHERE id=?3)",params![journal_key,generation,row.id.to_string()])?;
        if changed != 1 {
            return Err(invalid("Channel send lease expired or message deleted").into());
        }
        crate::services::gateway::submission::mark_submission_started()?;
        let proto = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.message_client().send_prepared_message(request),
        )
        .await
        .unwrap_or(Err(NetworkError::Timeout))?;
        retain_channel_response(&proto)?;
        self.materialize_channel_response(entry, prepared, &proto)
    }

    fn materialize_channel_response(
        &self,
        entry: &Entry,
        prepared: &Prepared,
        proto: &crate::proto::chat_v1::Message,
    ) -> Result<Message, MessageError> {
        let row = &entry.row;
        let owner = row.sender_id.ok_or(MessageError::NotLoggedIn)?;
        let encrypted = prepared.state_hash.is_some();
        let mut sent = super::conversion::proto_to_outgoing_channel_message(
            proto,
            owner,
            ConversationType::Channel,
            row.sent_at,
            encrypted,
        )?;
        if sent.conversation_id != row.conversation_id {
            return Err(MessageError::InvalidContent(
                "Channel response identity mismatch".into(),
            ));
        }
        sent.content = row.content.clone();
        // Caller stores this only after removing the parked dedupe identity.

        Ok(sent)
    }

    async fn attempt_channel_entry(
        &self,
        journal_key: &str,
        generation: &str,
        entry: &Entry,
    ) -> Result<Message, MessageError> {
        let row = &entry.row;
        let quote = entry.quote.as_ref().map(|q| QuoteIntent {
            target_author_id: q.author,
            target_timestamp: q.timestamp,
            target_preview: q.preview.clone(),
        });
        let deposit = Deposit {
            id: row.id,
            db: self.database.clone(),
            key: journal_key.into(),
            generation: generation.into(),
        };
        let result = CHANNEL_DEPOSIT
            .scope(deposit, async {
                crate::services::gateway::submission::check_send_session()?;
                if row.sender_id != Some(self.current_user_id()?) {
                    return Err(MessageError::NotLoggedIn);
                }
                let accepted: Option<(String,String)> = self.database.read_connection()?.query_row(
                    "SELECT json_extract(value,'$.response'),json_extract(value,'$.prepared') FROM settings WHERE key=?1 AND json_extract(value,'$.response') IS NOT NULL",
                    params![journal_key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                if let Some((response,prepared)) = accepted {
                    let bytes: Vec<u8> = serde_json::from_str(&response).map_err(|_|invalid("Invalid accepted response"))?;
                    let proto = crate::proto::chat_v1::Message::decode(bytes.as_slice()).map_err(|_|invalid("Invalid accepted response protobuf"))?;
                    let prepared: Prepared = serde_json::from_str(&prepared).map_err(|_|invalid("Invalid accepted request"))?;
                    return self.materialize_channel_response(entry,&prepared,&proto);
                }
                let stored = self
                    .message_repo
                    .get(&row.id)?
                    .ok_or(MessageError::MessageNotFound(row.id))?;
                if stored.is_deleted {
                    return Err(MessageError::MessageNotFound(row.id));
                }
                if self.settings_repo.get::<u32>(keys::CURRENT_DEVICE_ID)? != Some(entry.device) {
                    return Err(MessageError::InvalidContent("Sending device changed".into()));
                }
                let prepared: Option<String> = self.database.read_connection()?.query_row(
                    "SELECT json_extract(value,'$.prepared') FROM settings WHERE key=?1 AND json_extract(value,'$.generation')=?2",
                    params![journal_key,generation], |r|r.get(0)).optional()?.flatten();
                if let Some(raw) = prepared {
                    let prepared: Prepared = serde_json::from_str(&raw).map_err(|_|invalid("Invalid prepared channel request"))?;
                    return self.replay_channel_request(journal_key,generation,entry,&prepared).await;
                }
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    self.send_message(
                        &mut self.message_client(),
                        &mut self.auth_client(),
                        &mut self.channel_client(),
                        None,
                        &row.conversation_id,
                        row.as_text().unwrap_or(""),
                        vec![],
                        vec![],
                        row.thread_id.as_ref(),
                        row.mentioned_integrations.clone(),
                        row.targets_ai_bot,
                        quote.as_ref(),
                        row.sent_at,
                    ),
                )
                .await
                .unwrap_or(Err(MessageError::Network(NetworkError::Timeout)))
            })
            .await;
        match result {
            Ok(mut sent) => {
                crate::services::gateway::submission::check_send_session()?;
                {
                    let mut conn = self.database.connection()?;
                    let tx = conn.transaction()?;
                    let deleted: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deleted_messages WHERE id IN (?1,?2)) OR EXISTS(SELECT 1 FROM messages WHERE id IN (?1,?2) AND is_deleted=1)",params![row.id.to_string(),sent.id.to_string()],|r|r.get(0))?;
                    if deleted {
                        // Deleted while the post was accepted. Keep the request,
                        // token and response for `finish_channel_deletion`, and
                        // suppress the canonical id so catch-up cannot restore
                        // it. Only this generation may do so: a stale future
                        // finishing after the confirm (journal gone, parked id
                        // tombstoned by it) must not touch the server copy.
                        let fenced = tx.execute("UPDATE settings SET value=json_set(value,'$.phase','deleting','$.next_at',0) WHERE key=?1 AND json_extract(value,'$.generation')=?2",params![journal_key,generation])?;
                        if fenced == 1 {
                            settle_admission(
                                &tx,
                                row.sender_id.ok_or(MessageError::NotLoggedIn)?,
                                &[row.id, sent.id],
                            )?;
                            for id in [row.id, sent.id] {
                                tx.execute(
                                    "INSERT OR IGNORE INTO deleted_messages(id) VALUES (?1)",
                                    params![id.to_string()],
                                )?;
                                tx.execute(
                                    "DELETE FROM messages WHERE id=?1",
                                    params![id.to_string()],
                                )?;
                            }
                        }
                        tx.commit()?;
                        return Err(MessageError::MessageNotFound(row.id));
                    }
                    if sent.id != row.id {
                        // Preserve receipt evidence across the placeholder swap;
                        // its FK otherwise cascades when the parked row is removed.
                        tx.execute("UPDATE message_receipt_targets SET message_id=NULL WHERE message_id=?1",params![row.id.to_string()])?;
                        tx.execute(
                            "DELETE FROM messages WHERE id=?1",
                            params![row.id.to_string()],
                        )?;
                    }
                    tx.commit()?;
                }
                crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
                    self.database.clone(),
                )
                .bind_send_timestamp(
                    row.conversation_id,
                    ConversationType::Channel,
                    row.sender_id.ok_or(MessageError::NotLoggedIn)?,
                    row.sent_at,
                    sent.sent_at,
                )?;
                self.message_repo.insert_or_ignore(&sent)?;
                let prepared: Option<String> = self
                    .database
                    .read_connection()?
                    .query_row(
                        "SELECT json_extract(value,'$.prepared') FROM settings WHERE key=?1",
                        params![journal_key],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                if let Some(raw) = prepared {
                    let prepared: Prepared = serde_json::from_str(&raw)
                        .map_err(|_| invalid("Invalid accepted channel request"))?;
                    if let Some(token) = prepared.deletion_token.as_ref() {
                        self.message_repo.set_deletion_token(&sent.id, token)?;
                    }
                    let request = SendMessageRequest::decode(prepared.request.as_slice())
                        .map_err(|_| invalid("Invalid accepted channel request protobuf"))?;
                    if let Some(ttl) = request
                        .ephemeral_ttl
                        .as_ref()
                        .and_then(|ttl| u32::try_from(ttl.seconds).ok())
                        .filter(|ttl| *ttl > 0)
                    {
                        let _ = self.ephemeral_service.track_armed(sent.id, ttl);
                    }
                }
                if let Some(quote) = quote.as_ref() {
                    sent = self
                        .persist_outgoing_reply_context(sent, quote)
                        .map_err(|(_, e)| e)?;
                }
                // Confirm the exact generation, settle this send's own admission
                // claims, mark the canonical row sent and remove the parked
                // bubble atomically. The journal (ciphertext, token, response)
                // is dropped: the token now lives on the canonical row. A
                // crash cannot strand admission after recovery succeeded, and
                // a lost generation leaves everything to its new owner.
                {
                    let mut conn = self.database.connection()?;
                    let tx = conn.transaction()?;
                    let changed = tx.execute("DELETE FROM settings WHERE key=?1 AND json_extract(value,'$.generation')=?2",params![journal_key,generation])?;
                    if changed != 0 {
                        settle_admission(
                            &tx,
                            row.sender_id.ok_or(MessageError::NotLoggedIn)?,
                            &[row.id, sent.id],
                        )?;
                        tx.execute("UPDATE messages SET delivery_status='sent',failure_reason=NULL WHERE id=?1 AND delivery_status NOT IN ('delivered','read')",params![sent.id.to_string()])?;
                        if sent.id != row.id {
                            tx.execute(
                                "INSERT OR IGNORE INTO deleted_messages(id) VALUES (?1)",
                                params![row.id.to_string()],
                            )?;
                            tx.execute(
                                "DELETE FROM messages WHERE id=?1",
                                params![row.id.to_string()],
                            )?;
                        }
                    }
                    tx.commit()?;
                }
                if let Some(stored) = self.message_repo.get(&sent.id)? {
                    sent.delivery_status = stored.delivery_status;
                }
                Ok(sent)
            }
            Err(error) => {
                let transient = crate::services::gateway::submission::check_send_session().is_err()
                    || matches!(&error, MessageError::Network(e) if e.is_retryable() || e.is_transport_error());
                let changed = {
                    let conn = self.database.connection()?;
                    // Transient failures resume preparation or replay the exact
                    // request. Auth, invalid payload, membership and expiry stop.
                    conn.execute("UPDATE settings SET value=json_set(value,'$.phase',CASE WHEN ?3 THEN 'queued' ELSE 'terminal' END,'$.attempts',MIN(json_extract(value,'$.attempts')+1,6),'$.next_at',?4+MIN(300,5*(1 << MIN(json_extract(value,'$.attempts'),6)))) WHERE key=?1 AND json_extract(value,'$.generation')=?2",params![journal_key,generation,transient,now()])?
                };
                if changed == 0 {
                    return self
                        .message_repo
                        .get(&row.id)?
                        .ok_or(MessageError::MessageNotFound(row.id));
                }
                let phase: String = self.database.read_connection()?.query_row(
                    "SELECT json_extract(value,'$.phase') FROM settings WHERE key=?1",
                    params![journal_key],
                    |r| r.get(0),
                )?;
                let (status, reason) = match phase.as_str() {
                    "queued" => ("sending", Some("Waiting for connection")),
                    "submitted" => ("sending", Some("Delivery unconfirmed")),
                    _ => ("failed", error.permanent_send_failure_reason()),
                };
                if phase == "terminal" {
                    let mut conn = self.database.connection()?;
                    let tx = conn.transaction()?;
                    settle_admission(
                        &tx,
                        row.sender_id.ok_or(MessageError::NotLoggedIn)?,
                        &[row.id],
                    )?;
                    // Nothing ever reached the server: keep only the marker
                    // that refuses a fresh post on manual retry. A terminal
                    // journal holding a request or response may have been
                    // deposited, so its token stays for a later deletion.
                    tx.execute("UPDATE settings SET value=json_object('phase','terminal','row_id',json_extract(value,'$.entry.row.id')) WHERE key=?1 AND json_extract(value,'$.phase')='terminal' AND json_extract(value,'$.prepared') IS NULL AND json_extract(value,'$.response') IS NULL",params![journal_key])?;
                    tx.commit()?;
                }
                tracing::warn!(%error, message_id=%row.id, %phase, "Channel send retained");
                self.finish_retry_row(&row.id, status, reason)
            }
        }
    }
}

impl MessageService {
    /// The parked row was deleted after its request may have reached the
    /// server (a retained request or response). Fail closed: learn the
    /// canonical id from the stored response or an exact replay (the server
    /// returns the original for the same bytes), suppress it locally and
    /// remove the server copy with the retained token. The journal, and with
    /// it the only deletion authority for a sealed post, is dropped only
    /// after the server confirms the copy is gone or was never kept.
    async fn finish_channel_deletion(
        &self,
        journal_key: &str,
        generation: &str,
        entry: &Entry,
    ) -> Result<(), MessageError> {
        let row = &entry.row;
        let held: Option<(Option<String>, Option<String>)> = self.database.read_connection()?.query_row(
            "SELECT json_extract(value,'$.response'),json_extract(value,'$.prepared') FROM settings WHERE key=?1 AND json_extract(value,'$.generation')=?2 AND json_extract(value,'$.phase')='deleting'",
            params![journal_key, generation], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let Some((response, prepared)) = held else {
            return Ok(());
        };
        let finish = |phase: Option<&str>| -> Result<(), MessageError> {
            let conn = self.database.connection()?;
            match phase {
                // Unresolvable here: keep request and token, stop retrying.
                Some(phase) => conn.execute("UPDATE settings SET value=json_set(value,'$.phase',?3) WHERE key=?1 AND json_extract(value,'$.generation')=?2",params![journal_key,generation,phase])?,
                None => conn.execute("DELETE FROM settings WHERE key=?1 AND json_extract(value,'$.generation')=?2",params![journal_key,generation])?,
            };
            Ok(())
        };
        let prepared: Option<Prepared> = prepared
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(|_| invalid("Invalid prepared channel request"))?;
        let proto = match response {
            Some(raw) => {
                let bytes: Vec<u8> =
                    serde_json::from_str(&raw).map_err(|_| invalid("Invalid accepted response"))?;
                crate::proto::chat_v1::Message::decode(bytes.as_slice())
                    .map_err(|_| invalid("Invalid accepted response protobuf"))?
            }
            None => {
                let Some(prepared) = prepared.as_ref() else {
                    return finish(None); // never deposited
                };
                let request = SendMessageRequest::decode(prepared.request.as_slice())
                    .map_err(|_| invalid("Invalid retained channel request protobuf"))?;
                crate::services::gateway::submission::check_send_session()?;
                if !replayable(prepared)
                    || request.client_request_id != row.id.to_string()
                    || row.sender_id != Some(self.current_user_id()?)
                    || self.settings_repo.get::<u32>(keys::CURRENT_DEVICE_ID)? != Some(entry.device)
                {
                    tracing::warn!(message_id=%row.id, "Deleted channel send may hold an undeletable server copy; token retained");
                    return finish(Some("unresolved"));
                }
                match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    self.message_client().send_prepared_message(request),
                )
                .await
                .unwrap_or(Err(NetworkError::Timeout))
                {
                    Ok(proto) => {
                        let raw = serde_json::to_string(&proto.encode_to_vec())
                            .map_err(|_| invalid("Cannot retain channel response"))?;
                        self.database.connection()?.execute("UPDATE settings SET value=json_set(value,'$.response',json(?3)) WHERE key=?1 AND json_extract(value,'$.generation')=?2",params![journal_key,generation,raw])?;
                        proto
                    }
                    // AlreadyExists / FailedPrecondition: the server keeps no
                    // live copy under these exact bytes (`network_error.rs`).
                    Err(NetworkError::InvalidRequest(_)) => return finish(None),
                    Err(e) if e.is_retryable() || e.is_transport_error() => return Err(e.into()),
                    // PermissionDenied and the like: the server re-authorizes
                    // before answering a duplicate, so a copy may still exist.
                    Err(e) => {
                        tracing::warn!(error=%e, message_id=%row.id, "Deleted channel send unresolved; token retained");
                        return finish(Some("unresolved"));
                    }
                }
            }
        };
        let server_id: MessageId = proto
            .id
            .parse()
            .map_err(|_| invalid("Invalid accepted message id"))?;
        {
            let mut conn = self.database.connection()?;
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO deleted_messages(id) VALUES (?1)",
                params![server_id.to_string()],
            )?;
            tx.execute(
                "DELETE FROM messages WHERE id=?1",
                params![server_id.to_string()],
            )?;
            tx.commit()?;
        }
        let token = prepared
            .and_then(|p| p.deletion_token)
            .map(|t| t.to_vec())
            .unwrap_or_default();
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.message_client().delete_message(
                &proto.id,
                &row.conversation_id.to_string(),
                token,
            ),
        )
        .await
        .unwrap_or(Err(NetworkError::Timeout))
        {
            Ok(()) | Err(NetworkError::NotFound(_)) => finish(None),
            Err(e) if e.is_retryable() || e.is_transport_error() => Err(e.into()),
            Err(e) => {
                tracing::warn!(error=%e, message_id=%server_id, "Server refused deleting a cancelled channel post; token retained");
                finish(Some("unresolved"))
            }
        }
    }

    async fn delete_channel_entry(&self, journal_key: &str, generation: &str, entry: &Entry) {
        if let Err(error) = self
            .finish_channel_deletion(journal_key, generation, entry)
            .await
        {
            tracing::warn!(%error, message_id=%entry.row.id, "Channel deletion retained");
            if let Ok(conn) = self.database.connection() {
                let _ = conn.execute("UPDATE settings SET value=json_set(value,'$.attempts',MIN(json_extract(value,'$.attempts')+1,6),'$.next_at',?3+MIN(300,5*(1 << MIN(json_extract(value,'$.attempts'),6)))) WHERE key=?1 AND json_extract(value,'$.generation')=?2",params![journal_key,generation,now()]);
            }
        }
    }
}

/// One bounded, session-scoped worker. Leases survive restart; a new generation
/// fences a suspended old future at the actual deposit boundary.
pub(crate) fn spawn_channel_send_recovery(
    client: &Arc<crate::client::ChatterClient>,
) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(client);
    let mut connectivity = client.offline_state().subscribe();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous_session = None;
        loop {
            ticker.tick().await;
            let Some(client) = weak.upgrade() else {
                break;
            };
            let state = client.auth_state().get();
            let Some(owner) = state
                .current_user_id
                .filter(|_| client.offline_state().connection_state().is_connected())
            else {
                continue;
            };
            let mut reconnected = false;
            loop {
                match connectivity.try_recv() {
                    Ok(state) => reconnected |= state.is_online,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                        reconnected = true
                    }
                    Err(_) => break,
                }
            }
            reconnected |= previous_session != Some((owner, state.session_revision));
            previous_session = Some((owner, state.session_revision));
            let messages = client.messages();
            let session =
                crate::services::gateway::submission::SendSession::capture(client.auth_state());
            if let Err(error) = messages.adopt_proven_unsent_channels(owner) {
                tracing::warn!(%error,"Legacy predeposit channel recovery awaits storage");
            }
            let due = (|| -> Result<Vec<(String, String, Entry, bool)>, StorageError> {
                let mut conn = messages.database.connection()?;
                let tx = conn.transaction()?;
                let pattern = format!("{PREFIX}{owner}:%");
                // A deleted refusal that never reached the server is forgotten;
                // one holding a request or response may be live, so it moves
                // to deletion with its token.
                tx.execute("DELETE FROM settings WHERE key LIKE ?1 AND json_extract(value,'$.phase')='terminal' AND json_extract(value,'$.prepared') IS NULL AND json_extract(value,'$.response') IS NULL AND (EXISTS(SELECT 1 FROM deleted_messages WHERE id=COALESCE(json_extract(settings.value,'$.row_id'),json_extract(settings.value,'$.entry.row.id'))) OR EXISTS(SELECT 1 FROM messages WHERE id=COALESCE(json_extract(settings.value,'$.row_id'),json_extract(settings.value,'$.entry.row.id')) AND is_deleted=1))",params![pattern])?;
                tx.execute("UPDATE settings SET value=json_set(value,'$.phase','deleting','$.next_at',0) WHERE key LIKE ?1 AND json_extract(value,'$.phase')='terminal' AND (json_extract(value,'$.prepared') IS NOT NULL OR json_extract(value,'$.response') IS NOT NULL) AND (EXISTS(SELECT 1 FROM deleted_messages WHERE id=json_extract(settings.value,'$.entry.row.id')) OR EXISTS(SELECT 1 FROM messages WHERE id=json_extract(settings.value,'$.entry.row.id') AND is_deleted=1))",params![pattern])?;
                if reconnected {
                    tx.execute("UPDATE settings SET value=json_set(value,'$.next_at',0) WHERE key LIKE ?1 AND json_extract(value,'$.phase')='queued'",params![pattern])?;
                }
                let rows: Vec<(String, String, String)> = {
                    let mut stmt = tx.prepare(&format!("SELECT key,json_extract(value,'$.entry'),json_extract(value,'$.phase') FROM settings WHERE key LIKE ?1 AND json_extract(value,'$.phase') IN {LIVE} AND json_extract(value,'$.next_at')<=?2 ORDER BY json_extract(value,'$.next_at'),key LIMIT 4"))?;
                    let mapped = stmt.query_map(params![pattern, now()], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })?;
                    mapped.collect::<Result<_, _>>()?
                };
                let mut due = Vec::new();
                for (key, raw, phase) in rows {
                    let entry: Entry = match serde_json::from_str(&raw) {
                        Ok(entry) => entry,
                        Err(_) => {
                            tx.execute("UPDATE settings SET value=json_set(value,'$.phase','terminal') WHERE key=?1",params![key])?;
                            tracing::warn!("Malformed channel intent retained without submission");
                            continue;
                        }
                    };
                    if entry.row.sender_id != Some(owner)
                        || entry.row.conversation_type != ConversationType::Channel
                    {
                        tx.execute("UPDATE settings SET value=json_set(value,'$.phase','terminal') WHERE key=?1",params![key])?;
                        continue;
                    }
                    let mut deleting = phase == "deleting";
                    let deleted: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deleted_messages WHERE id=?1) OR EXISTS(SELECT 1 FROM messages WHERE id=?1 AND is_deleted=1)",params![entry.row.id.to_string()],|r|r.get(0))?;
                    if deleted && !deleting {
                        settle_admission(&tx, owner, &[entry.row.id])?;
                        // A retained request or response may already be a
                        // live server post: keep its token for deletion.
                        deleting = tx.execute("UPDATE settings SET value=json_set(value,'$.phase','deleting') WHERE key=?1 AND (json_extract(value,'$.prepared') IS NOT NULL OR json_extract(value,'$.response') IS NOT NULL)",params![key])? == 1;
                        if !deleting {
                            tx.execute("DELETE FROM settings WHERE key=?1", params![key])?;
                            continue;
                        }
                    }
                    let generation = uuid::Uuid::new_v4().to_string();
                    tx.execute("UPDATE settings SET value=json_set(value,'$.phase',?4,'$.generation',?2,'$.next_at',?3) WHERE key=?1",params![key,generation,now()+LEASE_SECONDS,if deleting {"deleting"} else {"preparing"}])?;
                    due.push((key, generation, entry, deleting));
                }
                tx.commit()?;
                Ok(due)
            })();
            let due = match due {
                Ok(due) => due,
                Err(error) => {
                    tracing::warn!(%error,"Channel recovery journal unavailable");
                    continue;
                }
            };
            for (key, generation, entry, deleting) in due {
                let current = client.auth_state().get();
                if current.current_user_id != Some(owner)
                    || current.session_revision != state.session_revision
                    || !client.offline_state().connection_state().is_connected()
                {
                    break;
                }
                if deleting {
                    crate::services::gateway::submission::SEND_SESSION
                        .scope(
                            session.clone(),
                            messages.delete_channel_entry(&key, &generation, &entry),
                        )
                        .await;
                    continue;
                }
                let result = crate::services::gateway::submission::SEND_SESSION
                    .scope(session.clone(), async {
                        messages.message_repo.insert_or_ignore(&entry.row)?;
                        messages
                            .attempt_channel_entry(&key, &generation, &entry)
                            .await
                    })
                    .await;
                let current = client.auth_state().get();
                if current.current_user_id != Some(owner)
                    || current.session_revision != state.session_revision
                {
                    break;
                }
                match result {
                    Ok(sent) => {
                        if sent.id != entry.row.id {
                            client.event_bus().emit_domain(
                                crate::events::DomainEvent::MessageDeleted {
                                    message_id: entry.row.id,
                                    author_id: owner,
                                    channel_id: Some(entry.row.conversation_id),
                                    target_sent_timestamp: entry.row.sent_at,
                                },
                            );
                        }
                        client.event_bus().emit_domain(
                            crate::events::DomainEvent::MessageReceived { message: sent },
                        );
                    }
                    Err(error) => tracing::warn!(%error,"Channel recovery remains retained"),
                }
            }
        }
    })
}
