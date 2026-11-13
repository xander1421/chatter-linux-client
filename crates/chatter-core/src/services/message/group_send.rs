//! Anonymous sends to private channels (owner decision 2026-11-13, OPEN-ITEMS
//! §914): the sender-key ciphertext a channel send already prepared is sealed
//! once for every device of every ACTIVE member of our verified state, our own
//! devices included, as a Sealed Sender v2 multi-recipient message
//! (crypto/signal/sealed_sender/multi.rs) and sent with NO credentials,
//! authorized by a group send token (crypto/group_send.rs). The server queues
//! it to each device inbox and never learns which member sent it. Signal's
//! group send: libsignal sealed_sender_multi_recipient_encrypt +
//! Signal-Server PUT /v1/messages/multi_recipient with Group-Send-Token.
//!
//! The token is fetched once per verified membership state and cached, as
//! Signal fetches endorsements with a group change rather than per send:
//! fetching per send would pair an authenticated request with each anonymous
//! one.

use std::collections::HashMap;
use std::sync::Mutex;

use chatter_proto::chat_v1;

use crate::crypto::group_send;
use crate::domain::ids::ChannelId;
use crate::domain::verified_state::{StateHash, VerifiedChannel};
use crate::error::NetworkError;

use super::{MessageError, MessageService};

/// Refetch this long before a token expires (libsignal's client refuses an
/// issuance with less than two hours left).
const REFRESH_MARGIN_SECS: u64 = 3 * 3600;

/// Device reconciliations before a send gives up.
const MAX_RECONCILE_ROUNDS: usize = 2;

#[derive(Clone)]
struct Cached {
    state_hash: StateHash,
    expiration: u64,
    token: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct TokenCache(Mutex<HashMap<ChannelId, Cached>>);

impl TokenCache {
    fn get(&self, channel_id: &ChannelId, state_hash: &StateHash, now: u64) -> Option<Vec<u8>> {
        let map = self.0.lock().ok()?;
        map.get(channel_id)
            .filter(|c| c.state_hash == *state_hash && c.expiration > now + REFRESH_MARGIN_SECS)
            .map(|c| c.token.clone())
    }

    fn put(&self, channel_id: &ChannelId, state_hash: StateHash, expiration: u64, token: Vec<u8>) {
        if let Ok(mut map) = self.0.lock() {
            map.insert(*channel_id, Cached { state_hash, expiration, token });
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn invalid(e: impl std::fmt::Display) -> MessageError {
    MessageError::EncryptionFailed(e.to_string())
}

/// The ACTIVE members of the verified state: the set the server requires a
/// private-channel send to address, exactly.
fn active_members(verified: &VerifiedChannel) -> Vec<String> {
    verified.state.active_members().map(|(id, _)| id.clone()).collect()
}

impl MessageService {
    /// Whether `channel_id` is a private channel, whose messages have no
    /// server copy: they reach members anonymously (this file), so there is
    /// nothing on the server to fetch, delete or pin.
    pub(crate) fn is_private_channel(&self, channel_id: &ChannelId) -> bool {
        self.channel_repo.get(channel_id).ok().flatten().is_some_and(|c| {
            c.channel_type == crate::domain::channel::ChannelType::Private
        })
    }

    /// The serialized `GroupSendFullToken` for our verified state of
    /// `channel_id`. The issuance must be for exactly our verified revision;
    /// a server ahead of us starts a catch-up and fails this send (retryable).
    async fn group_send_token(&self, channel_id: &ChannelId, verified: &VerifiedChannel) -> Result<Vec<u8>, MessageError> {
        let now = now_secs();
        if let Some(token) = self.group_send_tokens.get(channel_id, &verified.state_hash, now) {
            return Ok(token);
        }
        let resp = self
            .channel_client()
            .get_group_send_endorsements(&channel_id.to_string())
            .await?;
        if resp.revision != verified.state.revision {
            self.sync_channel_state_if_behind(channel_id);
            return Err(MessageError::Network(NetworkError::Unavailable));
        }
        let members: Vec<uuid::Uuid> = active_members(verified)
            .iter()
            .map(|id| uuid::Uuid::parse_str(id))
            .collect::<Result<_, _>>()
            .map_err(invalid)?;
        let server = group_send::pinned_server_params().map_err(invalid)?;
        let expiration = group_send::expiration(&resp.endorsements).map_err(invalid)?;
        let token = group_send::full_token(
            &resp.endorsements,
            &channel_id.to_string(),
            &verified.state_hash,
            &members,
            now,
            &server,
        )
        .map_err(invalid)?;
        self.group_send_tokens.put(channel_id, verified.state_hash, expiration, token.clone());
        Ok(token)
    }

    /// Send a prepared private-channel request anonymously and return the
    /// `Message` the channel journal records (what `SendMessage` would have
    /// answered: our id, our device, the server's timestamp). The request's
    /// `client_request_id` is the queue GUID, so a replay queues nothing twice.
    pub(super) async fn submit_group_request(
        &self,
        request: &chat_v1::SendMessageRequest,
    ) -> Result<chat_v1::Message, MessageError> {
        let channel_id: ChannelId = request.channel_id.parse().map_err(invalid)?;
        let verified = self.group_gate().for_distribution(&channel_id)?;
        let contents = match request.content.as_ref().and_then(|c| c.r#type.as_ref()) {
            Some(chat_v1::message::content::Type::Encrypted(e)) => e.ciphertext.clone(),
            _ => return Err(invalid("a private-channel send must carry sender-key content")),
        };
        let token = self.group_send_token(&channel_id, &verified).await?;
        let members = active_members(&verified);
        let mut auth_client = self.auth_client();
        for round in 0..=MAX_RECONCILE_ROUNDS {
            let sealed = self
                .signal_service
                .seal_group_message(&mut auth_client, &channel_id, contents.clone(), &members)
                .await
                .map_err(|e| MessageError::from_signal_send(e, "group seal"))?;
            crate::services::gateway::submission::mark_submission_started()?;
            let resp = self
                .grpc_pool
                .anonymous_message_client()
                .send_group_message(&request.channel_id, token.clone(), sealed, &request.client_request_id)
                .await?;
            if resp.success {
                return Ok(self.sent_group_message(request, resp.server_timestamp)?);
            }
            if resp.mismatched.is_empty() || round == MAX_RECONCILE_ROUNDS {
                return Err(invalid("members' devices still mismatched after reconciling"));
            }
            for account in &resp.mismatched {
                let d = account.devices.clone().unwrap_or_default();
                self.signal_service
                    .reconcile_devices(&mut auth_client, &account.user_id, &d.missing_devices, &d.extra_devices, &d.stale_devices)
                    .await
                    .map_err(|e| MessageError::from_signal_send(e, "device reconciliation"))?;
            }
        }
        unreachable!("the loop returns on its last round")
    }

    /// The journal's record of an accepted anonymous send.
    fn sent_group_message(&self, request: &chat_v1::SendMessageRequest, server_timestamp: u64) -> Result<chat_v1::Message, MessageError> {
        Ok(chat_v1::Message {
            id: request.client_request_id.clone(),
            channel_id: request.channel_id.clone(),
            author_id: self.current_user_id()?.to_string(),
            author_device_id: self
                .settings_repo
                .get::<u32>(crate::storage::repositories::settings_repo::keys::CURRENT_DEVICE_ID)?
                .unwrap_or_default(),
            parent_id: request.parent_id.clone(),
            content: request.content.clone(),
            timestamp: server_timestamp,
            updated_at: server_timestamp,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_is_per_state_and_refreshes_before_expiry() {
        let cache = TokenCache::default();
        let ch = ChannelId::new();
        let (h1, h2) = ([1u8; 32], [2u8; 32]);
        cache.put(&ch, h1, 100_000, b"t".to_vec());
        assert_eq!(cache.get(&ch, &h1, 1_000), Some(b"t".to_vec()));
        assert_eq!(cache.get(&ch, &h2, 1_000), None, "a new verified state needs a new token");
        assert_eq!(cache.get(&ch, &h1, 100_000 - REFRESH_MARGIN_SECS), None, "refetched before expiry");
        assert_eq!(cache.get(&ChannelId::new(), &h1, 1_000), None);
    }
}
