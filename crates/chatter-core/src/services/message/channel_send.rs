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
/// The retained (`unresolved`) shape of a journal whose bubble the user
/// deleted: the plaintext entry and lease fields go, while the exact request,
/// token, response, canonical id, intent and sending device stay (the device
/// is part of the request key, `settle_cancelled_deposit`). `?2` is now.
const RETAIN: &str = "json_set(json_remove(value,'$.entry','$.generation','$.attempts','$.next_at'),'$.phase','unresolved','$.row_id',COALESCE(json_extract(value,'$.row_id'),json_extract(value,'$.entry.row.id')),'$.channel_id',COALESCE(json_extract(value,'$.channel_id'),json_extract(value,'$.entry.row.conversation_id')),'$.device',COALESCE(json_extract(value,'$.device'),json_extract(value,'$.entry.device')),'$.retained_at',COALESCE(json_extract(value,'$.retained_at'),?2))";
/// After the server forgets a client_request_id (`SendRequestRetention`, 7d,
/// server `types.go:21`; one day of clock margin here) retained request bytes
/// can resolve nothing. Only they are dropped; token and identities stay.
const REQUEST_WINDOW: i64 = 8 * 24 * 60 * 60;
/// Longest replay of a prepared (unsealed) request after `prepared.at`.
const REPLAY_HORIZON: i64 = 23 * 60 * 60;
/// A prepared request may be replayed only below sealed-sender freshness.
/// A sealed (deletion-token) request embeds our sender certificate, which
/// has only 12 h left when used: it gets the sealed DM outbox's window.
fn replayable(prepared: &Prepared) -> bool {
    // Tolerate a small backward clock step instead of expiring at once.
    let age = now() - prepared.at;
    let horizon = if prepared.deletion_token.is_some() {
        super::dm_outbox::REPLAY_WINDOW_SECONDS
    } else {
        REPLAY_HORIZON
    };
    (-300..horizon).contains(&age)
}

/// The server's SendMessage idempotency key for this account, device,
/// channel and client_request_id (server `send.go`
/// `setSendRequestIdempotency`), which it returns as
/// `Message.send_request_key`.
fn send_request_key(owner: UserId, device: u32, channel: ChannelId, request: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut key = Sha256::new();
    key.update(b"chatter.send-message-request.v1\0");
    for part in [owner.to_string(), device.to_string(), channel.to_string(), request.to_lowercase()] {
        key.update((part.len() as u32).to_be_bytes());
        key.update(part.as_bytes());
    }
    key.finalize().into()
}

/// Whether `post` is the deposit of a send the user deleted before its
/// canonical id came back: its `send_request_key` is exactly the one a
/// retained (`unresolved`) journal of `owner` in `channel` sent under, the
/// journal UUID being the request's client_request_id. A match learns the
/// id: it is recorded on the journal and tombstoned, and any local copy is
/// removed, so every later fetch drops it as an ordinary tombstone. Nothing
/// else is held off. Delete-for-me stays local; only a journal carrying the
/// explicit delete-for-everyone intent (`remote_delete`) goes on to
/// `deleting`, where the recovery worker removes the server copy with the
/// retained token.
///
/// A journal retained before `RETAIN` kept `$.device` has no recorded
/// sending device. The key commits to it, so the candidates (the post's own
/// `author_device_id`, this install's `current_device`) are only tried, never
/// trusted: a wrong one cannot produce the key. The matching device is then
/// recorded on the journal.
pub(crate) fn settle_cancelled_deposit(
    db: &Database,
    owner: UserId,
    channel: ChannelId,
    current_device: Option<u32>,
    post: &crate::proto::chat_v1::Message,
) -> Result<bool, StorageError> {
    if post.send_request_key.len() != 32
        || !(post.author_id.is_empty() || post.author_id.parse::<UserId>().ok() == Some(owner))
    {
        return Ok(false);
    }
    let Ok(id) = post.id.parse::<MessageId>() else {
        return Ok(false);
    };
    let prefix = format!("{PREFIX}{owner}:");
    let journals: Vec<(String, Option<u32>)> = {
        let conn = db.read_connection()?;
        let mut stmt = conn.prepare("SELECT key,COALESCE(json_extract(value,'$.device'),json_extract(value,'$.entry.device')) FROM settings WHERE key LIKE ?1 AND json_extract(value,'$.phase')='unresolved' AND json_extract(value,'$.server_id') IS NULL AND json_extract(value,'$.channel_id')=?2")?;
        let rows = stmt.query_map(params![format!("{prefix}%"), channel.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let Some((key, device)) = journals.into_iter().find_map(|(key, device)| {
        let request = key.strip_prefix(&prefix)?.to_owned();
        let candidates = match device {
            Some(device) => vec![device],
            None => vec![post.author_device_id, current_device.unwrap_or(0)],
        };
        let device = candidates.into_iter().find(|device| {
            *device != 0
                && (post.author_device_id == 0 || post.author_device_id == *device)
                && send_request_key(owner, *device, channel, &request)[..] == post.send_request_key[..]
        })?;
        Some((key, device))
    }) else {
        return Ok(false);
    };
    let mut conn = db.connection()?;
    let tx = conn.transaction()?;
    let learned = tx.execute("UPDATE settings SET value=json_set(value,'$.server_id',?2,'$.device',?3) WHERE key=?1 AND json_extract(value,'$.phase')='unresolved' AND json_extract(value,'$.server_id') IS NULL",params![key,id.to_string(),device])? == 1;
    // An explicit delete-for-everyone was waiting for exactly this id.
    let remote = learned && tx.execute("UPDATE settings SET value=json_set(value,'$.phase','deleting','$.attempts',0,'$.next_at',0) WHERE key=?1 AND json_extract(value,'$.remote_delete')",params![key])? == 1;
    tx.execute("INSERT OR IGNORE INTO deleted_messages(id) VALUES(?1)", params![id.to_string()])?;
    tx.execute("DELETE FROM messages WHERE id=?1", params![id.to_string()])?;
    tx.commit()?;
    tracing::info!(message_id = %id, remote, "Deleted send's deposit recognised by its request key; tombstoned");
    Ok(true)
}

/// What removing a send's server copy needs once its plaintext entry may be
/// gone (`RETAIN`): the channel and the sending device. The canonical id and
/// the deletion token stay on the journal itself.
struct Target {
    channel: ChannelId,
    device: u32,
}

enum Due {
    Send(Entry),
    Delete(Target),
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

/// Settle a journal whose bubble was deleted. Delete-for-me and expiry stay
/// local. Only an explicit delete-for-everyone (`remote_delete`) on a known
/// canonical id goes on to `deleting`. Nothing is replayed to learn an
/// unknown id: an uncertain deposit keeps its exact request, token and
/// intent. A known canonical id is suppressed so catch-up cannot restore it.
/// Returns true when server deletion is due.
fn retire_deleted(conn: &rusqlite::Connection, key: &str) -> Result<bool, StorageError> {
    conn.execute("INSERT OR IGNORE INTO deleted_messages(id) SELECT json_extract(value,'$.server_id') FROM settings WHERE key=?1 AND json_extract(value,'$.server_id') IS NOT NULL",params![key])?;
    conn.execute("DELETE FROM messages WHERE id=(SELECT json_extract(value,'$.server_id') FROM settings WHERE key=?1)",params![key])?;
    // Never handed to the transport: nothing can exist on the server.
    if conn.execute("DELETE FROM settings WHERE key=?1 AND json_extract(value,'$.prepared') IS NULL AND json_extract(value,'$.response') IS NULL",params![key])? == 1 {
        return Ok(false);
    }
    if conn.execute("UPDATE settings SET value=json_set(value,'$.phase','deleting','$.next_at',0) WHERE key=?1 AND json_extract(value,'$.server_id') IS NOT NULL AND json_extract(value,'$.remote_delete')",params![key])? == 1 {
        return Ok(true);
    }
    conn.execute(&format!("UPDATE settings SET value={RETAIN} WHERE key=?1"),params![key,now()])?;
    Ok(false)
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
        let changed = deposit.db.connection()?.execute("UPDATE settings SET value=json_set(value,'$.phase','accepted','$.response',json(?3),'$.server_id',?4,'$.next_at',0) WHERE key=?1 AND json_extract(value,'$.generation')=?2",params![deposit.key,deposit.generation,raw,proto.id])?;
        if changed != 1 { return Err(invalid("Channel response lease expired")); }
        Ok(())
    }).unwrap_or(Ok(()))
}

impl MessageService {
    /// Explicit delete-for-everyone of a bubble: record the intent on a
    /// channel send the journal still owns, then delete the bubble locally.
    /// Without the intent a deleted bubble never touches the server copy; a
    /// no-op for any other message. If the bubble was not deleted the intent
    /// is withdrawn, so a later delete-for-me of it stays local. A bubble
    /// already gone (`NotFound`) keeps the intent: it was asked for.
    pub(crate) fn delete_channel_for_everyone_local(&self, id: &MessageId) -> Result<(), MessageError> {
        let owner = self.current_user_id()?;
        let journal = key(owner, *id);
        self.database.connection()?.execute("UPDATE settings SET value=json_set(value,'$.remote_delete',json('true')) WHERE key=?1",params![journal])?;
        let result = self.delete_message_local(id);
        if matches!(&result, Err(e) if !matches!(e, MessageError::Storage(StorageError::NotFound(_)))) {
            self.database.connection()?.execute("UPDATE settings SET value=json_remove(value,'$.remote_delete') WHERE key=?1",params![journal])?;
        }
        result
    }

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
                        // Deleted while the post was accepted: suppress both
                        // ids so catch-up cannot restore it and retire the
                        // journal (`retire_deleted`). Only this generation may
                        // do so: a stale future finishing after the confirm
                        // (journal gone, parked id tombstoned by it) must not
                        // touch the server copy.
                        let owned: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM settings WHERE key=?1 AND json_extract(value,'$.generation')=?2)",params![journal_key,generation],|r|r.get(0))?;
                        if owned {
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
                            retire_deleted(&tx, journal_key)?;
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
    /// The user deleted this bubble for everyone and the canonical id is
    /// known, from the stored response (`retire_deleted`) or from the exact
    /// request key of the deposit (`settle_cancelled_deposit`). Remove the
    /// server copy with the retained token; no plaintext entry is needed. The
    /// journal, and with it the only deletion authority for a sealed post, is
    /// dropped only after the server confirms the copy is gone. Anything else
    /// keeps it retained.
    async fn finish_channel_deletion(
        &self,
        journal_key: &str,
        generation: &str,
        owner: UserId,
        target: &Target,
    ) -> Result<(), MessageError> {
        let held: Option<(Option<String>, String)> = self.database.read_connection()?.query_row(
            "SELECT json_extract(value,'$.prepared'),json_extract(value,'$.server_id') FROM settings WHERE key=?1 AND json_extract(value,'$.generation')=?2 AND json_extract(value,'$.phase')='deleting' AND json_extract(value,'$.remote_delete') AND json_extract(value,'$.server_id') IS NOT NULL",
            params![journal_key, generation], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        let retain = || -> Result<(), MessageError> {
            self.database.connection()?.execute(&format!("UPDATE settings SET value={RETAIN} WHERE key=?1 AND json_extract(value,'$.generation')=?3"),params![journal_key,now(),generation])?;
            Ok(())
        };
        let Some((prepared, server_id)) = held else {
            // No explicit intent or no canonical id: never replay to learn one.
            return retain();
        };
        let prepared: Option<Prepared> = prepared
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(|_| invalid("Invalid prepared channel request"))?;
        let server_id: MessageId = server_id
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
        crate::services::gateway::submission::check_send_session()?;
        if owner != self.current_user_id()?
            || self.settings_repo.get::<u32>(keys::CURRENT_DEVICE_ID)? != Some(target.device)
        {
            tracing::warn!(message_id=%server_id, "Channel delete-for-everyone sender changed; token retained");
            return retain();
        }
        let token = prepared
            .and_then(|p| p.deletion_token)
            .map(|t| t.to_vec())
            .unwrap_or_default();
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.message_client().delete_message(
                &server_id.to_string(),
                &target.channel.to_string(),
                token,
            ),
        )
        .await
        .unwrap_or(Err(NetworkError::Timeout))
        {
            Ok(()) | Err(NetworkError::NotFound(_)) => {
                self.database.connection()?.execute("DELETE FROM settings WHERE key=?1 AND json_extract(value,'$.generation')=?2",params![journal_key,generation])?;
                Ok(())
            }
            // An expired session or a cancelled call is not a refusal.
            Err(e)
                if e.is_retryable()
                    || e.is_transport_error()
                    || matches!(e, NetworkError::Unauthenticated | NetworkError::Unknown(_)) =>
            {
                Err(e.into())
            }
            Err(e) => {
                tracing::warn!(error=%e, message_id=%server_id, "Server refused deleting a channel post; token retained");
                retain()
            }
        }
    }

    async fn delete_channel_entry(
        &self,
        journal_key: &str,
        generation: &str,
        owner: UserId,
        target: &Target,
    ) {
        if let Err(error) = self
            .finish_channel_deletion(journal_key, generation, owner, target)
            .await
        {
            tracing::warn!(%error, journal = journal_key, "Channel deletion retained");
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
            let due = (|| -> Result<Vec<(String, String, Due)>, StorageError> {
                let mut conn = messages.database.connection()?;
                let tx = conn.transaction()?;
                let pattern = format!("{PREFIX}{owner}:%");
                // A deleted refusal that never reached the server is forgotten;
                // one holding a request or response is retired with its token.
                tx.execute("DELETE FROM settings WHERE key LIKE ?1 AND json_extract(value,'$.phase')='terminal' AND json_extract(value,'$.prepared') IS NULL AND json_extract(value,'$.response') IS NULL AND (EXISTS(SELECT 1 FROM deleted_messages WHERE id=COALESCE(json_extract(settings.value,'$.row_id'),json_extract(settings.value,'$.entry.row.id'))) OR EXISTS(SELECT 1 FROM messages WHERE id=COALESCE(json_extract(settings.value,'$.row_id'),json_extract(settings.value,'$.entry.row.id')) AND is_deleted=1))",params![pattern])?;
                let refused: Vec<String> = {
                    let mut stmt = tx.prepare("SELECT key FROM settings WHERE key LIKE ?1 AND json_extract(value,'$.phase')='terminal' AND (json_extract(value,'$.prepared') IS NOT NULL OR json_extract(value,'$.response') IS NOT NULL) AND (EXISTS(SELECT 1 FROM deleted_messages WHERE id=json_extract(settings.value,'$.entry.row.id')) OR EXISTS(SELECT 1 FROM messages WHERE id=json_extract(settings.value,'$.entry.row.id') AND is_deleted=1))")?;
                    let mapped = stmt.query_map(params![pattern], |r| r.get(0))?;
                    mapped.collect::<Result<_, _>>()?
                };
                for key in refused {
                    retire_deleted(&tx, &key)?;
                }
                // Bound retained metadata: request bytes are kept only while
                // they could still resolve an unknown canonical id.
                tx.execute("UPDATE settings SET value=json_remove(value,'$.prepared.request','$.prepared.state_hash','$.response') WHERE key LIKE ?1 AND json_extract(value,'$.phase')='unresolved' AND json_extract(value,'$.prepared.request') IS NOT NULL AND (json_extract(value,'$.server_id') IS NOT NULL OR json_extract(value,'$.prepared.at')<?2)",params![pattern,now()-REQUEST_WINDOW])?;
                if reconnected {
                    tx.execute("UPDATE settings SET value=json_set(value,'$.next_at',0) WHERE key LIKE ?1 AND json_extract(value,'$.phase')='queued'",params![pattern])?;
                }
                let rows: Vec<(String, Option<String>, String, Option<String>, Option<u32>)> = {
                    let mut stmt = tx.prepare(&format!("SELECT key,json_extract(value,'$.entry'),json_extract(value,'$.phase'),COALESCE(json_extract(value,'$.channel_id'),json_extract(value,'$.entry.row.conversation_id')),COALESCE(json_extract(value,'$.device'),json_extract(value,'$.entry.device')) FROM settings WHERE key LIKE ?1 AND json_extract(value,'$.phase') IN {LIVE} AND json_extract(value,'$.next_at')<=?2 ORDER BY json_extract(value,'$.next_at'),key LIMIT 4"))?;
                    let mapped = stmt.query_map(params![pattern, now()], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    })?;
                    mapped.collect::<Result<_, _>>()?
                };
                let mut due = Vec::new();
                for (key, raw, phase, channel, device) in rows {
                    if phase == "deleting" {
                        // Needs no entry: a send whose id was learned only
                        // from its request key (`settle_cancelled_deposit`)
                        // was retained without one. The journal is this
                        // owner's by its key.
                        let Some((channel, device)) = channel
                            .and_then(|channel| channel.parse::<ChannelId>().ok())
                            .zip(device)
                        else {
                            tx.execute(&format!("UPDATE settings SET value={RETAIN} WHERE key=?1"),params![key,now()])?;
                            continue;
                        };
                        let generation = uuid::Uuid::new_v4().to_string();
                        tx.execute("UPDATE settings SET value=json_set(value,'$.generation',?2,'$.next_at',?3) WHERE key=?1",params![key,generation,now()+LEASE_SECONDS])?;
                        due.push((key, generation, Due::Delete(Target { channel, device })));
                        continue;
                    }
                    let entry: Entry = match serde_json::from_str(raw.as_deref().unwrap_or_default()) {
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
                    let mut deleting = false;
                    let deleted: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deleted_messages WHERE id=?1) OR EXISTS(SELECT 1 FROM messages WHERE id=?1 AND is_deleted=1)",params![entry.row.id.to_string()],|r|r.get(0))?;
                    if deleted {
                        settle_admission(&tx, owner, &[entry.row.id])?;
                        deleting = retire_deleted(&tx, &key)?;
                        if !deleting {
                            continue;
                        }
                    }
                    let generation = uuid::Uuid::new_v4().to_string();
                    tx.execute("UPDATE settings SET value=json_set(value,'$.phase',?4,'$.generation',?2,'$.next_at',?3) WHERE key=?1",params![key,generation,now()+LEASE_SECONDS,if deleting {"deleting"} else {"preparing"}])?;
                    let job = if deleting {
                        Due::Delete(Target { channel: entry.row.conversation_id, device: entry.device })
                    } else {
                        Due::Send(entry)
                    };
                    due.push((key, generation, job));
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
            for (key, generation, job) in due {
                let current = client.auth_state().get();
                if current.current_user_id != Some(owner)
                    || current.session_revision != state.session_revision
                    || !client.offline_state().connection_state().is_connected()
                {
                    break;
                }
                let entry = match job {
                    Due::Send(entry) => entry,
                    Due::Delete(target) => {
                        crate::services::gateway::submission::SEND_SESSION
                            .scope(
                                session.clone(),
                                messages.delete_channel_entry(&key, &generation, owner, &target),
                            )
                            .await;
                        continue;
                    }
                };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Same key as the server's (`send_idempotency_test.go` known answer);
    /// the request id's case does not matter.
    #[test]
    fn send_request_key_matches_server() {
        let owner: UserId = "33333333-3333-4333-8333-333333333333".parse().unwrap();
        let channel: ChannelId = "11111111-1111-4111-8111-111111111111".parse().unwrap();
        let rid = "0b6c1a52-8d0e-4f43-9a51-2f6f2f0e9a10";
        let key = send_request_key(owner, 2, channel, rid);
        assert_eq!(
            hex::encode(key),
            "6f6e867173e113e37dd0dad9ad82717d0c3ec2dc0310893ed30d4aec8ed03da8"
        );
        assert_eq!(key, send_request_key(owner, 2, channel, &rid.to_uppercase()));
    }

    /// Only a post carrying exactly the request key of this account's
    /// retained, deleted send is recognised (and then tombstoned); its
    /// other posts in the channel, whatever their time, are untouched.
    #[test]
    fn deleted_uncertain_send_settles_only_its_exact_post() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("journal.db"), "test-only").unwrap();
        db.run_migrations().unwrap();
        let (me, other, channel) = (UserId::new(), UserId::new(), ChannelId::new());
        let request = MessageId::new();
        db.connection()
            .unwrap()
            .execute(
                "INSERT INTO settings(key,value,updated_at) VALUES(?1,?2,'now')",
                params![
                    key(me, request),
                    serde_json::json!({"phase":"unresolved","channel_id":channel,"device":3,"prepared":{"at":now()}}).to_string()
                ],
            )
            .unwrap();
        let post = |author: &str, device: u32, key: [u8; 32]| crate::proto::chat_v1::Message {
            id: MessageId::new().to_string(),
            channel_id: channel.to_string(),
            author_id: author.into(),
            author_device_id: device,
            send_request_key: key.to_vec(),
            ..Default::default()
        };
        let settles = |p: &crate::proto::chat_v1::Message| settle_cancelled_deposit(&db, me, channel, Some(3), p).unwrap();
        let exact = send_request_key(me, 3, channel, &request.to_string());
        // Another send of ours in the same channel, at the same moment.
        assert!(!settles(&post(&me.to_string(), 3, send_request_key(me, 3, channel, &MessageId::new().to_string()))));
        // The same request id under another device, account or channel.
        assert!(!settles(&post(&me.to_string(), 3, send_request_key(me, 4, channel, &request.to_string()))));
        assert!(!settles(&post(&me.to_string(), 3, send_request_key(other, 3, channel, &request.to_string()))));
        assert!(!settles(&post(&me.to_string(), 3, send_request_key(me, 3, ChannelId::new(), &request.to_string()))));
        assert!(!settle_cancelled_deposit(&db, other, channel, Some(3), &post("", 3, exact)).unwrap());
        // The exact key, but claimed by someone else's post.
        assert!(!settles(&post(&other.to_string(), 3, exact)));
        assert!(!settles(&post(&me.to_string(), 4, exact)));
        // The deposit itself, sealed (no author) or not: learned and tombstoned.
        let sealed = post("", 3, exact);
        assert!(settles(&sealed));
        let tombstoned: bool = db
            .read_connection()
            .unwrap()
            .query_row("SELECT EXISTS(SELECT 1 FROM deleted_messages WHERE id=?1)", params![sealed.id], |r| r.get(0))
            .unwrap();
        assert!(tombstoned);
        // Settled once: the journal now knows its id and matches nothing more.
        assert!(!settles(&post(&me.to_string(), 3, exact)));
    }

    /// A journal retained before `$.device` was kept still settles, by trying
    /// the post's device and this install's device against the key (and
    /// records the one that matched). Only one carrying the explicit
    /// delete-for-everyone intent goes on to remote deletion.
    #[test]
    fn legacy_journal_settles_and_only_explicit_intent_deletes_remotely() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("journal.db"), "test-only").unwrap();
        db.run_migrations().unwrap();
        let (me, channel) = (UserId::new(), ChannelId::new());
        let (local, remote) = (MessageId::new(), MessageId::new());
        for (request, intent) in [(local, false), (remote, true)] {
            let mut value = serde_json::json!({"phase":"unresolved","channel_id":channel,"prepared":{"at":now(),"deletion_token":[7u8;32]}});
            if intent {
                value["remote_delete"] = serde_json::json!(true);
            }
            db.connection()
                .unwrap()
                .execute(
                    "INSERT INTO settings(key,value,updated_at) VALUES(?1,?2,'now')",
                    params![key(me, request), value.to_string()],
                )
                .unwrap();
        }
        let post = |request: MessageId, device: u32| crate::proto::chat_v1::Message {
            id: MessageId::new().to_string(),
            channel_id: channel.to_string(),
            author_device_id: device,
            send_request_key: send_request_key(me, 5, channel, &request.to_string()).to_vec(),
            ..Default::default()
        };
        // Neither candidate is the sending device: no match.
        assert!(!settle_cancelled_deposit(&db, me, channel, Some(6), &post(local, 0)).unwrap());
        // Sealed post, device learned from this install; delete-for-me stays local.
        assert!(settle_cancelled_deposit(&db, me, channel, Some(5), &post(local, 0)).unwrap());
        // Post's own device, this install re-registered since.
        assert!(settle_cancelled_deposit(&db, me, channel, Some(6), &post(remote, 5)).unwrap());
        let journal = |request: MessageId| -> (String, u32) {
            db.read_connection()
                .unwrap()
                .query_row(
                    "SELECT json_extract(value,'$.phase'),json_extract(value,'$.device') FROM settings WHERE key=?1",
                    params![key(me, request)],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        };
        assert_eq!(journal(local), ("unresolved".into(), 5));
        assert_eq!(journal(remote), ("deleting".into(), 5));
    }
}
