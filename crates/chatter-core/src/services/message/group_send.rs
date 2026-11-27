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
//! A session maintenance loop fetches endorsements for active private
//! channels. Posts use its cache, so a server refusing an anonymous submit
//! cannot make its sender request endorsements on the account connection.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use chatter_proto::chat_v1;

use crate::crypto::group_send;
use crate::domain::ids::ChannelId;
use crate::domain::verified_state::{StateHash, VerifiedChannel};
use crate::error::NetworkError;
use crate::mailbox::token_manager::{MailboxToken, TokenManager};
use crate::services::signal::PreKeySource;

use super::{MessageError, MessageService};

/// Refetch this long before a token expires (libsignal's client refuses an
/// issuance with less than two hours left).
const REFRESH_MARGIN_SECS: u64 = 3 * 3600;

/// Device reconciliations before a send gives up.
const MAX_RECONCILE_ROUNDS: usize = 2;

const MAX_DEVICES_PER_MISMATCH: usize = 8;
const MAX_REPAIRS_PER_POST: usize = 12;
// Keep enough credentials for a secret-chat start or a mailbox rotation.
pub(super) const RESERVED_MAILBOX_CREDENTIALS: usize = 4;

struct PostBudget {
    channel: ChannelId,
    token_limit: usize,
    tokens: Cell<usize>,
    repairs: Cell<usize>,
}
tokio::task_local! { static PRIVATE_POST_BUDGET: PostBudget; }

fn budget_exhausted() -> NetworkError {
    NetworkError::InvalidRequest("Private-channel send exceeded its discovery budget".into())
}

pub(super) fn claim_private_post_credential(tokens: &TokenManager) -> Result<MailboxToken, NetworkError> {
    PRIVATE_POST_BUDGET.try_with(|budget| {
        if budget.tokens.get() >= budget.token_limit {
            return Err(budget_exhausted());
        }
        let token = tokens.claim_budgeted_credential(RESERVED_MAILBOX_CREDENTIALS, |conn| {
            super::channel_send::debit_private_send(Some(conn), "$.private_tokens", 1, budget.token_limit)?;
            Ok(())
        })?;
        budget.tokens.set(budget.tokens.get() + 1);
        Ok(token)
    }).map_err(|_| budget_exhausted())?
}

pub(super) fn private_post_channel_id() -> Result<ChannelId, NetworkError> {
    PRIVATE_POST_BUDGET
        .try_with(|post| post.channel)
        .map_err(|_| budget_exhausted())
}

pub(super) fn charge_private_repairs(count: usize) -> Result<(), NetworkError> {
    if count == 0 || count > MAX_DEVICES_PER_MISMATCH {
        return Err(budget_exhausted());
    }
    PRIVATE_POST_BUDGET.try_with(|budget| {
        let total = budget.repairs.get() + count;
        if total > MAX_REPAIRS_PER_POST {
            return Err(budget_exhausted());
        }
        super::channel_send::debit_private_send(None, "$.private_repairs", count, MAX_REPAIRS_PER_POST)?;
        budget.repairs.set(total);
        Ok(())
    }).map_err(|_| budget_exhausted())?
}

#[derive(Clone)]
struct Cached {
    state_hash: StateHash,
    expiration: u64,
    checked_at: u64,
    token: Vec<u8>,
    /// The serialized endorsement response the token was combined from, so a
    /// send that omits members can combine a token for the rest.
    endorsements: Vec<u8>,
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

    fn recently_checked(&self, channel_id: &ChannelId, state_hash: &StateHash, now: u64) -> bool {
        const CHECK_INTERVAL_SECS: u64 = 15 * 60;
        self.0.lock().ok().is_some_and(|map| map.get(channel_id).is_some_and(|c| {
            c.state_hash == *state_hash
                && c.expiration > now + REFRESH_MARGIN_SECS
                && now.saturating_sub(c.checked_at) < CHECK_INTERVAL_SECS
        }))
    }

    pub(super) fn put(&self, channel_id: &ChannelId, state_hash: StateHash, expiration: u64, token: Vec<u8>) {
        self.put_endorsed(channel_id, state_hash, expiration, token, Vec::new());
    }

    fn put_endorsed(
        &self, channel_id: &ChannelId, state_hash: StateHash, expiration: u64, token: Vec<u8>, endorsements: Vec<u8>,
    ) {
        if let Ok(mut map) = self.0.lock() {
            map.insert(*channel_id, Cached { state_hash, expiration, checked_at: now_secs(), token, endorsements });
        }
    }

    fn endorsements(&self, channel_id: &ChannelId, state_hash: &StateHash, now: u64) -> Option<Vec<u8>> {
        let map = self.0.lock().ok()?;
        map.get(channel_id)
            .filter(|c| c.state_hash == *state_hash && c.expiration > now + REFRESH_MARGIN_SECS)
            .map(|c| c.endorsements.clone())
            .filter(|e| !e.is_empty())
    }
}

/// Strict discoveries that found no prekey bundle for a member, or the time
/// since the first, before a post leaves that member out (server
/// `group_send.go`: a send may omit exactly the members without one).
const NO_BUNDLE_MISSES: u32 = 3;
const NO_BUNDLE_SECS: u64 = 10 * 60;

#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
struct NoBundle {
    first_at: u64,
    misses: u32,
    /// The channel was told this member is left out.
    noticed: bool,
}

impl NoBundle {
    fn omitted(self, now: u64) -> bool {
        self.misses >= NO_BUNDLE_MISSES
            || (self.misses > 0 && now.saturating_sub(self.first_at) >= NO_BUNDLE_SECS)
    }

    fn missed(self, now: u64) -> Self {
        Self {
            first_at: if self.misses == 0 { now } else { self.first_at },
            misses: self.misses.saturating_add(1),
            noticed: self.noticed,
        }
    }
}

fn no_bundle_key(channel_id: &ChannelId, member: &str) -> String {
    format!("private.no-bundle:{channel_id}:{member}")
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
    pub(super) async fn reconcile_private_post_devices(
        &self,
        member: &str,
        devices: &chat_v1::MismatchedDevices,
    ) -> Result<(), crate::error::ServiceError> {
        self.signal_service.reconcile_private_post_devices(
            &mut self.private_post_prekeys(), member, devices,
        ).await
    }

    async fn private_post_devices(&self, member: &str, owner: &str) -> Result<Vec<(u32, Option<u32>)>, MessageError> {
        let mut devices = self.signal_service.session_device_registrations(member).await
            .map_err(|e| MessageError::from_signal_send(e, "group sessions"))?;
        if member == owner {
            let local = self.current_device_id_or_zero();
            devices.retain(|(device, _)| *device != local);
            if let Some(registration) = self.signal_service.private_echo_registration()
                .map_err(|e| MessageError::from_signal_send(e, "private echo registration"))? {
                devices.push((local, Some(registration)));
            }
        }
        Ok(devices)
    }

    /// Whether `channel_id` is a private channel, whose messages have no
    /// server copy: they reach members anonymously (this file), so there is
    /// nothing on the server to fetch, delete or pin.
    pub(crate) fn is_private_channel(&self, channel_id: &ChannelId) -> bool {
        self.channel_repo.routing_type(channel_id).ok().flatten()
            == Some(crate::domain::channel::ChannelType::Private)
    }

    async fn refresh_group_send_token(
        &self,
        channel_id: &ChannelId,
        verified: &VerifiedChannel,
    ) -> Result<(), MessageError> {
        let now = now_secs();
        if self.group_send_tokens.recently_checked(channel_id, &verified.state_hash, now) {
            return Ok(());
        }
        crate::state::send_session::check_send_session()?;
        let resp = self
            .channel_client()
            .get_group_send_endorsements(&channel_id.to_string())
            .await?;
        crate::state::send_session::check_send_session()?;
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
        crate::state::send_session::check_send_session()?;
        self.group_send_tokens.put_endorsed(channel_id, verified.state_hash, expiration, token, resp.endorsements);
        Ok(())
    }

    fn private_memberships(&self, owner: &crate::domain::UserId) -> Result<Vec<ChannelId>, MessageError> {
        let mut channels = self.channel_repo.list_channels_for_user(owner)?;
        channels.retain(|channel| self.is_private_channel(channel));
        channels.sort_unstable_by_key(|channel| *channel.as_uuid());
        Ok(channels)
    }

    async fn refresh_private_endorsements(&self, owner: crate::domain::UserId) -> Result<(), MessageError> {
        let owner_id = owner.to_string();
        let mut cert_checked = false;
        for channel in self.private_memberships(&owner)? {
            crate::state::send_session::check_send_session()?;
            let Ok(verified) = self.group_gate().for_distribution(&channel) else {
                continue;
            };
            if !verified.state.active_members().any(|(user, member)| {
                user == &owner_id && !member.is_muted(crate::inbox::ack_policy::now_ns())
            }) {
                continue;
            }
            if !cert_checked {
                cert_checked = true;
                if let Err(error) = self.signal_service.get_or_request_sender_certificate(&mut self.auth_client()).await {
                    tracing::debug!(%error, "Private-channel sender certificate unavailable");
                }
            }
            if let Err(error) = self.refresh_group_send_token(&channel, &verified).await {
                tracing::debug!(channel = %channel, %error, "Private-channel endorsements unavailable");
            }
        }
        Ok(())
    }

    pub(super) fn private_post_prekeys(&self) -> PreKeySource<'_> {
        PreKeySource::Anonymous {
            client: self
                .grpc_pool
                .private_post_auth_client(PRIVATE_POST_BUDGET.with(|post| post.channel)),
            tokens: &self.token_manager,
            claim: claim_private_post_credential,
        }
    }

    /// Encrypt channel-scoped controls as sender-key content and send over SSv2.
    pub(super) async fn send_private_channel_control(
        &self,
        channel_id: &ChannelId,
        mut content: chatter_proto::content_v1::Content,
    ) -> Result<(), MessageError> {
        use prost::Message as _;

        let named = content.content.as_ref()
            .and_then(crate::services::gateway::inbound::content_channel)
            .and_then(|id| id.parse::<ChannelId>().ok());
        if named != Some(*channel_id) {
            return Err(MessageError::InvalidContent("Control content must name its channel".into()));
        }
        self.group_gate().for_distribution(channel_id)?;
        let distribution = self.create_sender_key_for_channel(channel_id).await?;
        let sent_at = crate::util::time::TimestampNanos::now().as_u64();
        Self::pad_content_to_bucket(&mut content);
        let (content, rotated) = self.prepare_encrypted_content(
            channel_id,
            &content.encode_to_vec(),
            chat_v1::message::Content::default(),
            true,
            sent_at,
        ).await?;
        let request = chat_v1::SendMessageRequest {
            channel_id: channel_id.to_string(),
            content: Some(content),
            client_request_id: uuid::Uuid::new_v4().to_string(),
            ..Default::default()
        };
        self.submit_private_group(&request, rotated.as_ref().unwrap_or(&distribution), false).await?;
        Ok(())
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
        self.submit_private_group(request, distribution, true).await
    }

    async fn submit_private_group(
        &self,
        request: &chat_v1::SendMessageRequest,
        distribution: &chatter_proto::content_v1::SenderKeyDistribution,
        retain_resend_targets: bool,
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
        if !members.contains(&me) {
            return Err(MessageError::NotAuthor);
        }
        let token = self.group_send_tokens
            .get(&channel_id, &verified.state_hash, now_secs())
            .ok_or(NetworkError::Unavailable)?;
        self.signal_service.get_cached_sender_certificate()
            .map_err(|error| MessageError::from_signal_send(error, "Sender certificate"))?
            .ok_or(NetworkError::Unavailable)?;
        let budget = PostBudget {
            channel: channel_id,
            // A cold member may need two lookups (before and after KT) and
            // one sender-key deposit. Repairs have their own cumulative cap.
            token_limit: 3 * members.len() + 2 * MAX_REPAIRS_PER_POST,
            tokens: Cell::new(0),
            repairs: Cell::new(0),
        };
        PRIVATE_POST_BUDGET.scope(budget, self.submit_verified_group(
            request, distribution, channel_id, verified, me, members, contents, token, retain_resend_targets,
        )).await
    }

    fn require_private_post_balance(&self, planned: usize, repairs_left: usize) -> Result<(), MessageError> {
        if planned == 0 {
            return Ok(());
        }
        let available = self.token_manager.available_token_count()
            .map_err(|error| NetworkError::ServerError(error.to_string()))? as usize;
        if available < planned + 2 * repairs_left + RESERVED_MAILBOX_CREDENTIALS {
            return Err(NetworkError::AnonymousCredentialsUnavailable.into());
        }
        Ok(())
    }

    async fn private_post_holders(
        &self,
        channel_id: &ChannelId,
        distribution: &chatter_proto::content_v1::SenderKeyDistribution,
    ) -> Result<Option<HashSet<(String, u32, u32)>>, MessageError> {
        let current = self.create_sender_key_for_channel(channel_id).await?;
        if !sender_key_precedes(distribution, &current) {
            return Ok(None);
        }
        self.signal_service.sender_key_holders(channel_id)
            .map(Some)
            .map_err(|error| MessageError::from_signal_send(error, "sender key holders"))
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit_verified_group(
        &self,
        request: &chat_v1::SendMessageRequest,
        distribution: &chatter_proto::content_v1::SenderKeyDistribution,
        channel_id: ChannelId,
        verified: VerifiedChannel,
        me: String,
        members: Vec<String>,
        contents: Vec<u8>,
        token: Vec<u8>,
        retain_resend_targets: bool,
    ) -> Result<chat_v1::Message, MessageError> {
        // Do not send a subset of lookups. A cold roster scan covers
        // every member, including accounts whose sessions are already warm.
        let holders = self.private_post_holders(&channel_id, distribution).await?.unwrap_or_default();
        let now = now_secs();
        let mut omitted: HashSet<String> = members.iter()
            .filter(|member| **member != me && self.no_bundle(&channel_id, member).omitted(now))
            .cloned()
            .collect();
        let mut refresh_roster = false;
        let mut fanout = 0;
        for member in &members {
            let devices = self.private_post_devices(member, &me).await?;
            refresh_roster |= devices.is_empty()
                || (member == &me && devices.len() == 1 && devices[0].0 == self.current_device_id_or_zero()
                    && !self.signal_service.has_discovered_private_echo()
                        .map_err(|error| MessageError::from_signal_send(error, "private echo discovery"))?);
            if devices.iter().any(|(device, registration)| {
                !registration.is_some_and(|r| holders.contains(&(member.clone(), *device, r)))
            }) {
                fanout += 1;
            }
        }
        let planned = if refresh_roster { 3 * members.len() } else { fanout };
        self.require_private_post_balance(planned, MAX_REPAIRS_PER_POST)?;
        for member in &members {
            self.signal_service.require_group_device_ready(member)
                .map_err(|error| MessageError::from_signal_send(error, "group session discovery"))?;
        }

        if refresh_roster {
            let mut prekeys = self.private_post_prekeys();
            let mut blocked = None;
            for member in &members {
                match self.signal_service.require_group_device_sessions(&mut prekeys, member).await
                    .map_err(|error| MessageError::from_signal_send(error, "group session discovery"))
                {
                    // No bundle (the server's NotFound, or no device): wait,
                    // then leave the member out.
                    Ok(devices) if devices.is_empty() => {
                        self.missing_bundle(&channel_id, member, &me, now, &mut omitted)?;
                    }
                    Err(MessageError::Network(NetworkError::NotFound(_))) => {
                        self.missing_bundle(&channel_id, member, &me, now, &mut omitted)?;
                    }
                    Ok(_) => {
                        self.settings_repo.delete(&no_bundle_key(&channel_id, member))?;
                        omitted.remove(member);
                    }
                    Err(error @ MessageError::RecipientKeyVerifying { .. }) => blocked = Some(error),
                    Err(error) => return Err(error),
                }
            }
            if let Some(error) = blocked {
                return Err(error);
            }
        }
        // Everyone else is addressed; a token for exactly them.
        let recipients: Vec<String> = members.iter().filter(|m| !omitted.contains(*m)).cloned().collect();
        let token = if omitted.is_empty() {
            token
        } else {
            self.subset_token(&channel_id, &verified, &recipients, now)?
        };
        let mut auth_client = self.auth_client();
        // Discovery can await the network while the local sender key advances.
        let holders = self.private_post_holders(&channel_id, distribution).await?;
        let same_next = holders.is_some();
        let holders = holders.unwrap_or_default();
        let mut message_client = self.message_client();
        let mut distributed = HashSet::new();
        for round in 0..=MAX_RECONCILE_ROUNDS {
            let mut fanout = Vec::new();
            for member in &recipients {
                let devices = self.private_post_devices(member, &me).await?;
                if devices.into_iter().any(|(device, registration)| {
                    !distributed.contains(&(member.clone(), device))
                        && !registration.is_some_and(|r| holders.contains(&(member.clone(), device, r)))
                }) {
                    fanout.push(member);
                }
            }
            let repairs_left = PRIVATE_POST_BUDGET.with(|post| MAX_REPAIRS_PER_POST - post.repairs.get());
            self.require_private_post_balance(fanout.len(), repairs_left)?;
            for member in fanout {
                let plaintext = self.skdm_plaintext(&channel_id, &verified, distribution, member);
                let outcome = self.deposit_private_channel_key(
                    &mut auth_client, &mut message_client, member, &plaintext, &channel_id,
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
                if self.private_post_devices(member, &me).await?.iter()
                    .any(|(device, _)| !distributed.contains(&(member.clone(), *device)))
                {
                    return Err(MessageError::Network(NetworkError::Unavailable));
                }
            }
            let (sealed, destinations) = self
                .signal_service
                .seal_group_message(&channel_id, contents.clone(), &recipients)
                .await
                .map_err(|e| MessageError::from_signal_send(e, "group seal"))?;
            crate::services::gateway::submission::mark_submission_started()?;
            let resp = self
                .grpc_pool
                .private_post_client(channel_id)
                .send_group_message(&request.channel_id, token.clone(), sealed, &request.client_request_id)
                .await?;
            if resp.success {
                let addressed = destinations.iter()
                    .map(|a| (a.name().to_owned(), u32::from(a.device_id())))
                    .collect();
                // The post is accepted: a failure here must not fail (and so resend) it.
                if let Err(error) = self.signal_service.rotate_if_holders_unaddressed(&channel_id, &addressed) {
                    tracing::error!(%channel_id, %error, "Could not check sender-key holders after a private post");
                }
                if retain_resend_targets {
                    use prost::Message as _;
                    let original = chatter_proto::content_v1::GroupEncrypted::decode(contents.as_slice())
                        .map_err(invalid)?;
                    self.snapshot_private_resend_targets(channel_id, original.timestamp, &destinations)?;
                }
                for member in &omitted {
                    if let Err(error) = self.notice_omitted(&channel_id, member) {
                        tracing::warn!(%channel_id, %error, "Could not file the left-out-member notice");
                    }
                }
                return self.sent_group_message(request, resp.server_timestamp);
            }
            if resp.mismatched.is_empty() || round == MAX_RECONCILE_ROUNDS {
                return Err(invalid("members' devices still mismatched after reconciling"));
            }
            let count = resp.mismatched.iter().filter_map(|account| account.devices.as_ref())
                .map(|devices| devices.missing_devices.len() + devices.extra_devices.len() + devices.stale_devices.len())
                .sum();
            charge_private_repairs(count)?;
            for account in &resp.mismatched {
                if !recipients.contains(&account.user_id) {
                    return Err(invalid("device mismatch for an account outside the verified channel"));
                }
                let d = account.devices.clone().unwrap_or_default();
                distributed.retain(|(member, _)| member != &account.user_id);
                self.reconcile_private_post_devices(&account.user_id, &d)
                    .await
                    .map_err(|e| MessageError::from_signal_send(e, "device reconciliation"))?;
            }
        }
        unreachable!("the loop returns on its last round")
    }

    fn no_bundle(&self, channel_id: &ChannelId, member: &str) -> NoBundle {
        self.settings_repo.get::<NoBundle>(&no_bundle_key(channel_id, member)).ok().flatten().unwrap_or_default()
    }

    /// Strict discovery found no prekey bundle for `member`. Keep the post
    /// waiting (`Unavailable`, retried by the journal) until that has held
    /// for [`NO_BUNDLE_MISSES`] discoveries or [`NO_BUNDLE_SECS`], then leave
    /// the member out. Our own account always has this device's bundle.
    fn missing_bundle(
        &self, channel_id: &ChannelId, member: &str, me: &str, now: u64, omitted: &mut HashSet<String>,
    ) -> Result<(), MessageError> {
        if member == me {
            return Err(NetworkError::Unavailable.into());
        }
        let state = self.no_bundle(channel_id, member).missed(now);
        self.settings_repo.set(&no_bundle_key(channel_id, member), &state)?;
        if !state.omitted(now) {
            return Err(NetworkError::Unavailable.into());
        }
        omitted.insert(member.to_owned());
        Ok(())
    }

    /// A group send token for `recipients` only, from the cached endorsements
    /// (`full_token` combines any subset of them).
    fn subset_token(
        &self, channel_id: &ChannelId, verified: &VerifiedChannel, recipients: &[String], now: u64,
    ) -> Result<Vec<u8>, MessageError> {
        let endorsements = self.group_send_tokens
            .endorsements(channel_id, &verified.state_hash, now)
            .ok_or(NetworkError::Unavailable)?;
        let recipients: Vec<uuid::Uuid> = recipients.iter()
            .map(|id| uuid::Uuid::parse_str(id))
            .collect::<Result<_, _>>()
            .map_err(invalid)?;
        let server = group_send::pinned_server_params().map_err(invalid)?;
        group_send::full_token(&endorsements, &channel_id.to_string(), &verified.state_hash, &recipients, now, &server)
            .map_err(invalid)
    }

    /// Tell the channel, once, that its posts go out without `member`.
    fn notice_omitted(&self, channel_id: &ChannelId, member: &str) -> Result<(), MessageError> {
        use crate::domain::message::{ConversationType, Message, MessageContent, SystemMessage};
        let key = no_bundle_key(channel_id, member);
        let state = self.no_bundle(channel_id, member);
        if state.noticed {
            return Ok(());
        }
        let user: crate::domain::UserId = member.parse().map_err(invalid)?;
        let mut notice = Message::new_text(
            *channel_id, ConversationType::Channel, Some(user), String::new(),
            crate::util::time::TimestampNanos::now().as_u64(),
        );
        notice.content = MessageContent::System(SystemMessage::NoRegisteredDevice(user));
        notice.is_read = true;
        self.message_repo.insert_or_ignore(&notice)?;
        self.settings_repo.set(&key, &NoBundle { noticed: true, ..state })?;
        self.event_bus.emit_domain(crate::events::DomainEvent::MessageReceived { message: notice });
        Ok(())
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

/// Independent of posts and token issuance. Scan all active private memberships
/// so the selected channels do not disclose which ones have pending posts.
pub(crate) fn spawn_private_channel_maintenance<C: super::SendRecoveryClient>(
    client: &std::sync::Arc<C>,
) -> tokio::task::JoinHandle<()> {
    let weak = std::sync::Arc::downgrade(client);
    tokio::spawn(async move {
        crate::app::core_services::initial_token_refill_sleep().await;
        loop {
            {
                let Some(client) = weak.upgrade() else { break };
                let state = client.auth_state().get();
                if let Some(owner) = state.current_user_id.filter(|_| {
                    !state.session_ended && client.offline_state().connection_state().is_connected()
                }) {
                    let messages = client.messages();
                    let session = crate::state::send_session::SendSession::capture(client.auth_state());
                    crate::state::send_session::SEND_SESSION.scope(session, async {
                        if let Err(error) = messages.refresh_private_endorsements(owner).await {
                            tracing::warn!(%error, "Private-channel endorsement refresh failed");
                        }
                    }).await;
                }
            }
            crate::app::core_services::jittered_sleep().await;
        }
    })
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

    /// A member without a bundle is left out only after three discoveries
    /// or ten minutes, whichever comes first.
    #[test]
    fn a_member_without_a_bundle_is_omitted_after_a_bounded_wait() {
        let t0 = 1_000;
        let once = NoBundle::default().missed(t0);
        assert_eq!(once.first_at, t0);
        assert!(!once.omitted(t0));
        let twice = once.missed(t0 + 30);
        assert_eq!(twice.first_at, t0, "the wait is measured from the first miss");
        assert!(!twice.omitted(t0 + 30));
        assert!(twice.missed(t0 + 60).omitted(t0 + 60), "third discovery");
        assert!(once.omitted(t0 + NO_BUNDLE_SECS), "ten minutes");
        assert!(!NoBundle::default().omitted(t0 + NO_BUNDLE_SECS), "never missed");
    }
}
