//! Sender half of Signal's 1:1 retry: a friend's device could not decrypt a
//! DM we sent and said so with a `DecryptionErrorMessage`; send it again.
//!
//! Signal-Desktop `ts/util/handleRetry.preload.ts` `onRetryRequest`: archive
//! the session when the report's ratchet key matches our current one
//! (`archiveSessionOnMatch` — here the gateway's
//! `handle_decryption_error_report`, which runs first), then look the message
//! up in the send log by the timestamp the report names and send it to the
//! requester again, whether or not a session was archived.
//!
//! The timestamp a report names here is the SERVER's for the failed envelope
//! (`DeviceMessage.server_timestamp`): the receiver could not open the
//! content, so it never saw the message's own. The same value comes back to
//! the sender in `SendSealedMessageResponse.server_timestamp`, and
//! [`DmSentLog`] maps it to the message it was a deposit of.

use std::collections::VecDeque;
use std::sync::Arc;

use prost::Message as _;
use serde::{Deserialize, Serialize};

use super::{MessageError, MessageService};
use crate::domain::ids::UserId;
use crate::domain::message::{ConversationType, Message};
use crate::proto::content_v1::{content, Content, DataMessage};
use crate::services::current_user::CurrentUserProvider;
use crate::storage::repositories::settings_repo::{keys, SettingsRepository};
use crate::util::time::TimestampNanos;

/// How long a message is sent again on request, the window the group resend
/// uses (`group_resend_content`). Signal-Desktop's `retryRespondMaxAge`
/// defaults to 14 days.
const RESEND_MAX_AGE_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

/// Deposits remembered. The whole log is rewritten on every send, so this
/// bounds that write; a deposit older than the newest 512 is answered like a
/// message we no longer hold. A table keyed (peer, device, server_ts) would
/// lift the bound.
const MAX_ENTRIES: usize = 512;

/// One deposit of a friend DM in one peer device's inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct SentDm {
    peer: UserId,
    device: u32,
    /// The server's timestamp for the deposit, epoch millis — what the
    /// device's `DecryptionErrorMessage` names.
    server_ts_ms: u64,
    /// The message's own `sent_at` (epoch nanos), the key of our stored row.
    sent_at: u64,
}

/// Which message each recent friend-DM deposit carried: Signal's send log,
/// holding a pointer to the stored row instead of the proto.
///
/// Persisted (settings key `DM_SENT_LOG`) so a retry request that arrives
/// after an app restart is still answered.
#[derive(Clone)]
pub(crate) struct DmSentLog {
    entries: Arc<parking_lot::Mutex<VecDeque<SentDm>>>,
    settings: Arc<SettingsRepository>,
}

impl DmSentLog {
    pub(crate) fn load(settings: Arc<SettingsRepository>) -> Self {
        let stored: Vec<SentDm> = settings
            .get(keys::DM_SENT_LOG)
            .inspect_err(|e| tracing::warn!(error = %e, "Could not load the DM send log"))
            .ok()
            .flatten()
            .unwrap_or_default();
        Self { entries: Arc::new(parking_lot::Mutex::new(stored.into())), settings }
    }

    /// Record that `content` was deposited for `peer` (ourselves, for the
    /// `SyncSent` copy our other devices get): `deposits` is
    /// `(device, server timestamp in epoch nanos)` per device reached. Only a
    /// message with a body or attachments is recorded — the content a stored
    /// row can rebuild. Receipts and typing are not resent by Signal either
    /// (`ContentHint.IMPLICIT`); reactions, edits and deletes are not resent
    /// HERE, because nothing keeps their wire form.
    pub(crate) fn record(&self, peer: UserId, content: &Content, deposits: &[(u32, u64)]) {
        let has_payload = |dm: &DataMessage| !dm.body.is_empty() || !dm.attachments.is_empty();
        let sent_at = match &content.content {
            Some(content::Content::DataMessage(dm)) if has_payload(dm) => dm.timestamp,
            Some(content::Content::SyncSent(sync)) if sync.message.as_ref().is_some_and(has_payload) => {
                sync.timestamp
            }
            _ => return,
        };
        if deposits.is_empty() {
            return;
        }
        let mut entries = self.entries.lock();
        for &(device, server_ts_ns) in deposits {
            entries.push_back(SentDm { peer, device, server_ts_ms: server_ts_ns / 1_000_000, sent_at });
        }
        self.persist(&mut entries);
    }

    /// The `sent_at` of the message deposited for `(peer, device)` at
    /// `server_ts_ms`, consumed: one resend per deposit, so a peer cannot
    /// make us send the same message without end.
    pub(crate) fn take(&self, peer: &UserId, device: u32, server_ts_ms: u64) -> Option<u64> {
        let mut entries = self.entries.lock();
        let at = entries
            .iter()
            .position(|e| e.peer == *peer && e.device == device && e.server_ts_ms == server_ts_ms)?;
        let sent_at = entries.remove(at)?.sent_at;
        self.persist(&mut entries);
        Some(sent_at)
    }

    /// Drop what is past the resend window or over the cap and write the
    /// rest. A failed write only costs the answers to requests that arrive
    /// after a restart.
    fn persist(&self, entries: &mut VecDeque<SentDm>) {
        let now = TimestampNanos::now().as_u64();
        entries.retain(|e| now.saturating_sub(e.sent_at) < RESEND_MAX_AGE_NS);
        while entries.len() > MAX_ENTRIES {
            entries.pop_front();
        }
        if let Err(e) = self.settings.set(keys::DM_SENT_LOG, &*entries) {
            tracing::warn!(error = %e, "Could not persist the DM send log");
        }
    }
}

impl MessageService {
    /// Answer `peer_id`'s retry request for the DM we deposited for its
    /// device at `timestamp_ms` (server time, epoch millis): send the message
    /// again, to that one device, on the session the next encrypt
    /// establishes. The recipient files it under the message's own
    /// `(sender, sent_at)`, where it would have been.
    pub async fn resend_dm_on_decryption_error(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    ) {
        match self.resend_dm(peer_id, peer_device_id, timestamp_ms).await {
            Ok(true) => tracing::info!(%peer_id, peer_device_id, timestamp_ms,
                "Resent a DM after the peer's DecryptionErrorMessage"),
            Ok(false) => tracing::info!(%peer_id, peer_device_id, timestamp_ms,
                "DM resend request matched nothing we may resend"),
            Err(e) => tracing::warn!(%peer_id, peer_device_id, timestamp_ms, error = %e,
                "Failed to resend a DM"),
        }
    }

    async fn resend_dm(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    ) -> Result<bool, MessageError> {
        // This transport names the recipient to the server, which Shield mode
        // must never do (`send_sealed_dm_content`). Checked before the entry
        // is consumed.
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(false);
        }
        let Some(sent_at) = self.dm_sent_log.take(peer_id, peer_device_id, timestamp_ms) else {
            return Ok(false);
        };
        let Some(content) = self.dm_resend_content(peer_id, sent_at)? else {
            return Ok(false);
        };
        let to_ourselves = matches!(&content.content, Some(content::Content::SyncSent(_)));
        let sealed_body = self
            .signal_service
            .encrypt_message(
                &mut self.auth_client(),
                &peer_id.to_string(),
                peer_device_id,
                &content.encode_to_vec(),
            )
            .await
            .map_err(|e| match e {
                crate::error::ServiceError::Network(e) => MessageError::Network(e),
                e => MessageError::EncryptionFailed(e.to_string()),
            })?;
        if to_ourselves {
            // Identified on the authenticated channel, as every own-device
            // copy is (`sync_to_own_devices`).
            let ciphertexts = std::collections::HashMap::from([(peer_device_id, sealed_body)]);
            let outcome = self
                .deposit_sealed_ciphertexts(&mut self.message_client(), &peer_id.to_string(), ciphertexts, None)
                .await;
            return Ok(outcome.delivered > 0);
        }
        self.deposit_sealed_to_device(peer_id, peer_device_id, sealed_body).await?;
        Ok(true)
    }

    /// What `requester` may have back for the DM we sent at `sent_at`: our
    /// own message, not deleted, within the resend window — the message
    /// itself for the peer it was sent to, its `SyncSent` copy for ourselves
    /// (another of our devices), nothing for anyone else.
    pub(super) fn dm_resend_content(
        &self,
        requester: &UserId,
        sent_at: u64,
    ) -> Result<Option<Content>, MessageError> {
        let us = self.settings_repo.current_user_id()?;
        let Some(message) = self.message_repo.get_by_sender_timestamp(&us, sent_at)? else {
            return Ok(None);
        };
        if message.conversation_type != ConversationType::ServerDm
            || message.is_deleted
            || TimestampNanos::now().as_u64().saturating_sub(sent_at) > RESEND_MAX_AGE_NS
        {
            return Ok(None);
        }
        let peer = UserId::from_uuid(*message.conversation_id.as_uuid());
        let content = self.stored_server_dm_content(&peer, &message)?;
        if *requester == peer {
            return Ok(Some(content));
        }
        if *requester != us {
            return Ok(None);
        }
        Ok(super::dm::sync_sent_copy(&peer, &content).map(|mut copy| {
            Self::pad_content_to_bucket(&mut copy);
            copy
        }))
    }

    /// The wire `Content` of one of our stored friend DMs, as it was first
    /// sent: same `sent_at`, so the recipient sees one message.
    pub(super) fn stored_server_dm_content(
        &self,
        peer_id: &UserId,
        message: &Message,
    ) -> Result<Content, MessageError> {
        let pointers = message
            .attachments
            .iter()
            .map(|id| {
                self.attachment_repo
                    .get(id)?
                    .and_then(|a| a.to_pointer())
                    .ok_or_else(|| MessageError::InvalidContent(format!("attachment {id} can no longer be sent")))
            })
            .collect::<Result<Vec<_>, MessageError>>()?;
        let quote = match (message.reply_to_author_id, message.reply_to) {
            (Some(author), Some(parent_id)) => self.message_repo.get(&parent_id)?.map(|parent| {
                crate::services::gateway::outgoing::QuoteIntent {
                    target_author_id: author,
                    target_timestamp: parent.sent_at,
                    target_preview: message.reply_to_text.clone(),
                }
            }),
            _ => None,
        };
        let (mut content, _, _) = self.server_dm_content(
            peer_id,
            message.as_text().unwrap_or(""),
            pointers,
            message.sent_at,
            quote.as_ref(),
        )?;
        // The timer THIS message was sent under, not the conversation's
        // current one.
        if let Some(content::Content::DataMessage(dm)) = content.content.as_mut() {
            let ttl = self
                .ephemeral_service
                .get_state(&message.id)
                .ok()
                .flatten()
                .map(|e| e.ttl.as_secs() as u32);
            dm.ephemeral = ttl.is_some();
            dm.ephemeral_ttl_seconds = ttl.unwrap_or(0);
        }
        Ok(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;

    fn settings() -> (tempfile::TempDir, Arc<SettingsRepository>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(Database::open(&dir.path().join("s.db"), "testpass").expect("open"));
        db.run_migrations().expect("migrations");
        (dir, Arc::new(SettingsRepository::new(db)))
    }

    fn text(body: &str, sent_at: u64) -> Content {
        crate::services::build_data_message_content(body, vec![], sent_at)
    }

    #[test]
    fn a_deposit_is_answered_once_to_the_device_it_went_to_and_survives_a_restart() {
        let (_dir, settings) = settings();
        let peer = UserId::new();
        let now = TimestampNanos::now().as_u64();
        let server_ts = now + 1_234_567;

        let log = DmSentLog::load(settings.clone());
        log.record(peer, &text("hi", now), &[(1, server_ts), (2, server_ts + 5_000_000)]);
        // Nothing to rebuild from a row: a profile-key handshake, a receipt.
        log.record(peer, &text("", now + 1), &[(1, server_ts + 9_000_000)]);
        log.record(peer, &Content::default(), &[(1, server_ts + 10_000_000)]);
        // Past the resend window.
        log.record(peer, &text("old", now - RESEND_MAX_AGE_NS - 1), &[(1, 42_000_000)]);

        let log = DmSentLog::load(settings.clone());
        let ms = server_ts / 1_000_000;
        assert_eq!(log.take(&UserId::new(), 1, ms), None, "another peer naming our deposit");
        assert_eq!(log.take(&peer, 2, ms), None, "device 2's deposit has its own timestamp");
        assert_eq!(log.take(&peer, 1, ms), Some(now));
        assert_eq!(log.take(&peer, 1, ms), None, "one resend per deposit");
        assert_eq!(log.take(&peer, 1, (server_ts + 9_000_000) / 1_000_000), None);
        assert_eq!(log.take(&peer, 1, (server_ts + 10_000_000) / 1_000_000), None);
        assert_eq!(log.take(&peer, 1, 42), None);

        let log = DmSentLog::load(settings);
        assert_eq!(log.take(&peer, 1, ms), None, "consumed entries stay consumed");
        assert_eq!(log.take(&peer, 2, (server_ts + 5_000_000) / 1_000_000), Some(now));

        // The copy for our own other device is recorded under ourselves.
        let me = UserId::new();
        let sync = crate::services::message::dm::sync_sent_copy(&peer, &text("hi", now)).unwrap();
        log.record(me, &sync, &[(3, server_ts)]);
        assert_eq!(log.take(&me, 3, ms), Some(now));
    }

    #[test]
    fn the_log_is_bounded() {
        let (_dir, settings) = settings();
        let peer = UserId::new();
        let now = TimestampNanos::now().as_u64();
        let log = DmSentLog::load(settings);
        for i in 0..=MAX_ENTRIES as u64 {
            log.record(peer, &text("m", now + i), &[(1, (i + 1) * 1_000_000)]);
        }
        assert_eq!(log.entries.lock().len(), MAX_ENTRIES);
        assert_eq!(log.take(&peer, 1, 1), None, "the oldest deposit was dropped");
        assert_eq!(log.take(&peer, 1, 2), Some(now + 1));
    }

    /// Bob could not decrypt Alice's DM; Alice's resend carries the message
    /// as she sent it, and only to the peer she sent it to.
    #[tokio::test]
    async fn a_stored_dm_is_rebuilt_for_the_peer_it_was_sent_to() {
        use crate::client::ChatterClient;

        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let alice = ChatterClient::builder()
            .server_url("http://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let (alice_id, bob_id) = (UserId::new(), UserId::new());
        let m = alice.messages();
        m.settings_repo.set(keys::CURRENT_USER_ID, &alice_id.to_string()).unwrap();
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();

        let sent_at = TimestampNanos::now().as_u64();
        m.message_repo
            .insert_outgoing(&Message::new_text(
                bob_id.into(),
                ConversationType::ServerDm,
                Some(alice_id),
                "did this arrive?".into(),
                sent_at,
            ))
            .unwrap();

        let content = m.dm_resend_content(&bob_id, sent_at).unwrap().expect("the message Alice sent Bob");
        let Some(content::Content::DataMessage(dm)) = content.content else {
            panic!("resend is not a DataMessage");
        };
        assert_eq!(dm.body, "did this arrive?");
        assert_eq!(dm.timestamp, sent_at, "filed by the recipient where the lost copy would have been");

        assert!(m.dm_resend_content(&UserId::new(), sent_at).unwrap().is_none(), "not theirs to ask for");
        assert!(m.dm_resend_content(&bob_id, sent_at + 1).unwrap().is_none());

        // Another of Alice's devices gets the copy it files under Bob.
        let copy = m.dm_resend_content(&alice_id, sent_at).unwrap().expect("Alice's own copy");
        let Some(content::Content::SyncSent(sync)) = copy.content else {
            panic!("an own-device resend is a SyncSent");
        };
        assert_eq!(sync.destination_user_id, bob_id.to_string());
        assert_eq!(sync.timestamp, sent_at);
        assert_eq!(sync.message.unwrap().body, "did this arrive?");
    }
}
