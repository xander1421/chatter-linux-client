//! Durable exact-envelope outbox for normal-mode (sealed sender) friend DMs.
//!
//! A DM is encrypted at most once per delivery. The per-device ciphertexts
//! are stored (`dm_outbox.request`) before the first request byte leaves, and
//! every later attempt, live or after a restart or reconnect, replays exactly
//! those bytes under the same message GUID (`client_request_id`). The server
//! queues one copy per (destination user, device, GUID)
//! (`device_messages` `ON CONFLICT ... DO NOTHING`), and the receiver drops a
//! second DataMessage with the same (conversation, sender, sent_at), so a
//! replay after a lost response cannot deliver twice. An entry whose
//! ciphertext was never stored never reached the network; only that phase
//! encrypts, under a generation fence, so two workers cannot both deposit
//! fresh encryptions.
//!
//! A stored envelope carries the sender certificate it was sealed with, and
//! the receiver checks that certificate at the server's (fresh) timestamp. So
//! an envelope is replayed only inside [`REPLAY_WINDOW_SECONDS`] of sealing.
//! Past it the entry fails closed as "Delivery unconfirmed" with the stored
//! envelope dropped, and the user decides whether to retry. Sealing again on
//! our own would be a second deposit under a new GUID: if the first copy did
//! land, the receiver's only guard is its live (sender, sent_at) row, which a
//! delete-for-me or an expiry hard-deletes, so the message would come back.
//!
//! Shield (mailbox) DMs do not enter: their bootstrap queue owns them, and
//! the worker waits while Shield is on rather than leak the recipient.
use super::{MessageError, MessageService};
use crate::domain::{
    ids::{ChannelId, MessageId, UserId},
    message::ConversationType,
};
use crate::error::{NetworkError, StorageError};
use crate::proto::chat_v1::{SealedDelivery, SealedDeviceMessage, SendSealedMessageRequest};
use crate::proto::content_v1::Content;
use crate::storage::database::Database;
use prost::Message as _;
use rusqlite::{OptionalExtension, params};
use std::sync::Arc;

/// Unsent DMs one account may hold. A send past it fails before encryption.
const MAX_PENDING: i64 = 256;
/// After this an unconfirmed DM is shown as failed. The server keeps a queued
/// envelope 46 days, so a manual retry (same sent_at) still deduplicates.
const RETENTION_SECONDS: i64 = 30 * 24 * 3600;
/// How long one attempt owns an entry before another may take it over.
const LEASE_SECONDS: i64 = 60;
/// Attempts per worker tick.
const BATCH: i64 = 4;
/// How long after sealing a stored envelope may still be replayed. A sender
/// certificate is used only until 12 h before its 24 h expiry
/// (`internal/services/auth/service/sealed_sender.go:26-29`, refresh in
/// `signal/session.rs` `get_cached_sender_certificate`), so a sealed envelope
/// has at least 12 h left. Two hours of that are margin for this device's
/// clock against the server's, which stamps the replay.
pub(super) const REPLAY_WINDOW_SECONDS: i64 = 10 * 3600;

/// Whether an envelope sealed at `sealed_at` still carries a certificate the
/// receiver accepts. Unknown seal time, or a clock that went backwards: no.
fn replayable(sealed_at: Option<i64>, now: i64) -> bool {
    sealed_at.is_some_and(|t| (t..t + REPLAY_WINDOW_SECONDS).contains(&now))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn invalid(text: &str) -> StorageError {
    StorageError::InvalidData(text.into())
}

struct Slot {
    db: Arc<Database>,
    owner: String,
    id: String,
    generation: String,
    request_id: String,
    peer: String,
}
tokio::task_local! { static DM_OUTBOX: Slot; }

/// The GUID of the delivery in progress, if this future is one.
pub(super) fn request_id() -> Option<String> {
    DM_OUTBOX.try_with(|slot| slot.request_id.clone()).ok()
}

/// Called by `send_sealed_batch` before any request byte is sent: store the
/// exact envelopes about to go out. Fails, and so stops the send, when another
/// attempt took the entry over or the user deleted the message meanwhile.
pub(super) fn retain_request(
    recipient: &str,
    messages: &[SealedDeviceMessage],
    delivery: SealedDelivery,
    uak: Option<&[u8; 16]>,
    client_request_id: &str,
) -> Result<(), StorageError> {
    DM_OUTBOX
        .try_with(|slot| {
            if slot.peer != recipient {
                return Ok(());
            }
            if slot.request_id != client_request_id {
                return Err(invalid("Sealed DM request id changed"));
            }
            let request = SendSealedMessageRequest {
                destination_user_id: recipient.to_owned(),
                messages: messages.to_vec(),
                client_request_id: client_request_id.to_owned(),
                envelope_type: 0,
                delivery: delivery as i32,
            }
            .encode_to_vec();
            // The earliest seal time is kept: a narrowed replay (410) or a
            // re-encryption for added devices (409) never extends the window.
            let changed = slot.db.connection()?.execute(
                "UPDATE dm_outbox SET request=?4, uak=?5, sealed_at=COALESCE(sealed_at,?6)
                 WHERE owner_id=?1 AND message_id=?2 AND generation=?3
                 AND EXISTS(SELECT 1 FROM messages WHERE id=?2 AND is_deleted=0)",
                params![
                    slot.owner,
                    slot.id,
                    slot.generation,
                    request,
                    uak.map(|k| k.to_vec()),
                    now()
                ],
            )?;
            if changed != 1 {
                return Err(invalid("Sealed DM send was taken over or deleted"));
            }
            Ok(())
        })
        .unwrap_or(Ok(()))
}

/// Whether the outbox holds `message_id` for delivery: its exact envelope,
/// or the content it will encrypt once.
pub(crate) fn owns(db: &Database, owner: &str, message_id: &str) -> Result<bool, StorageError> {
    Ok(db.read_connection()?.query_row(
        "SELECT EXISTS(SELECT 1 FROM dm_outbox WHERE owner_id=?1 AND message_id=?2)",
        params![owner, message_id],
        |row| row.get(0),
    )?)
}

struct Entry {
    peer: UserId,
    device: u32,
    request_id: String,
    content: Vec<u8>,
    request: Option<Vec<u8>>,
    uak: Option<Vec<u8>>,
    devices: Option<Vec<u32>>,
    sealed_at: Option<i64>,
}

/// Outcome of the attempt made inside the composing send.
pub(super) enum FirstAttempt {
    Sent,
    /// Unconfirmed; the worker replays it (or settles a deferred refusal).
    Queued,
    /// Refused before the submission boundary; the row is now `failed`.
    Refused(MessageError),
}

/// What an attempt's failure means for the entry.
fn transient(error: &MessageError) -> bool {
    error.permanent_send_failure_reason().is_none()
        || matches!(
            error,
            MessageError::Storage(_)
                | MessageError::NotLoggedIn
                | MessageError::Network(NetworkError::Unauthenticated)
        )
}

impl MessageService {
    /// Take ownership of a stored `sending` row before anything is encrypted.
    /// `devices` limits a retry to the original send's devices. Returns the
    /// generation that owns the entry for the next [`LEASE_SECONDS`].
    pub(super) fn enqueue_sealed_dm(
        &self,
        row: &MessageId,
        peer: &UserId,
        content: &Content,
        devices: Option<&[u32]>,
    ) -> Result<String, MessageError> {
        let owner = self.current_user_id()?.to_string();
        let device = self.current_device_id_or_zero();
        if device == 0 {
            return Err(MessageError::NotLoggedIn);
        }
        let generation = uuid::Uuid::new_v4().to_string();
        let devices = devices
            .map(|d| serde_json::to_string(d).map_err(|_| invalid("Cannot retain retry devices")))
            .transpose()?;
        let conn = self.database.connection()?;
        let inserted = conn.execute(
            "INSERT INTO dm_outbox(owner_id,message_id,peer_id,device_id,request_id,content,devices,generation,next_attempt_at,created_at)
             SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10
             WHERE (SELECT COUNT(*) FROM dm_outbox WHERE owner_id=?1) < ?11",
            params![
                owner,
                row.to_string(),
                peer.to_string(),
                device,
                uuid::Uuid::new_v4().to_string(),
                content.encode_to_vec(),
                devices,
                generation,
                now() + LEASE_SECONDS,
                now(),
                MAX_PENDING
            ],
        )?;
        if inserted != 1 {
            return Err(MessageError::InvalidContent(
                "too many unsent messages; wait for the connection to return".into(),
            ));
        }
        Ok(generation)
    }

    fn load_outbox_entry(
        &self,
        owner: &str,
        id: &str,
        generation: &str,
    ) -> Result<Option<Entry>, MessageError> {
        let conn = self.database.read_connection()?;
        let row = conn
            .query_row(
                "SELECT peer_id,device_id,request_id,content,request,uak,devices,sealed_at FROM dm_outbox
                 WHERE owner_id=?1 AND message_id=?2 AND generation=?3",
                params![owner, id, generation],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, Option<Vec<u8>>>(4)?,
                        r.get::<_, Option<Vec<u8>>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, Option<i64>>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((peer, device, request_id, content, request, uak, devices, sealed_at)) = row
        else {
            return Ok(None);
        };
        Ok(Some(Entry {
            peer: peer.parse().map_err(|_| invalid("Invalid outbox peer"))?,
            device,
            request_id,
            content,
            request,
            uak,
            devices: devices
                .map(|d| serde_json::from_str(&d).map_err(|_| invalid("Invalid outbox devices")))
                .transpose()?,
            sealed_at,
        }))
    }

    /// One delivery attempt of the entry this generation owns. Encrypts only
    /// when no envelope was stored; otherwise replays the stored one.
    pub(super) async fn deliver_sealed_dm(
        &self,
        id: &MessageId,
        generation: &str,
    ) -> Result<(), MessageError> {
        let owner = self.current_user_id()?.to_string();
        let entry = self
            .load_outbox_entry(&owner, &id.to_string(), generation)?
            .ok_or_else(|| MessageError::Storage(invalid("Sealed DM send was taken over")))?;
        // Waiting, not failing: this transport names the recipient.
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            return Err(MessageError::Network(NetworkError::Unavailable));
        }
        if entry.device != self.current_device_id_or_zero() {
            return Err(MessageError::InvalidContent(
                "the sending device changed".into(),
            ));
        }
        let content = Content::decode(entry.content.as_slice())
            .map_err(|_| invalid("Invalid outbox content"))?;
        // Past the certificate window the stored envelope can no longer be
        // replayed, and whether it landed is unknown: fail closed, never seal
        // again behind the user's back (module doc). Settling drops the entry.
        if entry.request.is_some() && !replayable(entry.sealed_at, now()) {
            tracing::warn!(
                message_id = %id,
                "Sealed DM envelope past its certificate window; delivery unconfirmed"
            );
            return Err(MessageError::DmError(
                super::core::DELIVERY_UNCONFIRMED.into(),
            ));
        }
        let peer = entry.peer.to_string();
        let mut auth_client = self.auth_client();
        let mut message_client = self.message_client();
        let slot = Slot {
            db: self.database.clone(),
            owner,
            id: id.to_string(),
            generation: generation.to_owned(),
            request_id: entry.request_id.clone(),
            peer: peer.clone(),
        };
        let outcome = DM_OUTBOX
            .scope(slot, async {
                if let Some(raw) = entry.request.as_deref() {
                    let request = SendSealedMessageRequest::decode(raw)
                        .map_err(|_| invalid("Invalid outbox request"))?;
                    let ciphertexts = request
                        .messages
                        .into_iter()
                        .filter_map(|m| Some((m.device_id, m.sealed_message?.sealed_body)))
                        .collect();
                    let uak: Option<[u8; 16]> =
                        entry.uak.as_deref().and_then(|k| k.try_into().ok());
                    return Ok::<_, MessageError>(
                        self.deposit_sealed_ciphertexts(
                            &mut message_client,
                            &peer,
                            ciphertexts,
                            uak.as_ref(),
                        )
                        .await,
                    );
                }
                super::server_dm::validate_sealed_plaintext_size(content.encoded_len())?;
                let plaintext = content.encode_to_vec();
                Ok(match entry.devices.as_deref() {
                    Some(devices) => {
                        let ciphertexts = self
                            .signal_service
                            .encrypt_message_devices_bounded(
                                &mut auth_client,
                                &peer,
                                devices,
                                &plaintext,
                            )
                            .await
                            .map_err(|e| MessageError::from_signal_send(e, "device encryption"))?
                            .into_iter()
                            .map(|(device, sealed)| (device, sealed.ciphertext))
                            .collect();
                        let uak = self.sealed_access_key(&peer, false);
                        self.deposit_sealed_ciphertexts(
                            &mut message_client,
                            &peer,
                            ciphertexts,
                            uak.as_ref(),
                        )
                        .await
                    }
                    None => {
                        self.deposit_sealed_to_all_devices(
                            &mut auth_client,
                            &mut message_client,
                            &peer,
                            &plaintext,
                        )
                        .await
                    }
                })
            })
            .await?;
        if outcome.delivered == 0 {
            return Err(outcome
                .error
                .unwrap_or_else(|| MessageError::DmError("sealed DM reached no device".into())));
        }
        if outcome.degraded > 0 {
            tracing::warn!(
                peer_devices_degraded = outcome.degraded,
                "SEALED SENDER DEGRADED: queued DM delivered identified on some devices"
            );
        }
        self.record_dm_send(entry.peer, &content, &outcome.deposits);
        self.sync_sent_to_own_devices(&mut auth_client, &entry.peer, &content)
            .await;
        Ok(())
    }

    /// Record an attempt's outcome. Confirmed: the row becomes `sent` (which
    /// starts its disappearing clock) and the entry and any send journal
    /// bound to the row go in one transaction. Otherwise the entry waits with
    /// backoff; `final_failure` or a non-transient error settles it `failed`.
    ///
    /// Every branch acts only while `generation` still owns this account's
    /// entry: an attempt that lost its lease (or whose entry was settled or
    /// dropped meanwhile) records nothing, and the current owner decides.
    /// Returns whether the outcome was recorded.
    pub(super) fn settle_sealed_dm(
        &self,
        id: &MessageId,
        generation: &str,
        result: &Result<(), MessageError>,
        final_failure: bool,
    ) -> Result<bool, MessageError> {
        let owner = self.current_user_id()?.to_string();
        let mut conn = self.database.connection()?;
        let tx = conn.transaction()?;
        let finish = |status: &str, reason: Option<&str>| -> Result<bool, StorageError> {
            if tx.execute(
                "DELETE FROM dm_outbox WHERE owner_id=?1 AND message_id=?2 AND generation=?3",
                params![owner, id.to_string(), generation],
            )? != 1
            {
                return Ok(false);
            }
            tx.execute("UPDATE messages SET delivery_status=CASE WHEN delivery_status IN ('sent','delivered','read') THEN delivery_status ELSE ?2 END, failure_reason=CASE WHEN delivery_status IN ('sent','delivered','read') OR ?2='sent' THEN NULL ELSE ?3 END WHERE id=?1", params![id.to_string(), status, reason])?;
            // The bound admission claim is resolved with the row. A sent
            // one is tombstoned like `send_attempt::release`, so its attempt
            // id stays spent; a failed one may be composed again.
            let pattern = format!("send.unresolved.v1:{owner}:%");
            if status == "sent" {
                tx.execute("INSERT OR IGNORE INTO settings(key,value,updated_at) SELECT key || ':confirmed:' || json_extract(value,'$.attempt_id'),json_object('attempt_id',json_extract(value,'$.attempt_id'),'message_id',json_extract(value,'$.message_id')),updated_at FROM settings WHERE key LIKE ?1 AND key NOT LIKE '%:confirmed:%' AND json_extract(value,'$.message_id')=?2 AND json_extract(value,'$.attempt_id') IS NOT NULL", params![pattern, id.to_string()])?;
            }
            tx.execute("DELETE FROM settings WHERE key LIKE ?1 AND key NOT LIKE '%:confirmed:%' AND json_extract(value,'$.message_id')=?2 AND json_extract(value,'$.attempt_id') IS NOT NULL", params![pattern, id.to_string()])?;
            Ok(true)
        };
        let recorded = match result {
            Ok(()) => finish("sent", None)?,
            Err(error) if final_failure || !transient(error) => finish(
                "failed",
                Some(
                    error
                        .permanent_send_failure_reason()
                        .unwrap_or("Not delivered"),
                ),
            )?,
            Err(_) => {
                let waiting = tx.execute(
                    "UPDATE dm_outbox SET attempts=MIN(attempts+1,6), next_attempt_at=?4+(5 << MIN(attempts,6))
                     WHERE owner_id=?1 AND message_id=?2 AND generation=?3",
                    params![owner, id.to_string(), generation, now()],
                )? == 1;
                if waiting {
                    tx.execute("UPDATE messages SET failure_reason='Waiting for connection' WHERE id=?1 AND delivery_status='sending'", params![id.to_string()])?;
                }
                waiting
            }
        };
        tx.commit()?;
        Ok(recorded)
    }

    /// The first attempt of a just-enqueued DM, inside the composing send.
    /// A refusal before the submission boundary is settled `failed` at once.
    /// One after it is not: the caller relabels a started send `sending`, so
    /// that verdict is left to the worker, which takes the entry over on its
    /// next tick (at once, not after backoff).
    pub(super) async fn first_sealed_dm_attempt(
        &self,
        id: &MessageId,
        generation: &str,
    ) -> Result<FirstAttempt, MessageError> {
        let result = self.deliver_sealed_dm(id, generation).await;
        if crate::services::gateway::submission::check_send_session().is_err() {
            // Another account or session: the entry keeps its owner and lease.
            return Ok(FirstAttempt::Queued);
        }
        let error = match result {
            // Not recorded: this attempt lost its lease and the worker decides.
            Ok(()) => {
                return Ok(if self.settle_sealed_dm(id, generation, &Ok(()), false)? {
                    FirstAttempt::Sent
                } else {
                    FirstAttempt::Queued
                });
            }
            Err(error) => error,
        };
        if !transient(&error) && !crate::services::gateway::submission::submission_started() {
            // Refused before any ratchet step or request byte: plainly failed.
            let result = Err(error);
            if !self.settle_sealed_dm(id, generation, &result, true)? {
                return Ok(FirstAttempt::Queued);
            }
            return Ok(FirstAttempt::Refused(result.unwrap_err()));
        }
        tracing::warn!(message_id = %id, error = %error, "Sealed DM not confirmed; queued for replay");
        if transient(&error) {
            self.settle_sealed_dm(id, generation, &Err(error), false)?;
        } else {
            let owner = self.current_user_id()?.to_string();
            self.database.connection()?.execute(
                "UPDATE dm_outbox SET next_attempt_at=?4, attempts=6 WHERE owner_id=?1 AND message_id=?2 AND generation=?3",
                params![owner, id.to_string(), generation, now()],
            )?;
        }
        Ok(FirstAttempt::Queued)
    }
}

/// Why an expired entry (past retention, or from a previous device) failed.
/// One holding a stored envelope may have landed, so it reads like a replay
/// past its certificate window; one without never reached the network.
fn expired_error(envelope_stored: bool) -> MessageError {
    MessageError::DmError(if envelope_stored {
        super::core::DELIVERY_UNCONFIRMED.into()
    } else {
        "expired".into()
    })
}

/// Claimed entries of `owner` that are due: leased to a fresh generation in
/// one transaction. Entries whose row the user deleted are dropped (never
/// sent); expired ones and ones from a previous device are settled failed,
/// each with whether it holds a stored envelope.
#[allow(clippy::type_complexity)]
fn claim_due(
    service: &MessageService,
    owner: &UserId,
) -> Result<
    (
        Vec<(MessageId, String, ChannelId, u64)>,
        Vec<(MessageId, String, ChannelId, u64, bool)>,
    ),
    StorageError,
> {
    let owner_s = owner.to_string();
    let device = service.current_device_id_or_zero();
    if device == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut conn = service.database.connection()?;
    let tx = conn.transaction()?;
    let rows: Vec<(String, i64, u32, bool)> = {
        let mut stmt = tx.prepare(
            "SELECT message_id,created_at,device_id,request IS NOT NULL FROM dm_outbox
             WHERE owner_id=?1 AND next_attempt_at<=?2
             ORDER BY created_at,message_id LIMIT ?3",
        )?;
        let mapped = stmt.query_map(params![owner_s, now(), BATCH], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        mapped.collect::<Result<_, _>>()?
    };
    let (mut due, mut expired) = (Vec::new(), Vec::new());
    for (id, created, entry_device, envelope_stored) in rows {
        // Only the account's own live friend-DM row is ever sent.
        let stored: Option<(String, i64)> = tx
            .query_row(
                "SELECT conversation_id,sent_at FROM messages
                 WHERE id=?1 AND is_deleted=0 AND conversation_type=?2 AND sender_id=?3
                 AND NOT EXISTS(SELECT 1 FROM deleted_messages WHERE id=?1)",
                params![id, ConversationType::ServerDm.as_str(), owner_s],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let message_id: MessageId = id.parse().map_err(|_| invalid("Invalid outbox row"))?;
        let Some((conversation, sent_at)) = stored else {
            tx.execute(
                "DELETE FROM dm_outbox WHERE owner_id=?1 AND message_id=?2",
                params![owner_s, id],
            )?;
            continue;
        };
        let conversation: ChannelId = conversation
            .parse()
            .map_err(|_| invalid("Invalid outbox conversation"))?;
        // Leased either way, so settling it is fenced like any attempt.
        let generation = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "UPDATE dm_outbox SET generation=?3, next_attempt_at=?4 WHERE owner_id=?1 AND message_id=?2",
            params![owner_s, id, generation, now() + LEASE_SECONDS],
        )?;
        if created < now() - RETENTION_SECONDS || entry_device != device {
            expired.push((
                message_id,
                generation,
                conversation,
                sent_at as u64,
                envelope_stored,
            ));
            continue;
        }
        due.push((message_id, generation, conversation, sent_at as u64));
    }
    tx.commit()?;
    Ok((due, expired))
}

/// Replays queued sealed DMs on start and whenever the connection is up:
/// one session-scoped worker per client, a few entries per tick, each under
/// its own lease, oldest first.
pub(crate) fn spawn_dm_send_recovery(
    client: &Arc<crate::client::ChatterClient>,
) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(client);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
            let messages = client.messages();
            if messages
                .shield_mode
                .load(std::sync::atomic::Ordering::Acquire)
            {
                continue;
            }
            let (due, expired) = match claim_due(&messages, &owner) {
                Ok(claimed) => claimed,
                Err(error) => {
                    tracing::warn!(%error, "Sealed DM outbox unavailable");
                    continue;
                }
            };
            let session =
                crate::services::gateway::submission::SendSession::capture(client.auth_state());
            let fail = |id: &MessageId,
                        generation: &str,
                        conversation: ChannelId,
                        sent_at: u64,
                        error: MessageError| {
                let reason = error
                    .permanent_send_failure_reason()
                    .unwrap_or("Not delivered")
                    .to_owned();
                match messages.settle_sealed_dm(id, generation, &Err(error), true) {
                    Ok(true) => {
                        client.event_bus().emit_domain(
                            crate::events::DomainEvent::MessageSendFailed {
                                channel_id: conversation,
                                error: reason,
                                message_timestamps: vec![sent_at],
                            },
                        );
                    }
                    Ok(false) => {}
                    Err(error) => tracing::warn!(%error, "Sealed DM failure not recorded"),
                }
            };
            for (id, generation, conversation, sent_at, envelope_stored) in expired {
                tracing::warn!(message_id = %id, envelope_stored, "Sealed DM unconfirmed past retention or from a previous device; marked failed");
                fail(
                    &id,
                    &generation,
                    conversation,
                    sent_at,
                    expired_error(envelope_stored),
                );
            }
            for (id, generation, conversation, sent_at) in due {
                let current = client.auth_state().get();
                if current.current_user_id != Some(owner)
                    || current.session_revision != state.session_revision
                    || !client.offline_state().connection_state().is_connected()
                {
                    break;
                }
                let result = crate::services::gateway::submission::SEND_SESSION
                    .scope(
                        session.clone(),
                        tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            messages.deliver_sealed_dm(&id, &generation),
                        ),
                    )
                    .await
                    .unwrap_or(Err(MessageError::Network(NetworkError::Timeout)));
                let current = client.auth_state().get();
                if current.current_user_id != Some(owner)
                    || current.session_revision != state.session_revision
                {
                    break;
                }
                // A previous attempt deferred its terminal verdict here.
                let final_failure = matches!(&result, Err(e) if !transient(e));
                match result {
                    Ok(()) => match messages.settle_sealed_dm(&id, &generation, &Ok(()), false) {
                        Ok(true) => {
                            client.event_bus().emit_domain(
                                crate::events::DomainEvent::DmBootstrapCompleted {
                                    peer_id: UserId::from_uuid(*conversation.as_uuid()),
                                    sent_timestamps: vec![sent_at],
                                },
                            );
                        }
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(%error, "Sealed DM confirmation not recorded; it will replay")
                        }
                    },
                    Err(error) if final_failure => {
                        tracing::warn!(message_id = %id, %error, "Sealed DM refused; marked failed");
                        fail(&id, &generation, conversation, sent_at, error);
                    }
                    Err(error) => {
                        tracing::info!(message_id = %id, %error, "Sealed DM still unconfirmed; will replay");
                        if let Err(error) =
                            messages.settle_sealed_dm(&id, &generation, &Err(error), false)
                        {
                            tracing::warn!(%error, "Sealed DM backoff not recorded");
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored envelope is replayed only while the certificate sealed into
    /// it is still valid: never with an unknown seal time, never at or past
    /// the window, never across a clock that went backwards.
    #[test]
    fn replay_stays_inside_the_certificate_window() {
        let sealed = 1_000_000;
        assert!(replayable(Some(sealed), sealed));
        assert!(replayable(Some(sealed), sealed + REPLAY_WINDOW_SECONDS - 1));
        assert!(!replayable(Some(sealed), sealed + REPLAY_WINDOW_SECONDS));
        assert!(!replayable(Some(sealed), sealed - 1));
        assert!(!replayable(None, sealed));
        // Inside the 12 h a used certificate has left, with margin.
        assert!(REPLAY_WINDOW_SECONDS < 12 * 3600);
    }

    /// An expired entry that holds a stored envelope may have been delivered,
    /// so it reads "Delivery unconfirmed" (retryable, never auto-resealed);
    /// one that never stored an envelope never left and stays a plain failure.
    /// Either way the verdict is final, so the worker settles it.
    #[test]
    fn expired_entry_with_stored_envelope_reads_delivery_unconfirmed() {
        let stored = expired_error(true);
        assert_eq!(
            stored.permanent_send_failure_reason(),
            Some(super::super::core::DELIVERY_UNCONFIRMED)
        );
        assert!(!transient(&stored));
        let never_sent = expired_error(false);
        assert_eq!(
            never_sent.permanent_send_failure_reason(),
            Some("Cannot send message")
        );
        assert!(!transient(&never_sent));
    }

    /// Only the generation that owns the entry may store envelopes, only for
    /// its peer, and never for a message the user deleted meanwhile.
    #[tokio::test]
    async fn exact_envelope_is_retained_only_by_the_owning_generation() {
        use crate::domain::message::Message;
        use crate::storage::repositories::UnifiedMessageRepository;
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Database::open(&dir.path().join("outbox.db"), "test-only").unwrap());
        db.run_migrations().unwrap();
        let (me, peer) = (UserId::new(), UserId::new());
        let mut row = Message::new_text(
            ChannelId::from(peer),
            ConversationType::ServerDm,
            Some(me),
            "queued".into(),
            7,
        );
        row.delivery_status = "sending".into();
        UnifiedMessageRepository::new(db.clone())
            .insert_outgoing(&row)
            .unwrap();
        db.connection()
            .unwrap()
            .execute(
                "INSERT INTO dm_outbox(owner_id,message_id,peer_id,device_id,request_id,content,generation,created_at)
                 VALUES (?1,?2,?3,1,'guid',x'00','g2',0)",
                params![me.to_string(), row.id.to_string(), peer.to_string()],
            )
            .unwrap();
        let envelope = SealedDeviceMessage {
            device_id: 1,
            registration_id: 9,
            sealed_message: None,
        };
        let stored = || -> Option<Vec<u8>> {
            db.read_connection()
                .unwrap()
                .query_row("SELECT request FROM dm_outbox", [], |r| r.get(0))
                .unwrap()
        };
        let attempt = |generation: &str, recipient: String, guid: &str| {
            let slot = Slot {
                db: db.clone(),
                owner: me.to_string(),
                id: row.id.to_string(),
                generation: generation.into(),
                request_id: "guid".into(),
                peer: peer.to_string(),
            };
            let envelope = envelope.clone();
            let guid = guid.to_owned();
            DM_OUTBOX.scope(slot, async move {
                retain_request(
                    &recipient,
                    &[envelope],
                    SealedDelivery::AllDevices,
                    Some(&[3; 16]),
                    &guid,
                )
            })
        };
        // Outside an outbox send (receipts, typing, SKDMs) nothing is stored.
        retain_request(
            &peer.to_string(),
            &[envelope.clone()],
            SealedDelivery::AllDevices,
            None,
            "x",
        )
        .unwrap();
        assert!(
            attempt("g1", peer.to_string(), "guid").await.is_err(),
            "superseded lease"
        );
        assert!(
            attempt("g2", peer.to_string(), "other").await.is_err(),
            "GUID is fixed"
        );
        attempt("g2", me.to_string(), "guid").await.unwrap(); // own-device sync
        assert!(stored().is_none());
        attempt("g2", peer.to_string(), "guid").await.unwrap();
        let request = SendSealedMessageRequest::decode(stored().unwrap().as_slice()).unwrap();
        assert_eq!(request.client_request_id, "guid");
        // A replay re-stores the envelope but never moves its seal time.
        let sealed_at = || -> Option<i64> {
            db.read_connection()
                .unwrap()
                .query_row("SELECT sealed_at FROM dm_outbox", [], |r| r.get(0))
                .unwrap()
        };
        db.connection()
            .unwrap()
            .execute("UPDATE dm_outbox SET sealed_at=5", [])
            .unwrap();
        attempt("g2", peer.to_string(), "guid").await.unwrap();
        assert_eq!(sealed_at(), Some(5));
        assert_eq!(request.messages, vec![envelope.clone()]);
        assert!(owns(&db, &me.to_string(), &row.id.to_string()).unwrap());
        assert!(!owns(&db, &peer.to_string(), &row.id.to_string()).unwrap());
        db.connection()
            .unwrap()
            .execute("UPDATE messages SET is_deleted=1", [])
            .unwrap();
        assert!(
            attempt("g2", peer.to_string(), "guid").await.is_err(),
            "deleted row is never sent"
        );
    }
}
