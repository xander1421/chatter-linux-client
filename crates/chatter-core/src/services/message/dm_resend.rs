//! Sender half of Signal's 1:1 retry: a friend's device could not decrypt a
//! DM we sent and said so with a `DecryptionErrorMessage`; send it again.
//!
//! Signal-Desktop `ts/util/handleRetry.preload.ts` `onRetryRequest`
//! (:172-260): archive the session when the report's ratchet key matches our
//! current one (`archiveSessionOnMatch` — here the gateway's
//! `handle_decryption_error_report`, which runs first), then look the exact
//! sent `Content` up in the message send log and send those bytes again to
//! the requesting device, or — too old, or not found — a `NullMessage`, so
//! the session the report broke is rebuilt either way
//! (`sendDistributionMessageOrNullMessage`).
//!
//! The timestamp a report names here is the SERVER's for the failed envelope
//! (`DeviceMessage.server_timestamp`): the receiver could not open the
//! content, so it never saw the message's own. The same value comes back to
//! the sender in `SendSealedMessageResponse.server_timestamp`, and the send
//! log ([`SendLogRepository`]) is keyed by it per (peer, device).

use prost::Message as _;

use super::{MessageError, MessageService};
use crate::domain::ids::UserId;
use crate::proto::content_v1::{content, Content, DataMessage};
use crate::services::current_user::CurrentUserProvider;
use crate::storage::repositories::send_log_repo::SendLogRepository;
use crate::util::time::TimestampNanos;

/// How long a sent `Content` is resent on request: Signal-Desktop's
/// `retryRespondMaxAge` default (`handleRetry.preload.ts:172`, 14 days). Our
/// server queues a message for 30 days, so a device offline longer than this
/// gets a `NullMessage` instead — its session still heals, the message is
/// not repeated.
pub(crate) const RESEND_MAX_AGE_NS: u64 = 14 * 24 * 60 * 60 * 1_000_000_000;

/// What a retry request is answered with.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Resend {
    /// The bytes we sent, exactly.
    Original(Vec<u8>),
    /// Nothing we may send again: a `NullMessage`, to heal the session.
    NullMessage(Vec<u8>),
}

/// Whether `content` goes in the send log, and the `sent_at` of our own
/// stored message it carries or edits (checked again before a resend, so a
/// message deleted or expired since is not repeated). Signal logs every send
/// but typing, calling and retries (`handleMessageSend.preload.ts:93`
/// `shouldSaveProto`); the session-heal `NullMessage` is a retry here.
fn loggable(content: &Content) -> Option<Option<u64>> {
    let has_payload = |dm: &DataMessage| !dm.body.is_empty() || !dm.attachments.is_empty();
    match content.content.as_ref()? {
        content::Content::Typing(_)
        | content::Content::NullMessage(_)
        | content::Content::CallKey(_)
        | content::Content::CallHangup(_)
        | content::Content::GroupCallSignal(_)
        | content::Content::AnonCallOffer(_)
        | content::Content::AnonCallAnswer(_)
        | content::Content::AnonCallHangup(_) => None,
        content::Content::DataMessage(dm) => Some(has_payload(dm).then_some(dm.timestamp)),
        content::Content::SyncSent(sync) => {
            Some(sync.message.as_ref().is_some_and(has_payload).then_some(sync.timestamp))
        }
        content::Content::Edit(edit) => Some(Some(edit.target_sent_timestamp)),
        _ => Some(None),
    }
}

/// When a logged `content` stops being resendable: the resend window, or
/// sooner for a disappearing message — its plaintext must not outlive it here.
fn expires_at(content: &Content, now: u64) -> u64 {
    let dm = match content.content.as_ref() {
        Some(content::Content::DataMessage(dm)) => Some(dm),
        Some(content::Content::SyncSent(sync)) => sync.message.as_ref(),
        Some(content::Content::Edit(edit)) => edit.data_message.as_ref(),
        _ => None,
    };
    let ttl_ns = dm
        .filter(|dm| dm.ephemeral && dm.ephemeral_ttl_seconds > 0)
        .map(|dm| u64::from(dm.ephemeral_ttl_seconds) * 1_000_000_000);
    now.saturating_add(ttl_ns.map_or(RESEND_MAX_AGE_NS, |ttl| ttl.min(RESEND_MAX_AGE_NS)))
}

impl MessageService {
    fn send_log(&self) -> SendLogRepository {
        SendLogRepository::new(self.database.clone())
    }

    /// Log `content` as deposited for `peer` (ourselves, for the copy our
    /// other devices get): `deposits` is `(device, server timestamp in epoch
    /// nanos)` per device reached. The bytes logged are the ones every one of
    /// those devices was sent (`encode_to_vec` of this same `content`).
    pub(crate) fn record_dm_send(&self, peer: UserId, content: &Content, deposits: &[(u32, u64)]) {
        let Some(message_ts) = loggable(content) else { return };
        if deposits.is_empty() {
            return;
        }
        let now = TimestampNanos::now().as_u64();
        if let Err(e) = self.send_log().record(
            &peer,
            &content.encode_to_vec(),
            message_ts,
            now,
            expires_at(content, now),
            deposits,
        ) {
            tracing::warn!(%peer, error = %e, "Could not log a DM send; a retry request for it gets a NullMessage");
        }
    }

    /// Answer `peer_id`'s retry request for the DM we deposited for its
    /// device at `timestamp_ms` (server time, epoch millis): send the logged
    /// bytes again, or a `NullMessage`, to that one device, on the session
    /// the next encrypt establishes (the report may just have archived ours).
    pub async fn resend_dm_on_decryption_error(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    ) {
        match self.resend_dm(peer_id, peer_device_id, timestamp_ms).await {
            Ok(Some(Resend::Original(_))) => tracing::info!(%peer_id, peer_device_id, timestamp_ms,
                "Resent a DM after the peer's DecryptionErrorMessage"),
            Ok(Some(Resend::NullMessage(_))) => tracing::info!(%peer_id, peer_device_id, timestamp_ms,
                "DM retry request matched nothing we may resend; sent a NullMessage"),
            Ok(None) => tracing::info!(%peer_id, peer_device_id, timestamp_ms,
                "DM retry request not answered in Shield mode"),
            Err(e) => tracing::warn!(%peer_id, peer_device_id, timestamp_ms, error = %e,
                "Failed to answer a DM retry request"),
        }
    }

    async fn resend_dm(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    ) -> Result<Option<Resend>, MessageError> {
        // This transport names the recipient to the server, which Shield mode
        // must never do (`send_sealed_dm_content`). Checked before the log
        // entry is consumed.
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(None);
        }
        let resend = self.dm_resend_plaintext(peer_id, peer_device_id, timestamp_ms)?;
        let plaintext = match &resend {
            Resend::Original(bytes) | Resend::NullMessage(bytes) => bytes,
        };
        let sealed_body = self
            .signal_service
            .encrypt_message(&mut self.auth_client(), &peer_id.to_string(), peer_device_id, plaintext)
            .await
            .map_err(|e| match e {
                crate::error::ServiceError::Network(e) => MessageError::Network(e),
                e => MessageError::EncryptionFailed(e.to_string()),
            })?;
        if *peer_id == self.settings_repo.current_user_id()? {
            // Identified on the authenticated channel, as every own-device
            // copy is (`sync_to_own_devices`).
            let ciphertexts = std::collections::HashMap::from([(peer_device_id, sealed_body)]);
            let outcome = self
                .deposit_sealed_ciphertexts(&mut self.message_client(), &peer_id.to_string(), ciphertexts, None)
                .await;
            if outcome.delivered == 0 {
                return Err(outcome
                    .error
                    .unwrap_or_else(|| MessageError::DmError("own-device resend reached no device".into())));
            }
        } else {
            self.deposit_sealed_to_device(peer_id, peer_device_id, sealed_body).await?;
        }
        Ok(Some(resend))
    }

    /// The plaintext that answers `requester`'s report about our deposit for
    /// its `device` at `server_ts_ms`: the logged bytes, consumed, while the
    /// message they carry or edit is still ours to show (not deleted, not
    /// disappeared); a `NullMessage` otherwise.
    pub(super) fn dm_resend_plaintext(
        &self,
        requester: &UserId,
        device: u32,
        server_ts_ms: u64,
    ) -> Result<Resend, MessageError> {
        let now = TimestampNanos::now().as_u64();
        let null = || Resend::NullMessage(super::null_message_content().encode_to_vec());
        let Some(payload) = self.send_log().take(requester, device, server_ts_ms, now)? else {
            return Ok(null());
        };
        if let Some(sent_at) = payload.message_ts {
            let us = self.settings_repo.current_user_id()?;
            let still_ours = self
                .message_repo
                .get_by_sender_timestamp(&us, sent_at)?
                .is_some_and(|m| !m.is_deleted);
            if !still_ours {
                return Ok(null());
            }
        }
        Ok(Resend::Original(payload.content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::{ConversationType, Message, MessageContent};
    use crate::proto::content_v1::Reaction;
    use crate::storage::repositories::settings_repo::keys;

    fn client() -> (tempfile::TempDir, std::sync::Arc<crate::client::ChatterClient>, UserId) {
        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let dir = tempfile::tempdir().unwrap();
        let client = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(dir.path().to_str().unwrap())
            .build()
            .expect("build client");
        let me = UserId::new();
        client.messages().settings_repo.set(keys::CURRENT_USER_ID, &me.to_string()).unwrap();
        client.messages().settings_repo.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
        (dir, client, me)
    }

    fn is_null_message(resend: &Resend) -> bool {
        let Resend::NullMessage(bytes) = resend else { return false };
        matches!(Content::decode(bytes.as_slice()).unwrap().content, Some(content::Content::NullMessage(_)))
    }

    /// Bob could not decrypt Alice's DM; Alice has edited it since. The
    /// resend is the bytes Bob was sent, not a rebuild of the edited row.
    #[tokio::test]
    async fn the_resend_is_the_original_bytes_after_a_local_edit() {
        let (_dir, alice, alice_id) = client();
        let m = alice.messages();
        let bob = UserId::new();
        let sent_at = TimestampNanos::now().as_u64();
        let mut message =
            Message::new_text(bob.into(), ConversationType::ServerDm, Some(alice_id), "did this arrive?".into(), sent_at);
        m.message_repo.insert_outgoing(&message).unwrap();
        let mut content = crate::services::build_data_message_content("did this arrive?", vec![], sent_at);
        MessageService::pad_content_to_bucket(&mut content);
        let server_ts = sent_at + 5_000_000;
        m.record_dm_send(bob, &content, &[(1, server_ts), (2, server_ts + 1_000_000)]);

        message.content = MessageContent::Text("edited".into());
        m.message_repo.update(&message).unwrap();

        let ms = server_ts / 1_000_000;
        assert!(is_null_message(&m.dm_resend_plaintext(&UserId::new(), 1, ms).unwrap()), "not theirs to ask for");
        assert!(is_null_message(&m.dm_resend_plaintext(&bob, 2, ms).unwrap()), "device 2's deposit has its own timestamp");
        assert_eq!(m.dm_resend_plaintext(&bob, 1, ms).unwrap(), Resend::Original(content.encode_to_vec()));
        assert!(is_null_message(&m.dm_resend_plaintext(&bob, 1, ms).unwrap()), "one resend per deposit");

        // Deleted for everyone since: device 2 gets a NullMessage, not the text.
        m.message_repo.delete(&message.id).unwrap();
        assert!(is_null_message(&m.dm_resend_plaintext(&bob, 2, ms + 1).unwrap()));
    }

    /// A reaction has no stored row to rebuild from; the send log still has
    /// its bytes. So does the copy our own other device was sent.
    #[tokio::test]
    async fn a_reaction_and_an_own_device_copy_are_resent() {
        let (_dir, alice, alice_id) = client();
        let m = alice.messages();
        let bob = UserId::new();
        let now = TimestampNanos::now().as_u64();
        let reaction = Content {
            timestamp: now,
            content: Some(content::Content::Reaction(Reaction {
                emoji: "+1".into(),
                target_author_aci: bob.to_string(),
                target_sent_timestamp: now - 1,
                timestamp: now,
                ..Default::default()
            })),
            ..Default::default()
        };
        m.record_dm_send(bob, &reaction, &[(1, 9_000_000)]);
        m.record_dm_send(alice_id, &reaction, &[(2, 9_000_000)]);
        assert_eq!(m.dm_resend_plaintext(&bob, 1, 9).unwrap(), Resend::Original(reaction.encode_to_vec()));
        assert_eq!(m.dm_resend_plaintext(&alice_id, 2, 9).unwrap(), Resend::Original(reaction.encode_to_vec()));

        // Typing is never logged.
        let typing = Content {
            content: Some(content::Content::Typing(Default::default())),
            ..Default::default()
        };
        m.record_dm_send(bob, &typing, &[(1, 10_000_000)]);
        assert!(is_null_message(&m.dm_resend_plaintext(&bob, 1, 10).unwrap()));
    }

    #[tokio::test]
    async fn a_miss_is_answered_with_a_null_message() {
        let (_dir, alice, _) = client();
        let resend = alice.messages().dm_resend_plaintext(&UserId::new(), 1, 42).unwrap();
        assert!(is_null_message(&resend));
    }

    #[test]
    fn a_disappearing_message_leaves_the_log_with_its_timer() {
        let mut dm = crate::services::build_data_message_content("poof", vec![], 1);
        assert_eq!(expires_at(&dm, 0), RESEND_MAX_AGE_NS);
        if let Some(content::Content::DataMessage(d)) = dm.content.as_mut() {
            d.ephemeral = true;
            d.ephemeral_ttl_seconds = 30;
        }
        assert_eq!(expires_at(&dm, 0), 30_000_000_000);
    }
}
