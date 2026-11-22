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

    fn forget(&self, channel_id: &ChannelId) {
        if let Ok(mut map) = self.0.lock() {
            map.remove(channel_id);
        }
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
            // The server issued for a newer membership: record it as a peer
            // ahead so the sync below actually fetches the changes (a bare
            // sync only acts on a revision some receive path recorded).
            if resp.revision > verified.state.revision {
                self.group_gate().cross_check(channel_id, false, resp.revision, &[]);
            }
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
        distribution: &chatter_proto::content_v1::SenderKeyDistribution,
    ) -> Result<chat_v1::Message, MessageError> {
        let channel_id: ChannelId = request.channel_id.parse().map_err(invalid)?;
        let verified = self.group_gate().for_distribution(&channel_id)?;
        let me = self.current_user_id()?.to_string();
        if verified
            .state
            .members
            .get(&me)
            .is_some_and(|m| m.is_muted(crate::inbox::ack_policy::now_ns()))
        {
            return Err(MessageError::Muted);
        }
        let contents = match request.content.as_ref().and_then(|c| c.r#type.as_ref()) {
            Some(chat_v1::message::content::Type::Encrypted(e)) => e.ciphertext.clone(),
            _ => return Err(invalid("a private-channel send must carry sender-key content")),
        };
        check_distribution(distribution, &contents)?;
        let members = active_members(&verified);
        let mut auth_client = self.auth_client();
        // The server requires the complete active roster. Discover every
        // member's sessions, but a pending key never waits on a KT RPC here.
        let mut blocked = None;
        for member in &members {
            match self.signal_service.require_group_device_sessions(&mut auth_client, member).await
                .map_err(|error| MessageError::from_signal_send(error, "group session discovery"))
            {
                Ok(devices) if devices.is_empty() && member != &me => {
                    return Err(MessageError::Network(NetworkError::Unavailable));
                }
                Ok(_) => {}
                Err(error @ MessageError::RecipientKeyVerifying { .. }) => blocked = Some(error),
                Err(error) => return Err(error),
            }
        }
        if let Some(error) = blocked {
            return Err(error);
        }
        let token = self.group_send_token(&channel_id, &verified).await?;
        let current = self.create_sender_key_for_channel(&channel_id).await?;
        let same_next = sender_key_precedes(distribution, &current);
        let holders = if same_next {
            self.signal_service.sender_key_holders(&channel_id)
                .map_err(|e| MessageError::from_signal_send(e, "sender key holders"))?
        } else {
            Default::default()
        };
        let own_device = self.current_device_id_or_zero();
        let mut message_client = self.message_client();
        let mut distributed = std::collections::HashSet::new();
        for round in 0..=MAX_RECONCILE_ROUNDS {
            for member in &members {
                let devices = self.signal_service.session_device_registrations(member).await
                    .map_err(|e| MessageError::from_signal_send(e, "group sessions"))?;
                let missing: Vec<_> = devices.into_iter().filter(|(device, registration)| {
                    (member != &me || *device != own_device)
                        && !distributed.contains(&(member.clone(), *device))
                        && !registration.is_some_and(|r| holders.contains(&(member.clone(), *device, r)))
                }).collect();
                if missing.is_empty() {
                    continue;
                }
                let plaintext = self.skdm_plaintext(&channel_id, &verified, distribution, member);
                let outcome = self.deposit_sealed_to_all_devices(
                    &mut auth_client, &mut message_client, member, &plaintext, Some(channel_id.as_bytes()),
                ).await;
                if let Some(error) = outcome.error {
                    return Err(error);
                }
                if same_next {
                    self.record_sender_key_holders(&channel_id, member, &outcome);
                }
                for (device, _) in outcome.deposits {
                    distributed.insert((member.clone(), device));
                }
                if missing.iter().any(|(device, _)| !distributed.contains(&(member.clone(), *device))) {
                    return Err(MessageError::Network(NetworkError::Unavailable));
                }
            }
            let (sealed, destinations) = self
                .signal_service
                .seal_group_message(&mut auth_client, &channel_id, contents.clone(), &members)
                .await
                .map_err(|e| MessageError::from_signal_send(e, "group seal"))?;
            crate::services::gateway::submission::mark_submission_started()?;
            let resp = self
                .grpc_pool
                .anonymous_message_client()
                .send_group_message(&request.channel_id, token.clone(), sealed, &request.client_request_id)
                .await;
            // The server refuses every unverifiable send the same way (a
            // token for a membership that has since changed included): drop
            // the cached token so the retry fetches endorsements for the
            // current state, which also starts a catch-up when we are behind.
            if resp.is_err() {
                self.group_send_tokens.forget(&channel_id);
            }
            let resp = resp?;
            if resp.success {
                use prost::Message as _;
                let original = chatter_proto::content_v1::GroupEncrypted::decode(contents.as_slice())
                    .map_err(invalid)?;
                self.snapshot_private_receipt_targets(channel_id, original.timestamp, &destinations)?;
                return self.sent_group_message(request, resp.server_timestamp);
            }
            if resp.mismatched.is_empty() || round == MAX_RECONCILE_ROUNDS {
                return Err(invalid("members' devices still mismatched after reconciling"));
            }
            for account in &resp.mismatched {
                distributed.retain(|(member, _)| member != &account.user_id);
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

fn distribution_position(
    distribution: &chatter_proto::content_v1::SenderKeyDistribution,
) -> Option<(u32, u32)> {
    let key = libsignal_protocol::SenderKeyDistributionMessage::try_from(
        distribution.distribution_message.as_slice()).ok()?;
    Some((key.chain_id().ok()?, key.iteration().ok()?))
}

fn sender_key_precedes(
    distribution: &chatter_proto::content_v1::SenderKeyDistribution,
    current: &chatter_proto::content_v1::SenderKeyDistribution,
) -> bool {
    match (distribution_position(distribution), distribution_position(current)) {
        (Some((id, before)), Some((next_id, after))) =>
            id == next_id && before.checked_add(1) == Some(after),
        _ => false,
    }
}

fn check_distribution(
    distribution: &chatter_proto::content_v1::SenderKeyDistribution,
    contents: &[u8],
) -> Result<(), MessageError> {
    use prost::Message as _;
    let envelope = chatter_proto::content_v1::GroupEncrypted::decode(contents).map_err(invalid)?;
    let post = libsignal_protocol::SenderKeyMessage::try_from(envelope.ciphertext.as_slice())
        .map_err(invalid)?;
    if distribution_position(distribution).is_some_and(|(chain, iteration)|
        chain == post.chain_id() && iteration <= post.iteration())
    {
        Ok(())
    } else {
        Err(MessageError::InvalidContent("Private post sender key changed during preparation".into()))
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
