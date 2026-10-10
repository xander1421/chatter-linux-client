//! Group message operations.
//!
//! Sender key distribution, group mailbox announcements, and N-way fanout delivery.

use crate::domain::ids::{ChannelId, UserId};
use crate::network::grpc::auth_client::AuthClient;
use crate::network::grpc::message_client::MessageClient;
use crate::storage::repositories::settings_repo::keys;
use crate::util::time::TimestampNanos;

use super::{MessageError, MessageService};
use crate::services::channel::verified_state::{
    PeerStateCheck, VerificationStatus, VerifiedChannel, VerifiedMembership, compare_peer_state,
};
use crate::storage::repositories::settings_repo::SettingsRepository;
use crate::storage::repositories::verified_membership_repo::VerifiedMembershipRepository;
use std::sync::Arc;

/// The channel service's half of the `(revision, state_hash)` cross-check
/// (Design B): a peer stamped a revision above our verified state, so fetch
/// `GetChannelChanges(local + 1)`, verify and commit. Registered with
/// [`MessageService::set_channel_state_sync`]. Called on a spawned task;
/// `peer_revision` is the highest revision a peer has shown us.
#[async_trait::async_trait]
pub trait ChannelStateSync: Send + Sync {
    async fn fetch_channel_changes(&self, channel_id: ChannelId, peer_revision: u64);
}

/// Every sender-key decision reads the channel's VERIFIED membership
/// (`verified_membership_repo`), never `channel_repo`, which mirrors whatever
/// the server says. Shared by the send side (`MessageService`) and the receive
/// side (`MessageGateway`); both reach the database through their settings repo.
pub(crate) struct GroupStateGate {
    repo: VerifiedMembershipRepository,
    settings: Arc<SettingsRepository>,
}

impl GroupStateGate {
    pub(crate) fn new(settings: &Arc<SettingsRepository>) -> Self {
        Self {
            repo: VerifiedMembershipRepository::new(settings.database().clone()),
            settings: settings.clone(),
        }
    }

    fn peer_ahead_key(channel_id: &ChannelId) -> String {
        format!("channel_state_peer_ahead:{channel_id}")
    }

    /// The verified state, when it allows sender-key traffic: present and
    /// neither failed nor forked. Anything else is send-blocked, like a
    /// pending rotation (`is_channel_send_blocked`).
    pub(crate) fn for_distribution(
        &self,
        channel_id: &ChannelId,
    ) -> Result<VerifiedChannel, MessageError> {
        match self.repo.load(channel_id)? {
            Some(v) if v.status == VerificationStatus::Ok => Ok(v),
            other => Err(MessageError::MembershipNotVerified {
                channel_id: *channel_id,
                status: other.map(|v| v.status),
            }),
        }
    }

    /// The verified state whatever its status (receiving is not distributing).
    pub(crate) fn load(&self, channel_id: &ChannelId) -> Option<VerifiedChannel> {
        self.repo
            .load(channel_id)
            .inspect_err(
                |e| tracing::warn!(%channel_id, error = %e, "Could not load verified membership"),
            )
            .ok()
            .flatten()
    }

    /// Whether group content or an SKDM from `sender` is accepted: a member
    /// of the verified distribution set. Signal drops group messages from
    /// non-members. Fails closed.
    pub(crate) fn admits(&self, channel_id: &ChannelId, sender: &UserId) -> bool {
        self.repo
            .is_distribution_member(channel_id, &sender.to_string())
            .inspect_err(|e| tracing::warn!(%channel_id, %sender, error = %e, "Verified membership lookup failed"))
            .unwrap_or(false)
    }

    /// Check a peer's `(revision, state_hash)` stamp against our verified
    /// state. A fork marks the channel `Fork` (send-blocked until the user
    /// acknowledges it) — only on the word of a verified member, or anyone
    /// the server invents could block the channel. A peer ahead records its
    /// revision for [`Self::take_peer_ahead`]. `None` when there is nothing
    /// to compare (no stamp, or no verified state and no revision claimed).
    pub(crate) fn cross_check(
        &self,
        channel_id: &ChannelId,
        sender_is_member: bool,
        peer_revision: u64,
        peer_hash: &[u8],
    ) -> Option<PeerStateCheck> {
        let check = match self.load(channel_id) {
            None if peer_revision == 0 => return None,
            None => PeerStateCheck::PeerAhead,
            Some(local) => {
                let local_revision = local.revision();
                let at_peer = (peer_revision < local_revision)
                    .then(|| {
                        self.repo
                            .state_hash_at(channel_id, peer_revision)
                            .ok()
                            .flatten()
                    })
                    .flatten();
                compare_peer_state(
                    local_revision,
                    &local.state_hash(),
                    peer_revision,
                    peer_hash,
                    at_peer.as_ref(),
                )
            }
        };
        match check {
            PeerStateCheck::Fork if sender_is_member => {
                tracing::error!(%channel_id, peer_revision,
                    "Membership FORK: a member's state hash differs from ours; channel send-blocked");
                if let Err(e) = self.repo.set_status(channel_id, VerificationStatus::Fork) {
                    tracing::warn!(%channel_id, error = %e, "Could not record membership fork");
                }
            }
            PeerStateCheck::Fork => {
                tracing::warn!(%channel_id, peer_revision, "Fork claim from a non-member ignored");
            }
            PeerStateCheck::PeerAhead => {
                let key = Self::peer_ahead_key(channel_id);
                let known = self.settings.get::<u64>(&key).ok().flatten().unwrap_or(0);
                if peer_revision > known {
                    if let Err(e) = self.settings.set(&key, &peer_revision) {
                        tracing::warn!(%channel_id, error = %e, "Could not record a peer's newer revision");
                    }
                }
            }
            PeerStateCheck::InSync | PeerStateCheck::PeerBehind => {}
        }
        Some(check)
    }

    /// The highest revision a peer showed above ours since the last call,
    /// cleared on read (the receive paths that have no `MessageService`
    /// record it; [`MessageService::sync_channel_state_if_behind`] drains it).
    pub(crate) fn take_peer_ahead(&self, channel_id: &ChannelId) -> Option<u64> {
        let key = Self::peer_ahead_key(channel_id);
        let rev = self.settings.get::<u64>(&key).ok().flatten()?;
        let _ = self.settings.delete(&key);
        Some(rev)
    }
}

/// Stamp our verified `(revision, state_hash)` into a group DataMessage.
pub(crate) fn stamp_group_data_message(
    dm: &mut crate::proto::content_v1::DataMessage,
    state: &VerifiedChannel,
) {
    dm.group_state_revision = state.revision();
    dm.group_state_hash = state.state_hash().to_vec();
}

fn sender_key_distribution_result(
    delivered: usize,
    recipients: usize,
    failure: Option<MessageError>,
) -> Result<(), MessageError> {
    if delivered == 0 && recipients > 0 {
        return Err(failure.unwrap_or(MessageError::Network(
            crate::error::NetworkError::Unavailable,
        )));
    }
    Ok(())
}

impl MessageService {
    /// Distribute a sender key distribution message to channel members.
    ///
    /// Implements NDSS '21 pairwise distribution:
    /// - Each member receives SKDM + our mailbox announcement
    /// - Encrypted pairwise via Double Ratchet (Sealed Sender envelope)
    /// - Deposited in the member's device inboxes UAK-authorized, like a 1:1 DM.
    ///   Channel members already know each other, so this spends no anonymous
    ///   mailbox token (OPEN-ITEMS §120).
    pub async fn distribute_sender_key(
        &self,
        auth_client: &mut AuthClient,
        message_client: &mut MessageClient,
        channel_id: &ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        // Our user ID (the deposit leaves this device out of our own fan-out —
        // it already holds our sender key).
        let our_user_id = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;

        // Fan SKDM to ALL verified members including our own user's OTHER
        // linked devices. Own device on this client is the only one excluded —
        // multi-device sync requires every other device that might receive
        // this channel's messages to hold our sender key. The server's member
        // list is not consulted: a member it alone asserts gets nothing.
        let verified = self.group_gate().for_distribution(channel_id)?;
        let fanout_user_ids: Vec<String> = verified.members();

        if fanout_user_ids.is_empty() {
            tracing::debug!(
                channel_id = %channel_id,
                "Skipping sender key distribution (no channel members)"
            );
            return Ok(());
        }

        let has_mailbox = self.our_group_mailbox_pk(channel_id).is_some();

        // Distribute to each member's devices pairwise via Sealed Sender.
        let mut success_count = 0;
        let mut error_count = 0;
        let mut failure = None;

        for member_id in &fanout_user_ids {
            let is_self = member_id == &our_user_id;
            let plaintext = self.skdm_plaintext(channel_id, &verified, skdm, member_id);

            // Sender keys are per (user, device), so every device gets its own
            // ciphertext. Our own current device already holds the sender key.
            let outcome = self
                .deposit_sealed_to_all_devices(auth_client, message_client, member_id, &plaintext)
                .await;
            success_count += outcome.delivered;
            self.record_sender_key_holders(channel_id, member_id, &outcome);
            if outcome.delivered == 0 && !is_self {
                error_count += 1;
                if let Some(error) = outcome.error {
                    super::server_dm::record_deposit_error(&mut failure, error);
                }
            }
        }

        tracing::info!(
            channel_id = %channel_id,
            total_members = fanout_user_ids.len(),
            success = success_count,
            errors = error_count,
            has_mailbox = has_mailbox,
            "Distributed sender key via pairwise sealed sender"
        );

        // Consider it success if at least one device received the SKDM (or
        // the only "member" is ourselves on this one device and no fanout is needed).
        let non_self_members = fanout_user_ids
            .iter()
            .filter(|u| *u != &our_user_id)
            .count();
        sender_key_distribution_result(success_count, non_self_members, failure)
    }

    /// Our mailbox public key for this channel, if we have one (NDSS '21
    /// announcement that rides every SKDM).
    fn our_group_mailbox_pk(&self, channel_id: &ChannelId) -> Option<Vec<u8>> {
        self.group_mailbox_manager
            .load_group_state(channel_id)
            .ok()
            .flatten()
            .and_then(|state| state.our_mailbox)
            .map(|mailbox| mailbox.public_key.to_vec())
    }

    /// The padded, encoded SKDM Content for one recipient: the SKDM stamped
    /// with our verified `(revision, state_hash)` for the receiver's fork /
    /// staleness cross-check, our mailbox announcement, and our profile key
    /// (see [`Self::skdm_profile_key`]).
    fn skdm_plaintext(
        &self,
        channel_id: &ChannelId,
        verified: &VerifiedChannel,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
        recipient: &str,
    ) -> Vec<u8> {
        use crate::proto::content_v1::{self, content};
        use prost::Message;

        let mut skdm = skdm.clone();
        skdm.state_revision = verified.revision();
        skdm.state_hash = verified.state_hash().to_vec();
        skdm.profile_key = self.skdm_profile_key(verified, recipient);
        let mut skdm_content = content_v1::Content {
            ephemeral_reply_to: self.our_group_mailbox_pk(channel_id),
            timestamp: TimestampNanos::now().as_u64(),
            padding: vec![],
            content: Some(content::Content::SenderKey(skdm)),
        };
        Self::pad_content_to_bucket(&mut skdm_content);
        skdm_content.encode_to_vec()
    }

    /// Our profile key for a channel member's SKDM, so they can decrypt our
    /// name and avatar without being our friend — Signal shares every group
    /// member's profile key with the group. The SKDM is pairwise-encrypted,
    /// so it reaches only this member; `None` for anyone not in the channel's
    /// verified distribution set — an SKDM also answers a
    /// `SenderKeyRequest`, which anyone can send — and for whoever the
    /// visibility setting keeps it from (ourselves, a member we blocked, a
    /// non-friend under "my contacts", everyone under "nobody"). Fails
    /// closed: a lookup error shares nothing.
    fn skdm_profile_key(&self, verified: &VerifiedChannel, recipient: &str) -> Option<Vec<u8>> {
        if !verified.contains(recipient) {
            return None;
        }
        let recipient: UserId = recipient.parse().ok()?;
        crate::services::profile_visibility::shareable_profile_key(
            &self.settings_repo,
            &self.user_repo,
            &recipient,
        )
        .map(|k| k.to_vec())
    }

    /// Record exactly the devices an SKDM deposit reached (`deposits` of its
    /// [`super::server_dm::DepositOutcome`]), each under the registration id
    /// of the session it was encrypted with (`registration_ids`). A device
    /// without one is not recorded, so it still counts as lacking our key.
    fn record_sender_key_holders(
        &self,
        channel_id: &ChannelId,
        member_id: &str,
        outcome: &super::server_dm::DepositOutcome,
    ) {
        let devices: Vec<(u32, u32)> = outcome
            .deposits
            .iter()
            .filter_map(|&(device, _)| {
                let registration_id = outcome.registration_ids.get(&device).copied();
                if registration_id.is_none() {
                    tracing::warn!(%channel_id, member_id, device, "SKDM deposit without a registration id — not recorded as held");
                }
                registration_id.map(|r| (device, r))
            })
            .collect();
        if let Err(e) = self
            .signal_service
            .mark_sender_key_held(channel_id, member_id, &devices)
        {
            tracing::warn!(%channel_id, member_id, error = %e, "Failed to record SKDM delivery");
        }
    }

    /// `(device, registration id)` of each device of `member` that must hold
    /// our sender key, from local state only (no prekey fetch per send, as
    /// Signal-Desktop reads its open sessions): the devices we hold a usable
    /// session with — a peer's added device gets one from its
    /// `PEER_DEVICES_CHANGED` (`handle_peer_device_change`), a removed one is
    /// archived there or on the server's mismatch answer to a send. For our
    /// own user, our live device list (no device change notification reaches
    /// us for our own links) minus this device, falling back to our sessions
    /// when it cannot be fetched. A device we have no session with has no
    /// registration id (`None`), which no delivery records, so it counts as new.
    async fn sender_key_devices(
        &self,
        member: &str,
        our_user_id: &str,
        our_device_id: u32,
    ) -> Vec<(u32, Option<u32>)> {
        let own = if member == our_user_id {
            self.own_device_ids().await
        } else {
            None
        };
        let devices = match own {
            Some(ids) => ids.into_iter().collect(),
            None => self
                .signal_service
                .session_devices(member)
                .await
                .unwrap_or_default(),
        };
        devices
            .into_iter()
            .filter(|&d| member != our_user_id || d != our_device_id)
            .map(|d| (d, self.signal_service.session_registration_id(member, d)))
            .collect()
    }

    /// Verified channel members (our own user included, for our other devices)
    /// with a device that does not hold our CURRENT sender key — Signal-Desktop's
    /// `newToMemberServiceIds`. A member other than us with no known device
    /// is one we have never reached; it is included so the fan-out
    /// establishes its sessions (Signal fetches prekeys for such members).
    pub(crate) async fn members_lacking_sender_key(
        &self,
        channel_id: &ChannelId,
        our_user_id: &str,
        our_device_id: u32,
    ) -> Result<Vec<UserId>, MessageError> {
        let holders = self
            .signal_service
            .sender_key_holders(channel_id)
            .map_err(|e| MessageError::EncryptionFailed(e.to_string()))?;
        let mut lacking = Vec::new();
        for user in self.group_gate().for_distribution(channel_id)?.members() {
            let Ok(user_id) = user.parse::<UserId>() else {
                continue;
            };
            let devices = self
                .sender_key_devices(&user, our_user_id, our_device_id)
                .await;
            if lacks_sender_key(&holders, &user, &devices, user == our_user_id) {
                lacking.push(user_id);
            }
        }
        Ok(lacking)
    }

    /// Our live device ids, from the `own_devices` cache (refetched when
    /// over a minute old). `None` when the list cannot be fetched.
    pub(super) async fn own_device_ids(&self) -> Option<std::collections::HashSet<u32>> {
        const MAX_AGE: u64 = 60 * 1_000_000_000;
        let mut cache = self.own_devices.lock().await;
        let now = TimestampNanos::now().as_u64();
        if let Some((fetched_at, ids)) = cache.as_ref() {
            if now.saturating_sub(*fetched_at) <= MAX_AGE {
                return Some(ids.clone());
            }
        }
        match self.fetch_own_device_ids().await {
            Ok(ids) => {
                *cache = Some((now, ids.clone()));
                Some(ids)
            }
            Err(e) => {
                tracing::warn!(error = %e, "Could not list our devices");
                None
            }
        }
    }

    /// Our devices that can still send and receive: the ones with a prekey
    /// bundle, the same set every own-user fan-out encrypts to, so "a device
    /// of ours" means one thing everywhere. (An identity republish drops every
    /// other device's bundle and its `user_devices` row together — server
    /// `republish_identity.go` — so this and `ListDevices` agree.) The
    /// wildcard fetch pops no one-time prekeys.
    async fn fetch_own_device_ids(
        &self,
    ) -> Result<std::collections::HashSet<u32>, crate::error::NetworkError> {
        use crate::services::current_user::CurrentUserProvider;
        let me = self
            .settings_repo
            .current_user_id()
            .map_err(|e| crate::error::NetworkError::InvalidRequest(e.to_string()))?;
        let resp = self
            .auth_client()
            .get_pre_key_bundles_all_devices(&me.to_string())
            .await?;
        Ok(bundle_device_ids(resp))
    }

    /// Hand our CURRENT sender key to every channel member with a device that
    /// does not hold it yet ([`Self::members_lacking_sender_key`]): a member
    /// newly added, a member's newly linked or re-registered device, our own
    /// newly linked device. The SKDM goes to all of that member's devices, as
    /// Signal-Desktop's does; a device that already held it keeps its chain
    /// (`add_sender_key_state` ignores a known chain id). MUST run before the message is encrypted: the SKDM carries the
    /// chain's current iteration, so one built after `group_encrypt` would
    /// start one past the message it was meant to unlock (Signal-Desktop
    /// `sendToGroup` sends to `newToMemberDevices` first for the same reason).
    ///
    /// A member we cannot reach is logged and left for the next send — failing
    /// the whole send would let one unreachable member mute the channel. That
    /// member reads this message only if its SKDM lands before the next one.
    ///
    /// an unreachable member costs a prekey fetch per send; add a
    /// per-member backoff if that shows up against the fetch limiter.
    pub(crate) async fn distribute_sender_key_to_new_members(
        &self,
        channel_id: &ChannelId,
    ) -> Result<(), MessageError> {
        // No key yet: `encrypt_for_group` creates one and the caller fans it
        // to everyone via `distribute_sender_key`. Mid-rotation: the old key
        // must not go anywhere, and `encrypt_for_group` refuses the send.
        let crypto = |e: crate::error::ServiceError| MessageError::EncryptionFailed(e.to_string());
        if self
            .signal_service
            .is_channel_send_blocked(channel_id)
            .map_err(crypto)?
            || !self
                .signal_service
                .has_sender_key(channel_id)
                .await
                .map_err(crypto)?
        {
            return Ok(());
        }
        let our_user_id = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;
        let our_device_id = self
            .settings_repo
            .get::<u32>(keys::CURRENT_DEVICE_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;
        let lacking = self
            .members_lacking_sender_key(channel_id, &our_user_id, our_device_id)
            .await?;
        if lacking.is_empty() {
            return Ok(());
        }

        let skdm = self.create_sender_key_for_channel(channel_id).await?;
        crate::services::gateway::submission::mark_submission_started()?;
        for member_id in &lacking {
            if let Err(e) = self.send_skdm_to_member(channel_id, member_id, &skdm).await {
                tracing::warn!(
                    %channel_id, %member_id, error = %e,
                    "Could not hand our sender key to a member's new devices before sending; \
                     they cannot read this message unless it lands first"
                );
            }
        }
        Ok(())
    }

    /// Whether to send a `SenderKeyRequest` for `(channel, sender, device)` now.
    ///
    /// One outstanding request per key, with exponential backoff: a history
    /// sync that parks twenty of one sender's messages used to fire twenty
    /// requests (and twenty prekey fetches, which tripped the server's per-target
    /// fetch limiter), and the sender answered every one. The gate is lifted
    /// when that key's SKDM arrives (`retry_pending_group_decrypts`).
    pub fn should_request_sender_key(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
    ) -> bool {
        sender_key_request_due(
            &mut self.sender_key_requests.lock(),
            (channel_id, sender_id, device_id),
            std::time::Instant::now(),
        )
    }

    /// Queue a group-encrypted proto whose sender key isn't stored yet.
    ///
    /// Call site: inbox stream decrypt fails with "no sender key" → enqueue
    /// here, then fire a `SenderKeyRequest`. The matching
    /// `SenderKeyReceived` event on the bus should trigger
    /// [`retry_pending_group_decrypts`].
    pub fn defer_pending_group_decrypt(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
        proto_msg: crate::proto::chat_v1::Message,
    ) {
        // Bound the buffer so an SKDM that never arrives (kicked sender, lost
        // device, malicious peer) can't leak memory. Each message waits
        // PENDING_GROUP_DECRYPT_TTL for its SKDM, then is handed to
        // `expire_pending_group_decrypt` (which asks the sender to resend it);
        // the whole map is capped at MAX_PENDING entries — oldest-first
        // eviction.
        const MAX_PENDING: usize = 512;
        let now = std::time::Instant::now();
        let message_id = proto_msg.id.clone();

        let mut guard = self.pending_group_decrypts.lock();

        // A history sync can hand us the same message more than once. Park it
        // once: a second copy would decrypt as libsignal's DuplicatedMessage
        // after the first consumed the message key.
        let parked = guard.entry((channel_id, sender_id, device_id)).or_default();
        if parked
            .iter()
            .any(|(_, m)| m.id == proto_msg.id && m.content == proto_msg.content)
        {
            return;
        }
        parked.push((now, proto_msg));

        // Enforce the global cap by evicting the globally-oldest parked message
        // until back within budget.
        let mut evicted = Vec::new();
        let mut total: usize = guard.values().map(|v| v.len()).sum();
        while total > MAX_PENDING {
            let oldest_key = guard
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .min_by_key(|(_, v)| v[0].0)
                .map(|(k, _)| *k);
            match oldest_key {
                Some(k) => {
                    if let Some(v) = guard.get_mut(&k) {
                        evicted.push((k, v.remove(0).1));
                        total -= 1;
                    }
                }
                None => break,
            }
        }
        guard.retain(|_, msgs| !msgs.is_empty());
        drop(guard);

        // An evicted message is not lost silently: its sender is asked to
        // resend it, exactly as when its wait runs out. Channel messages are
        // resendable (the sender keeps them 24h), which is where Signal-Desktop
        // sends a DecryptionErrorMessage rather than recording a delivery
        // issue (`handleRetry.ts`: `ContentHint.RESENDABLE`). Rate-limited per
        // sender so one flooding member cannot turn the cap into a DEM storm.
        let now = std::time::Instant::now();
        for ((channel_id, sender_id, device_id), proto_msg) in evicted {
            if !eviction_resend_due(&mut self.eviction_resends.lock(), sender_id, now) {
                tracing::warn!(%channel_id, %sender_id, device_id, message_id = %proto_msg.id,
                    "Parked group message evicted over the cap and the sender's resend budget; dropped");
                continue;
            }
            let svc = self.clone();
            tokio::spawn(async move {
                svc.resend_parked(channel_id, sender_id, device_id, proto_msg)
                    .await;
            });
        }

        let svc = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PENDING_GROUP_DECRYPT_TTL).await;
            svc.expire_pending_group_decrypt(channel_id, sender_id, device_id, &message_id)
                .await;
        });
    }

    /// A parked message whose SKDM did not arrive within
    /// [`PENDING_GROUP_DECRYPT_TTL`]: stop waiting and ask its sender to resend
    /// it pairwise, as for a message the SKDM could not open. Dropping it here
    /// silently lost it for good when the SKDM came late. A no-op when the
    /// SKDM already drained it (`retry_pending_group_decrypts`).
    pub(crate) async fn expire_pending_group_decrypt(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
        message_id: &str,
    ) {
        let expired = {
            let mut guard = self.pending_group_decrypts.lock();
            let key = (channel_id, sender_id, device_id);
            let Some(parked) = guard.get_mut(&key) else {
                return;
            };
            let (expired, kept): (Vec<_>, Vec<_>) = std::mem::take(parked)
                .into_iter()
                .partition(|(_, m)| m.id == message_id);
            *parked = kept;
            if parked.is_empty() {
                guard.remove(&key);
            }
            expired
        };
        let Some((_, proto_msg)) = expired.into_iter().next() else {
            return;
        };
        tracing::info!(%channel_id, %sender_id, device_id, %message_id,
            "Sender key never arrived for a parked message; requesting a resend");
        self.resend_parked(channel_id, sender_id, device_id, proto_msg)
            .await;
    }

    /// Hand a parked (still sender-key encrypted) message that left the queue
    /// undecrypted to [`Self::request_group_resend`].
    async fn resend_parked(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
        proto_msg: crate::proto::chat_v1::Message,
    ) {
        use crate::proto::chat_v1::message::content::Type;
        use crate::proto::content_v1::GroupEncrypted;
        use prost::Message as _;

        let Some(Type::Encrypted(enc)) = proto_msg.content.as_ref().and_then(|c| c.r#type.as_ref())
        else {
            return;
        };
        let Ok(group_encrypted) = GroupEncrypted::decode(enc.ciphertext.as_slice()) else {
            return;
        };
        self.request_group_resend(
            channel_id,
            sender_id,
            device_id,
            &proto_msg,
            &group_encrypted.ciphertext,
        )
        .await;
    }

    /// Drain deferred group messages for `(channel, sender, device)` and
    /// re-run decrypt using the now-stored sender key. Successes emit
    /// `DomainEvent::MessageReceived`; permanent failures are dropped.
    pub async fn retry_pending_group_decrypts(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
    ) {
        use crate::events::DomainEvent;
        use crate::proto::chat_v1;
        use crate::proto::content_v1::GroupEncrypted;
        use crate::services::message::conversion::proto_to_domain_message_with_sender;

        // An SKDM reply is not proof that it supplied a missing historical key.
        // Keep request backoff until a parked message is durably persisted.

        let pending: Vec<chat_v1::Message> = {
            let mut guard = self.pending_group_decrypts.lock();
            guard
                .remove(&(channel_id, sender_id, device_id))
                .unwrap_or_default()
                .into_iter()
                .map(|(_parked_at, msg)| msg)
                .collect()
        };

        // The SKDM's stamp may have shown a peer ahead of us.
        self.sync_channel_state_if_behind(&channel_id);

        if pending.is_empty() {
            return;
        }
        // Admitted when parked; the verified state may have dropped the sender since.
        if !self.group_gate().admits(&channel_id, &sender_id) {
            tracing::warn!(%channel_id, %sender_id, count = pending.len(),
                "Dropping parked group messages: sender is no longer a verified member");
            return;
        }

        tracing::info!(
            channel_id = %channel_id,
            sender_id = %sender_id,
            device_id,
            count = pending.len(),
            "Retrying deferred group-encrypted messages after SKDM arrival"
        );

        for proto_msg in pending {
            let Some(chat_v1::message::content::Type::Encrypted(enc)) =
                proto_msg.content.as_ref().and_then(|c| c.r#type.as_ref())
            else {
                continue;
            };
            use prost::Message as _;
            let group_encrypted = match GroupEncrypted::decode(enc.ciphertext.as_slice()) {
                Ok(ge) => ge,
                Err(e) => {
                    tracing::warn!(error = %e, "Retry: failed to decode GroupEncrypted");
                    continue;
                }
            };
            match self
                .signal_service
                .decrypt_from_group(&sender_id, device_id, &group_encrypted)
                .await
            {
                Ok(plaintext) => {
                    use prost::Message as ProstMessage;
                    // The sender-key plaintext is a `content_v1::Content`
                    // wrapping a `DataMessage` (see message/core.rs send path
                    // and the streaming decrypt in client/channels.rs) — NOT a
                    // `chat_v1::message::Content`. Decoding it as the wire
                    // Content silently yielded r#type=None and dropped every
                    // healed message; mirror the stream path exactly instead.
                    let dm = match crate::proto::content_v1::Content::decode(plaintext.as_slice())
                        .ok()
                        .and_then(|c| c.content)
                    {
                        Some(crate::proto::content_v1::content::Content::DataMessage(dm)) => dm,
                        _ => {
                            tracing::warn!(
                                sender_id = %sender_id,
                                device_id,
                                "Retry: decrypted plaintext is not a valid DataMessage; dropping"
                            );
                            continue;
                        }
                    };
                    if dm.channel_id.parse::<ChannelId>().ok() != Some(channel_id)
                        || dm.timestamp == 0
                        || proto_msg.channel_id.parse::<ChannelId>().ok() != Some(channel_id)
                        || proto_msg.author_id.parse::<UserId>().ok() != Some(sender_id)
                        || proto_msg.author_device_id != device_id
                        || (!dm.message_id.is_empty() && dm.message_id != proto_msg.id)
                        || ((!dm.message_id.is_empty() || !dm.thread_id.is_empty())
                            && dm.thread_id != proto_msg.parent_id)
                    {
                        tracing::warn!("Retry: authenticated channel identity mismatch");
                        continue;
                    }
                    self.check_group_stamp(&channel_id, &dm);

                    // Persist incoming attachment pointers so the receiver can
                    // download + decrypt (parity with the stream path).
                    if !dm.attachments.is_empty() {
                        let attachment_service =
                            crate::services::attachment::AttachmentService::new_local(
                                self.attachment_repo.clone(),
                            );
                        attachment_service.create_from_pointers(&dm.attachments);
                    }

                    // Rebuild the wire proto with the decrypted text +
                    // attachment ids so `proto_to_domain_message_with_sender`
                    // picks them up (Text or Mixed), exactly as the stream path.
                    let files: Vec<chat_v1::FileMetadata> = dm
                        .attachments
                        .iter()
                        .filter(|p| !p.cdn_key.is_empty())
                        .map(|p| chat_v1::FileMetadata {
                            id: crate::domain::AttachmentId::from_cdn_key(&p.cdn_key).to_string(),
                            name: p.file_name.clone(),
                            mime_type: p.content_type.clone(),
                            size: p.plaintext_size,
                            ..Default::default()
                        })
                        .collect();
                    let new_type = if files.is_empty() {
                        chat_v1::message::content::Type::Text(dm.body.clone())
                    } else {
                        chat_v1::message::content::Type::Mixed(chat_v1::message::MixedContent {
                            text: dm.body.clone(),
                            files,
                        })
                    };
                    let mut patched = proto_msg.clone();
                    // Target identity comes from the authenticated DataMessage,
                    // never from the later server transport envelope.
                    patched.timestamp = dm.timestamp;
                    patched.content = Some(chat_v1::message::Content {
                        r#type: Some(new_type),
                    });
                    // Group decrypt retry path — channel_id is the
                    // current retry target. Derive ConversationType so
                    // identified groups stay tagged Channel.
                    let conv_type =
                        crate::services::message::conversion::conversation_type_for_channel(
                            &self.channel_repo,
                            &channel_id,
                        );
                    match proto_to_domain_message_with_sender(&patched, Some(sender_id), conv_type)
                    {
                        Ok(mut message) => {
                            crate::services::gateway::inbound::apply_inbound_quote(
                                &mut message,
                                dm.quote.as_ref(),
                                &self.message_repo,
                            );
                            if !super::channel_inbound::ChannelInbound::persist_message(
                                self,
                                &mut message,
                                channel_id,
                                !dm.message_id.is_empty(),
                            ) {
                                continue;
                            }
                            {
                                let mut gate = self.sender_key_requests.lock();
                                gate.remove(&(channel_id, sender_id, device_id));
                                gate.remove(&(channel_id, sender_id, 0));
                            }
                            let _ = self
                                .event_bus
                                .domain_sender()
                                .send(DomainEvent::MessageReceived { message });
                        }
                        Err(e) => tracing::warn!(error = %e, "Retry: conversion failed"),
                    }
                }
                // libsignal's DuplicatedMessage: the message's iteration is
                // below the chain we hold. The SKDM we got starts at the
                // sender's CURRENT iteration (libsignal
                // `create_sender_key_distribution_message`), so a message they
                // sent before distributing to us has no key on this device and
                // never will: only the sender can give us the content again.
                // Any other failure after the SKDM is equally terminal here.
                Err(e) => {
                    tracing::info!(
                        sender_id = %sender_id,
                        device_id,
                        message_id = %proto_msg.id,
                        error = %e,
                        "Parked message still undecryptable after SKDM; requesting a resend"
                    );
                    self.request_group_resend(
                        channel_id,
                        sender_id,
                        device_id,
                        &proto_msg,
                        &group_encrypted.ciphertext,
                    )
                    .await;
                }
            }
        }
    }

    /// Ask every other verified member for their SKDM (asking leaks nothing,
    /// so a forked or failed state still asks; only verified members' SKDMs
    /// are accepted). Nothing without a verified state.
    ///
    /// Called right after joining an encrypted channel so we can start
    /// decrypting messages the existing members send — without this step
    /// their first message after our join would fail the no-sender-key
    /// path and sit in the pending-decrypt queue until a manual retry.
    /// Individual request failures are logged but do not abort the loop;
    /// missing peers will heal on first decrypt miss.
    pub async fn request_skdms_from_channel_members(
        &self,
        channel_id: &ChannelId,
    ) -> Result<(), MessageError> {
        let current_user = self
            .settings_repo
            .get_string(keys::CURRENT_USER_ID)
            .map_err(MessageError::Storage)?
            .ok_or(MessageError::NotLoggedIn)?;
        let Some(verified) = self.group_gate().load(channel_id) else {
            tracing::debug!(%channel_id, "No verified membership yet; not requesting SKDMs");
            return Ok(());
        };
        for member in verified.members() {
            if member == current_user {
                continue;
            }
            let Ok(member_id) = member.parse::<UserId>() else {
                continue;
            };
            if let Err(e) = self.send_sender_key_request(channel_id, &member_id).await {
                tracing::warn!(
                    channel_id = %channel_id,
                    member_id = %member_id,
                    error = %e,
                    "Failed to request SKDM from existing member"
                );
            }
        }
        Ok(())
    }

    /// Distribute sender key to channel members (simplified API - no client params).
    pub async fn distribute_skdm(
        &self,
        channel_id: &ChannelId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        let mut auth_client = self.auth_client();
        let mut message_client = self.message_client();

        self.distribute_sender_key(&mut auth_client, &mut message_client, channel_id, skdm)
            .await
    }

    /// Send SKDM to a single member (for when a new member joins).
    ///
    /// This is called when we receive a MemberAdded event - we send our
    /// existing SKDM to the new member so they can decrypt our messages.
    pub async fn send_skdm_to_member(
        &self,
        channel_id: &ChannelId,
        member_id: &UserId,
        skdm: &crate::proto::content_v1::SenderKeyDistribution,
    ) -> Result<(), MessageError> {
        let verified = self.group_gate().for_distribution(channel_id)?;
        let member_id_str = member_id.to_string();
        if !verified.contains(&member_id_str) {
            return Err(MessageError::EncryptionFailed(format!(
                "{member_id} is not a verified member of {channel_id}; refusing to hand it our sender key"
            )));
        }
        let mut auth_client = self.auth_client();

        let plaintext = self.skdm_plaintext(channel_id, &verified, skdm, &member_id_str);
        // To ourselves (our other devices asking, or a newly linked one) the
        // deposit leaves out this device, which already holds the key.

        // Per-device sealed deposit, UAK-authorized: sender keys are keyed by
        // (user, device), so each device needs its own ciphertext.
        let mut message_client = self.message_client();
        let outcome = self
            .deposit_sealed_to_all_devices(
                &mut auth_client,
                &mut message_client,
                &member_id_str,
                &plaintext,
            )
            .await;

        if outcome.delivered == 0 {
            return Err(MessageError::EncryptionFailed(
                "SKDM delivery to new member failed on every device".into(),
            ));
        }
        self.record_sender_key_holders(channel_id, &member_id_str, &outcome);

        tracing::info!(
            channel_id = %channel_id,
            member_id = %member_id,
            "Sent SKDM to new channel member via sealed sender"
        );

        Ok(())
    }

    /// Whether `user` may be handed our sender key for `channel_id`: only a
    /// member of the VERIFIED distribution set, the same set every SKDM
    /// fan-out iterates (a verified removal drops them and marks the key for
    /// rotation), and only while the state is neither failed nor forked.
    /// Signal distributes sender keys to current group members only. Fails
    /// closed: a lookup error hands out nothing.
    fn may_hold_sender_key(&self, channel_id: &ChannelId, user: &UserId) -> bool {
        self.group_gate()
            .for_distribution(channel_id)
            .is_ok_and(|v| v.contains(&user.to_string()))
    }

    /// The key-gating reads of the verified membership state.
    pub(crate) fn group_gate(&self) -> GroupStateGate {
        GroupStateGate::new(&self.settings_repo)
    }

    /// Register the channel service's change fetcher (W4).
    pub fn set_channel_state_sync(&self, sync: Arc<dyn ChannelStateSync>) {
        *self.channel_state_sync.write() = Some(sync);
    }

    /// Act on a peer's stamp: a peer ahead of us (recorded by any receive
    /// path) has the channel service fetch the changes we lack. A recorded
    /// revision is left for later when no fetcher is registered.
    pub(crate) fn sync_channel_state_if_behind(&self, channel_id: &ChannelId) {
        let Some(sync) = self.channel_state_sync.read().clone() else {
            return;
        };
        let Some(peer_revision) = self.group_gate().take_peer_ahead(channel_id) else {
            return;
        };
        let channel_id = *channel_id;
        tokio::spawn(async move { sync.fetch_channel_changes(channel_id, peer_revision).await });
    }

    /// Cross-check a received group DataMessage's stamp from a verified
    /// member ([`GroupStateGate::cross_check`]), then fetch if it is ahead.
    pub(crate) fn check_group_stamp(
        &self,
        channel_id: &ChannelId,
        dm: &crate::proto::content_v1::DataMessage,
    ) {
        self.group_gate().cross_check(
            channel_id,
            true,
            dm.group_state_revision,
            &dm.group_state_hash,
        );
        self.sync_channel_state_if_behind(channel_id);
    }

    /// A peer sharing a channel with us added or removed a device
    /// (`PEER_DEVICES_CHANGED`, server `peerfanout.go`: `change_type` is
    /// `added`, `revoked`, or `reregistered` for a whole account).
    ///
    /// As Signal-Desktop `sendToGroup`: a device change of a member who stays
    /// needs no sender-key reset — an added device only needs our SKDM, and a
    /// removed one is just dropped (`removedFromMemberDevices` without a
    /// reset). So `added` opens a session with that one device, which makes
    /// the next group send's top-up ([`Self::members_lacking_sender_key`])
    /// hand it our current key; `revoked` archives that one device's session
    /// so nothing is encrypted to it again, and leaves the peer's other
    /// sessions alone. `reregistered` deletes the account: like a removed
    /// member, every shared channel's key is reset and its sessions archived.
    pub async fn handle_peer_device_change(&self, peer: UserId, device_id: u32, change_type: &str) {
        let peer_str = peer.to_string();
        match change_type {
            "added" => {
                let result = self
                    .signal_service
                    .ensure_session(&mut self.auth_client(), &peer_str, device_id)
                    .await;
                match result {
                    Ok(()) => tracing::info!(peer = %peer_str, device_id,
                        "Opened a session with a peer's new device; next group send hands it our sender key"),
                    Err(e) => tracing::warn!(peer = %peer_str, device_id, error = %e,
                        "Could not open a session with a peer's new device; it heals via its SenderKeyRequest"),
                }
            }
            "revoked" => {
                if let Err(e) = self
                    .signal_service
                    .actor
                    .archive_session_for_resend(peer_str.clone(), device_id, None)
                    .await
                {
                    tracing::warn!(peer = %peer_str, device_id, error = %e,
                        "Failed to archive the session of a peer's removed device");
                }
            }
            other => {
                if other != "reregistered" {
                    tracing::warn!(peer = %peer_str, change_type = other,
                        "Unknown peer device change; treating it as an account reset");
                }
                if let Err(e) = self
                    .signal_service
                    .actor
                    .invalidate_peer_sessions(peer_str.clone())
                    .await
                {
                    tracing::warn!(peer = %peer_str, error = %e, "Failed to archive peer sessions");
                }
                // The verified channels that hand them our key. A server-only
                // removal is applied to the distribution set as fail-safe and
                // stays "pending signature" in the verified state (Design B):
                // only a signed RemoveMember + AddMember re-admits them.
                let repo = VerifiedMembershipRepository::new(self.database.clone());
                let channels = match repo.channels_distributing_to(&peer_str) {
                    Ok(channels) => channels,
                    Err(e) => {
                        tracing::warn!(peer = %peer_str, error = %e,
                            "Could not list the peer's channels; no sender-key rotation marked");
                        return;
                    }
                };
                for channel_id in &channels {
                    if let Err(e) = repo.exclude(channel_id, &peer_str) {
                        tracing::warn!(%channel_id, peer = %peer_str, error = %e,
                            "Failed to exclude a re-registered peer from distribution");
                    }
                    if let Err(e) = self.signal_service.mark_channel_for_rotation(channel_id) {
                        tracing::warn!(%channel_id, peer = %peer_str, error = %e,
                            "Failed to mark channel for sender-key rotation");
                    }
                }
                tracing::info!(peer = %peer_str, channels_marked = channels.len(),
                    "Peer account re-registered: shared channels marked for sender-key rotation");
            }
        }
    }

    /// Answer a `SenderKeyRequest` from `requester` — the authenticated sender
    /// of the request, never a field inside it. Anyone who knows a channel id
    /// can ask, so a requester that is not a current member is dropped before
    /// any key is created. `skdm_bytes` is the SKDM the gateway already built,
    /// or empty to fetch ours here. `Ok(false)` = dropped.
    pub async fn answer_sender_key_request(
        &self,
        channel_id: &ChannelId,
        requester: &UserId,
        skdm_bytes: &[u8],
    ) -> Result<bool, MessageError> {
        if !self.may_hold_sender_key(channel_id, requester) {
            tracing::debug!(%channel_id, %requester, "Dropping SenderKeyRequest from a non-member");
            return Ok(false);
        }
        let skdm = if skdm_bytes.is_empty() {
            self.create_sender_key_for_channel(channel_id).await?
        } else {
            use prost::Message as _;
            crate::proto::content_v1::SenderKeyDistribution::decode(skdm_bytes)
                .map_err(|e| MessageError::InvalidContent(format!("SKDM decode: {e}")))?
        };
        self.send_skdm_to_member(channel_id, requester, &skdm)
            .await?;
        Ok(true)
    }

    /// Create a sender key for a channel (generates or retrieves existing SKDM).
    ///
    /// This creates or retrieves our sender key for the specified channel.
    /// Used when responding to sender key requests.
    pub async fn create_sender_key_for_channel(
        &self,
        channel_id: &ChannelId,
    ) -> Result<crate::proto::content_v1::SenderKeyDistribution, MessageError> {
        self.signal_service
            .create_sender_key(channel_id)
            .await
            .map_err(|e| {
                MessageError::EncryptionFailed(format!("Failed to create sender key: {}", e))
            })
    }

    /// Send a sender key request to a channel member.
    ///
    /// Fanned to every device the target has, not just the primary. Sender
    /// keys are per (user, device); whichever device currently holds the
    /// sending sender key responds with its own SKDM. Without this fanout, a
    /// request would land only in device 1's inbox and dormant multi-device
    /// users whose device 1 is offline would never reply.
    pub async fn send_sender_key_request(
        &self,
        channel_id: &ChannelId,
        target_user_id: &UserId,
    ) -> Result<(), MessageError> {
        use crate::proto::content_v1::{self, SenderKeyRequest, content};
        use prost::Message;

        let mut auth_client = self.auth_client();

        // Build the SenderKeyRequest proto
        let timestamp = TimestampNanos::now().as_u64();
        let request = SenderKeyRequest {
            channel_id: channel_id.to_string(),
            target_user_id: target_user_id.to_string(),
            timestamp,
        };

        // Wrap in Content proto
        let mut content = content_v1::Content {
            content: Some(content::Content::SenderKeyRequest(request.clone())),
            ephemeral_reply_to: None,
            timestamp,
            padding: vec![],
        };
        Self::pad_content_to_bucket(&mut content);

        let plaintext = content.encode_to_vec();
        let target_id_str = target_user_id.to_string();

        // Sealed per device, UAK-authorized (pairwise sender keys are per
        // (user, device); we need a distinct envelope for each).
        let mut message_client = self.message_client();
        let outcome = self
            .deposit_sealed_to_all_devices(
                &mut auth_client,
                &mut message_client,
                &target_id_str,
                &plaintext,
            )
            .await;

        if outcome.delivered == 0 {
            return Err(MessageError::EncryptionFailed(
                "SenderKeyRequest failed to reach any target device".into(),
            ));
        }

        tracing::info!(
            channel_id = %channel_id,
            target_user_id = %target_user_id,
            "Sent sender key request via sealed sender (multi-device fanout)"
        );

        Ok(())
    }

    /// Sender-asserted timestamp (epoch nanos) of a group message, read from
    /// the `GroupEncrypted` envelope.
    ///
    /// That proto is the `EncryptedData` ciphertext the server stores and
    /// returns whole. Its `timestamp` sits beside the sender-key ciphertext,
    /// so a recipient that cannot decrypt still has it. The outer
    /// `Message.timestamp` is the server's receive time and is not a name
    /// for the message. `None` when the envelope does not carry one.
    fn group_envelope_timestamp_ns(proto_msg: &crate::proto::chat_v1::Message) -> Option<u64> {
        use crate::proto::chat_v1::message::content::Type;
        use crate::proto::content_v1::GroupEncrypted;
        use prost::Message as _;

        let Some(Type::Encrypted(enc)) = proto_msg.content.as_ref().and_then(|c| c.r#type.as_ref())
        else {
            return None;
        };
        let ge = GroupEncrypted::decode(enc.ciphertext.as_slice()).ok()?;
        (ge.timestamp != 0).then_some(ge.timestamp)
    }

    /// RECEIVER half of Signal's sender-key retry: ask the device that sent a
    /// group message we cannot decrypt to resend it pairwise.
    ///
    /// The SKDM a sender hands us starts at its chain's CURRENT iteration, so a
    /// message it sent before we held the key ("old counter") is unreadable
    /// here for good; only the sender can give us the content again
    /// (Signal-Desktop `onRetryRequest`). One request per (message, device).
    /// The DecryptionErrorMessage names the original by the sender-asserted
    /// envelope timestamp ([`Self::group_envelope_timestamp_ns`]), the same
    /// clock the sender stored as `sent_at`.
    pub async fn request_group_resend(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        device_id: u32,
        proto_msg: &crate::proto::chat_v1::Message,
        ciphertext: &[u8],
    ) {
        use crate::services::current_user::CurrentUserProvider;

        // Our own sender-key ciphertext from this very device is undecryptable
        // by design, not lost.
        let us = (
            self.settings_repo.current_user_id().ok(),
            self.settings_repo
                .get::<u32>(keys::CURRENT_DEVICE_ID)
                .ok()
                .flatten(),
        );
        if us == (Some(sender_id), Some(device_id)) {
            return;
        }
        // The clock the sender put on the envelope. The server's
        // Message.timestamp is receive time and does not match the sender's
        // sent_at, so a DecryptionErrorMessage named with it is dropped.
        let Some(named_ns) = Self::group_envelope_timestamp_ns(proto_msg) else {
            tracing::warn!(%channel_id, %sender_id, device_id, message_id = %proto_msg.id,
                "Group message envelope has no sender timestamp; not asking for a resend by server receive time");
            return;
        };
        // Already held — decrypted before (then "old counter" is a genuine
        // duplicate), or recovered by an earlier resend (stored under the
        // server id it carries, and the same (sender, sent_at)).
        let held = proto_msg
            .id
            .parse::<crate::domain::ids::MessageId>()
            .ok()
            .and_then(|id| self.message_repo.get(&id).ok().flatten())
            .is_some()
            || self
                .message_repo
                .get_by_conversation_sender_timestamp(&channel_id, &sender_id, named_ns)
                .ok()
                .flatten()
                .is_some();
        if held {
            return;
        }
        // A retired device will never answer, and the server refuses to queue
        // anything for it. The server receive time is what this check compares
        // to our device list.
        if self
            .is_retired_device(sender_id, device_id, proto_msg.timestamp)
            .await
        {
            tracing::info!(%channel_id, %sender_id, device_id, message_id = %proto_msg.id,
                "Group message from a retired device is unrecoverable; dropped");
            self.record_channel_delivery_issue(channel_id, sender_id, proto_msg);
            return;
        }
        // Recorded before the request goes out, so the answer cannot race it.
        // Keyed by the sender clock the resend's DataMessage will carry.
        self.solicited_resends
            .insert((channel_id, sender_id, named_ns / 1_000_000));
        {
            let mut asked = self.group_retry_requests.lock();
            // cleared wholesale at 4096 entries; a forgotten entry
            // costs at most one repeat request, which the sender answers again.
            if asked.len() >= 4096 {
                asked.clear();
            }
            if !asked.insert((proto_msg.id.clone(), device_id)) {
                return;
            }
        }

        let dem = match crate::services::signal::SignalService::group_decryption_error(
            ciphertext, named_ns, device_id,
        ) {
            Ok(dem) => dem,
            Err(e) => {
                tracing::warn!(%channel_id, %sender_id, device_id, error = %e,
                    "Could not build DecryptionErrorMessage for a group message");
                return;
            }
        };
        match self
            .send_decryption_error_report(&sender_id, device_id, dem)
            .await
        {
            Ok(()) => tracing::info!(
                %channel_id, %sender_id, device_id, message_id = %proto_msg.id,
                "Asked the sender to resend a group message we cannot decrypt"
            ),
            Err(MessageError::Network(e)) if e.is_unknown_destination_device() => {
                tracing::info!(%channel_id, %sender_id, device_id,
                    "Sender device is retired; dropping its parked group messages");
                self.retired_peer_devices
                    .lock()
                    .insert((sender_id, device_id), std::time::Instant::now());
                self.pending_group_decrypts
                    .lock()
                    .retain(|(_, s, d), _| (*s, *d) != (sender_id, device_id));
                self.record_channel_delivery_issue(channel_id, sender_id, proto_msg);
            }
            Err(e) => tracing::warn!(
                %channel_id, %sender_id, device_id, message_id = %proto_msg.id, error = %e,
                "Failed to send DecryptionErrorMessage for a group message"
            ),
        }
    }

    /// A peer's channel message that no device can ever give us: leave a
    /// DeliveryIssue entry where it would have been, so the gap is visible
    /// rather than silent. Idempotent per (channel, sender, time bucket), so a
    /// re-fetch on every sync does not repeat it. Our own lost messages get no
    /// entry: a reinstall would otherwise fill every channel with them.
    pub(crate) fn record_channel_delivery_issue(
        &self,
        channel_id: ChannelId,
        sender_id: UserId,
        proto_msg: &crate::proto::chat_v1::Message,
    ) {
        use crate::inbox::ack_policy::{FailureMarker, MarkerTarget, record_marker_then_ack};
        use crate::services::current_user::CurrentUserProvider;

        if self.settings_repo.current_user_id().ok() == Some(sender_id) {
            return;
        }
        let target = MarkerTarget {
            conversation_id: channel_id,
            conversation_type: crate::domain::message::ConversationType::Channel,
            peer_id: Some(sender_id),
            envelope_ts_ns: Self::group_envelope_timestamp_ns(proto_msg)
                .unwrap_or(proto_msg.timestamp),
        };
        let tx = self.event_bus.domain_sender();
        record_marker_then_ack(
            &self.message_repo,
            Some(&tx),
            target,
            FailureMarker::DeliveryIssue,
        );
    }

    /// Whether `(user, device)` is a device the server no longer lists, for a
    /// group message the server stamped `sent_at` (epoch nanos). Nobody holds
    /// such a device's sender key any more, so a message from it is
    /// unrecoverable: asking for its SKDM or a resend only reaches a device
    /// that does not exist.
    ///
    /// Our OWN devices are checked against the ones holding a prekey bundle
    /// (`fetch_own_device_ids`). A peer device counts as retired only once the server has
    /// refused a send to it, for [`PENDING_GROUP_DECRYPT_TTL`] — the server
    /// reuses a removed device's id, so the verdict must not stick.
    /// Fails open (not retired) when the list cannot be fetched.
    pub(crate) async fn is_retired_device(
        &self,
        user_id: UserId,
        device_id: u32,
        sent_at: u64,
    ) -> bool {
        use crate::services::current_user::CurrentUserProvider;

        if self.settings_repo.current_user_id().ok() != Some(user_id) {
            let mut retired = self.retired_peer_devices.lock();
            retired.retain(|_, at| at.elapsed() < PENDING_GROUP_DECRYPT_TTL);
            return retired.contains_key(&(user_id, device_id));
        }
        // Held across the fetch: a history sync full of one retired device's
        // messages costs one device-list fetch, not one per message.
        let mut cache = self.own_devices.lock().await;
        let now = TimestampNanos::now().as_u64();
        if let Some((fetched_at, ids)) = cache.as_ref() {
            if ids.contains(&device_id) {
                return false;
            }
            if !own_device_list_stale(*fetched_at, now, sent_at) {
                return true;
            }
        }
        match self.fetch_own_device_ids().await {
            Ok(ids) => {
                let retired = !ids.contains(&device_id);
                *cache = Some((now, ids));
                retired
            }
            Err(e) => {
                tracing::warn!(error = %e, device_id, "Could not list our devices; treating the device as current");
                false
            }
        }
    }

    /// SENDER half of Signal's sender-key retry: `peer_id`'s device could not
    /// decrypt the group message we sent at `timestamp_ms` (the sender-asserted
    /// envelope clock, epoch millis — not the server receive time). Resend its
    /// content pairwise to that one device, as a `DataMessage` stamped with
    /// that same `sent_at` so the peer files it under the same (sender, sent_at)
    /// as the copy it could not read.
    ///
    /// Bounded like Signal-Desktop's send log: only our own messages, only for
    /// 24h, and only to a device that was a recipient of that send and whose
    /// user is still a member. A device or member added later gets nothing,
    /// and a member who left gets nothing.
    pub async fn resend_on_decryption_error(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    ) {
        match self
            .resend_group_message(peer_id, peer_device_id, timestamp_ms)
            .await
        {
            Ok(true) => tracing::info!(%peer_id, peer_device_id, timestamp_ms,
                "Resent a group message pairwise after the member's DecryptionErrorMessage"),
            Ok(false) => tracing::info!(%peer_id, peer_device_id, timestamp_ms,
                "Group resend request matched nothing we may resend"),
            // The device is gone: the server refused the send, or — with no
            // session, so the single-device bundle fetch came first — holds no
            // bundle for it (NotFound, signal_prekeys.go). Same verdict as the
            // receiver half above.
            Err(MessageError::Network(e))
                if e.is_unknown_destination_device()
                    || matches!(e, crate::error::NetworkError::NotFound(_)) =>
            {
                tracing::info!(%peer_id, peer_device_id, timestamp_ms,
                    "Group resend: device retired on server");
                self.retired_peer_devices
                    .lock()
                    .insert((*peer_id, peer_device_id), std::time::Instant::now());
            }
            Err(e) => tracing::warn!(%peer_id, peer_device_id, timestamp_ms, error = %e,
                "Failed to resend a group message"),
        }
    }

    async fn resend_group_message(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    ) -> Result<bool, MessageError> {
        let Some(plaintext) = self.group_resend_content(peer_id, peer_device_id, timestamp_ms)?
        else {
            return Ok(false);
        };
        let sealed_body = self
            .signal_service
            .encrypt_message(
                &mut self.auth_client(),
                &peer_id.to_string(),
                peer_device_id,
                &plaintext,
            )
            .await
            .map_err(|e| match e {
                // Kept typed: a NotFound here is the retired-device verdict.
                crate::error::ServiceError::Network(e) => MessageError::Network(e),
                e => MessageError::EncryptionFailed(e.to_string()),
            })?;
        self.deposit_sealed_to_device(peer_id, peer_device_id, sealed_body)
            .await?;
        Ok(true)
    }

    /// The content `peer_id`'s `peer_device_id` may have back for our group
    /// message sent at `timestamp_ms`, or `None` when the retention and
    /// original-recipient bounds of [`Self::resend_on_decryption_error`] say no.
    fn group_resend_content(
        &self,
        peer_id: &UserId,
        peer_device_id: u32,
        timestamp_ms: u64,
    ) -> Result<Option<Vec<u8>>, MessageError> {
        use crate::domain::ids::DeviceId;
        use crate::domain::message::ConversationType;
        use crate::services::current_user::CurrentUserProvider;
        const RETENTION_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

        let us = self.settings_repo.current_user_id()?;
        // scans our newest 1000 messages; add a (sender, sent_at
        // range) repo query if one user sends more than that within 24h.
        let Some(message) = self
            .message_repo
            .list_by_sender(&us, 1000)?
            .into_iter()
            .find(|m| {
                m.sent_at / 1_000_000 == timestamp_ms
                    && m.conversation_type == ConversationType::Channel
                    && !m.is_deleted
            })
        else {
            return Ok(None);
        };
        if TimestampNanos::now()
            .as_u64()
            .saturating_sub(message.sent_at)
            > RETENTION_NS
        {
            return Ok(None);
        }
        let receipts =
            crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
                self.database.clone(),
            );
        let original = receipts.targets_for_send(
            message.conversation_id,
            message.conversation_type.clone(),
            us,
            message.sent_at,
        )?;
        // Still a member = in the verified distribution set; nothing while
        // the state is failed or forked.
        let Ok(verified) = self.group_gate().for_distribution(&message.conversation_id) else {
            return Ok(None);
        };
        let current = verified_user_ids(&verified);
        let device = DeviceId(peer_device_id);
        // Our own other devices are not receipt targets, yet they read our
        // channel messages through the same sender key. Their own snapshot
        // bounds them like any member: only a device that existed at send time.
        let allowed = if *peer_id == us {
            receipts
                .own_devices_for_send(
                    message.conversation_id,
                    message.conversation_type.clone(),
                    us,
                    message.sent_at,
                )?
                .contains(&device)
        } else {
            resend_targets(&original, &current)
                .into_iter()
                .any(|(user, id)| user == *peer_id && id == device)
        };
        if !allowed {
            return Ok(None);
        }
        Ok(self.rebuild_group_content(&message, &verified))
    }

    /// User-initiated retry of a stored private channel message: the same
    /// pairwise `DataMessage` a DecryptionErrorMessage gets, to every original
    /// recipient device whose user is still a member. Nothing is posted to the
    /// channel, and the receipt snapshot is not rewritten.
    pub(super) async fn resend_stored_channel_message(
        &self,
        message: &crate::domain::message::Message,
    ) -> Result<usize, MessageError> {
        use crate::services::current_user::CurrentUserProvider;

        let us = self.settings_repo.current_user_id()?;
        let original =
            crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
                self.database.clone(),
            )
            .targets_for_send(
                message.conversation_id,
                message.conversation_type.clone(),
                us,
                message.sent_at,
            )?;
        let verified = self
            .group_gate()
            .for_distribution(&message.conversation_id)?;
        let targets = resend_targets(&original, &verified_user_ids(&verified));
        if targets.is_empty() {
            return Err(MessageError::InvalidContent(
                "no original recipient of this message is still in the channel".into(),
            ));
        }
        let plaintext = self
            .rebuild_group_content(message, &verified)
            .ok_or_else(|| MessageError::InvalidContent("message cannot be resent".into()))?;
        crate::services::gateway::submission::mark_submission_started()?;
        self.deliver_group_resend_to(&plaintext, &targets).await
    }

    /// Encrypt `plaintext` for each `(user, device)` and deposit that one
    /// device's inbox. A device that fails does not stop the others. `Err`
    /// only when none were reached.
    async fn deliver_group_resend_to(
        &self,
        plaintext: &[u8],
        targets: &[(UserId, crate::domain::ids::DeviceId)],
    ) -> Result<usize, MessageError> {
        let mut delivered = 0usize;
        let mut error = None;
        for (user, device) in targets {
            let sealed = match self
                .signal_service
                .encrypt_message(
                    &mut self.auth_client(),
                    &user.to_string(),
                    device.0,
                    plaintext,
                )
                .await
            {
                Ok(sealed) => sealed,
                Err(e) => {
                    super::server_dm::record_deposit_error(
                        &mut error,
                        MessageError::from_signal_send(e, "group resend"),
                    );
                    continue;
                }
            };
            match self.deposit_sealed_to_device(user, device.0, sealed).await {
                Ok(()) => delivered += 1,
                Err(e) => super::server_dm::record_deposit_error(&mut error, e),
            }
        }
        if delivered == 0 {
            return Err(error.unwrap_or_else(|| {
                MessageError::InvalidContent("resend reached no device".into())
            }));
        }
        Ok(delivered)
    }

    /// The pairwise `Content` for one of our stored channel messages: the
    /// fields the original sender-key `DataMessage` carried (`core.rs`
    /// `send_message`), rebuilt from the local row. `None` for content that
    /// is not a user message.
    fn rebuild_group_content(
        &self,
        message: &crate::domain::message::Message,
        verified: &VerifiedChannel,
    ) -> Option<Vec<u8>> {
        use crate::domain::message::MessageContent;
        use crate::proto::content_v1::{Content, DataMessage, content};
        use prost::Message as _;

        let body = match &message.content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Mixed(m) => m.text.clone().unwrap_or_default(),
            _ => return None,
        };
        let attachments = message
            .attachments
            .iter()
            .filter_map(|id| self.attachment_repo.get(id).ok().flatten())
            .filter_map(|a| a.to_pointer())
            .collect();
        let quote = message
            .reply_to
            .and_then(|id| self.message_repo.get(&id).ok().flatten())
            .and_then(|orig| {
                Some(crate::services::gateway::QuoteIntent {
                    target_author_id: orig.sender_id?,
                    target_timestamp: orig.sent_at,
                    target_preview: message.reply_to_text.clone(),
                })
            });
        let ttl = self
            .ephemeral_service
            .get_state(&message.id)
            .ok()
            .flatten()
            .map(|e| e.ttl.as_secs() as u32);

        let mut dm = DataMessage {
            body,
            attachments,
            channel_id: message.conversation_id.to_string(),
            // The sender-asserted clock (DataMessage and GroupEncrypted
            // timestamp), not the server receive time. Members file this
            // message, and this resend, under that sent_at.
            timestamp: message.sent_at,
            quote: self.build_wire_quote(quote.as_ref()),
            thread_id: message
                .thread_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            mentioned_integrations: message
                .mentioned_integrations
                .iter()
                .map(|id| id.to_string())
                .collect(),
            targets_ai_bot: message.targets_ai_bot,
            ephemeral: ttl.is_some(),
            ephemeral_ttl_seconds: ttl.unwrap_or(0),
            // Our row's id is the server's (`send_message` stores the
            // server response), i.e. the id every other member holds.
            message_id: message.id.to_string(),
            ..Default::default()
        };
        stamp_group_data_message(&mut dm, verified);
        let mut content = Content {
            timestamp: dm.timestamp,
            content: Some(content::Content::DataMessage(dm)),
            ..Default::default()
        };
        Self::pad_content_to_bucket(&mut content);
        Some(content.encode_to_vec())
    }

    /// Deposit one sealed envelope in one device inbox
    /// (`SealedDelivery::NamedDevices`), UAK-authorized when we hold the
    /// peer's profile key (the same choice as `send_decryption_error_report`).
    /// A retired device comes back as `MismatchedDevices` with it `extra`
    /// (`NetworkError::is_unknown_destination_device`).
    pub(super) async fn deposit_sealed_to_device(
        &self,
        peer_id: &UserId,
        device_id: u32,
        sealed_body: Vec<u8>,
    ) -> Result<(), MessageError> {
        let message = crate::proto::chat_v1::SealedDeviceMessage {
            device_id,
            registration_id: 0,
            sealed_message: Some(crate::proto::chat_v1::SealedSenderMessage {
                sealed_body,
                server_key_id: self.signal_service.get_server_key_id(),
                timestamp: TimestampNanos::now_anonymized().as_u64(),
            }),
        };
        let peer = peer_id.to_string();
        let uak = self.sealed_access_key(&peer, false);
        if uak.is_none() {
            tracing::warn!(
                %peer_id,
                reason = "no stored profile key to derive the peer's UAK",
                "SEALED SENDER DEGRADED: group resend delivered identified \
                 (Basic auth) — the server WILL record us as the sender"
            );
        }
        let (_, uak_rejected) = self
            .send_sealed_batch(
                &mut self.message_client(),
                &peer,
                vec![message],
                crate::proto::chat_v1::SealedDelivery::NamedDevices,
                uak.as_ref(),
                &uuid::Uuid::new_v4().to_string(),
            )
            .await?;
        if uak_rejected {
            tracing::warn!(%peer_id, reason = "UAK from our stored profile key rejected", "SEALED SENDER DEGRADED: group resend delivered identified");
        }
        Ok(())
    }

    /// Pad a Content proto to the nearest standard bucket size for traffic analysis protection.
    ///
    /// The server validates that sealed sender ciphertexts match one of these plaintext bucket
    /// sizes: 256, 512, 1024, 4096 bytes. Without padding, message sizes leak event type info.
    pub(crate) fn pad_content_to_bucket(content: &mut crate::proto::content_v1::Content) {
        use prost::Message;
        const BUCKETS: [usize; 4] = [256, 512, 1024, 4096];

        // Get current size without padding
        content.padding = vec![];
        let base_size = content.encoded_len();

        // Find smallest bucket that fits
        let target = BUCKETS
            .iter()
            .copied()
            .find(|&b| b >= base_size)
            .unwrap_or(4096);

        if base_size >= target {
            return;
        }

        // Start with an overestimate, then trim to exact target.
        // Protobuf overhead for the padding field: 1 (tag) + varint(len) + len.
        let estimate = target.saturating_sub(base_size);
        content.padding = vec![0u8; estimate];

        while content.encoded_len() > target && !content.padding.is_empty() {
            content.padding.pop();
        }
        // If we undershot by 1 due to varint boundary, add back
        while content.encoded_len() < target {
            content.padding.push(0);
        }
    }
}

/// Original recipient devices whose user is still a member.
///
/// An empty `original` stays empty. A missing send-time record is not
/// "everyone who is a member now", and a user absent from `current_members`
/// (they left) is dropped with every device of theirs.
pub(super) fn resend_targets(
    original: &[(UserId, crate::domain::ids::DeviceId)],
    current_members: &[UserId],
) -> Vec<(UserId, crate::domain::ids::DeviceId)> {
    original
        .iter()
        .copied()
        .filter(|(user, _)| current_members.contains(user))
        .collect()
}

/// The verified distribution set as user ids.
fn verified_user_ids(verified: &VerifiedChannel) -> Vec<UserId> {
    verified
        .members()
        .iter()
        .filter_map(|u| u.parse().ok())
        .collect()
}

/// Device ids in a prekey-bundle response (multi-device list or the
/// single-bundle fallback).
fn bundle_device_ids(
    resp: crate::proto::chat_v1::GetPreKeyBundleResponse,
) -> std::collections::HashSet<u32> {
    use crate::proto::chat_v1::get_pre_key_bundle_response::Result;
    match resp.result {
        Some(Result::Devices(list)) => list.bundles.iter().map(|b| b.device_id).collect(),
        Some(Result::Bundle(b)) => [b.device_id].into(),
        None => Default::default(),
    }
}

/// How long a group message parked for a missing sender key waits for its
/// SKDM before we ask the sender to resend it instead.
const PENDING_GROUP_DECRYPT_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether a cached own-device list that lacks a device must be refetched
/// before calling that device retired: it is over a minute old, or the message
/// is newer than the list (a device linked since). 30s of slack for the gap
/// between the server's clock, which stamps `sent_at`, and ours.
fn own_device_list_stale(fetched_at: u64, now: u64, sent_at: u64) -> bool {
    const MAX_AGE: u64 = 60 * 1_000_000_000;
    const CLOCK_SLACK: u64 = 30 * 1_000_000_000;
    now.saturating_sub(fetched_at) > MAX_AGE || sent_at.saturating_add(CLOCK_SLACK) > fetched_at
}

/// Per sender: start of the current one-minute window, and resend requests
/// sent in it for parked messages the cap evicted.
pub(crate) type EvictionResendGate = std::collections::HashMap<UserId, (std::time::Instant, u32)>;

/// At most 10 eviction resend requests per sender per minute.
pub(crate) fn eviction_resend_due(
    gate: &mut EvictionResendGate,
    sender: UserId,
    now: std::time::Instant,
) -> bool {
    const WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
    const PER_WINDOW: u32 = 10;
    // never pruned; one entry per sender that ever overflowed the cap.
    let entry = gate.entry(sender).or_insert((now, 0));
    if now.duration_since(entry.0) >= WINDOW {
        *entry = (now, 0);
    }
    if entry.1 >= PER_WINDOW {
        return false;
    }
    entry.1 += 1;
    true
}

/// Backoff state per `(channel, sender, device)`: when the next request may go
/// out, and how many have gone out without an answer.
pub(crate) type SenderKeyRequestGate =
    std::collections::HashMap<(ChannelId, UserId, u32), (std::time::Instant, u32)>;

/// Whether `user` (whose sender-key devices are `devices`) has a device that
/// does not hold our current sender key, compared on (user, device,
/// registration id). A member other than us with no known device has never
/// been reached and lacks it; our own user with none has no other device.
fn lacks_sender_key(
    holders: &std::collections::HashSet<(String, u32, u32)>,
    user: &str,
    devices: &[(u32, Option<u32>)],
    is_self: bool,
) -> bool {
    if devices.is_empty() {
        return !is_self;
    }
    devices
        .iter()
        .any(|&(d, r)| r.is_none_or(|r| !holders.contains(&(user.to_string(), d, r))))
}

/// 30s, 60s, 120s ... capped at 15 min between unanswered requests.
fn sender_key_request_due(
    gate: &mut SenderKeyRequestGate,
    key: (ChannelId, UserId, u32),
    now: std::time::Instant,
) -> bool {
    const BASE: std::time::Duration = std::time::Duration::from_secs(30);
    const MAX: std::time::Duration = std::time::Duration::from_secs(15 * 60);
    let entry = gate.entry(key).or_insert((now, 0));
    if now < entry.0 {
        return false;
    }
    let delay = BASE.saturating_mul(1u32 << entry.1.min(10)).min(MAX);
    *entry = (now + delay, entry.1 + 1);
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn skdm_distribution_preserves_retryable_causes_and_validation_errors() {
        use crate::error::NetworkError;
        for cause in [
            NetworkError::Unavailable,
            NetworkError::Timeout,
            NetworkError::RateLimited,
            NetworkError::ConnectionFailed("offline".into()),
        ] {
            let error = sender_key_distribution_result(0, 2, Some(MessageError::Network(cause)))
                .unwrap_err();
            assert!(matches!(error, MessageError::Network(_)));
            assert_eq!(error.permanent_send_failure_reason(), None);
        }
        let error = sender_key_distribution_result(
            0,
            2,
            Some(MessageError::Network(NetworkError::InvalidRequest(
                "invalid recipient".into(),
            ))),
        )
        .unwrap_err();
        assert_eq!(
            error.permanent_send_failure_reason(),
            Some("Message rejected")
        );
        let mut failure = Some(error);
        super::super::server_dm::record_deposit_error(
            &mut failure,
            MessageError::Network(NetworkError::Timeout),
        );
        assert_eq!(
            sender_key_distribution_result(0, 2, failure)
                .unwrap_err()
                .permanent_send_failure_reason(),
            None
        );
        assert_eq!(
            sender_key_distribution_result(0, 2, None)
                .unwrap_err()
                .permanent_send_failure_reason(),
            None
        );
        assert!(sender_key_distribution_result(1, 2, None).is_ok());
        assert!(sender_key_distribution_result(0, 0, None).is_ok());
    }

    /// Own devices are the ones with a bundle; a response without any is none.
    #[test]
    fn own_devices_are_the_bundle_holders() {
        use crate::proto::chat_v1::{
            GetPreKeyBundleResponse, SignalPreKeyBundle, get_pre_key_bundle_response::Result,
        };
        let b = |device_id| SignalPreKeyBundle {
            device_id,
            ..Default::default()
        };
        let many = GetPreKeyBundleResponse {
            result: Some(Result::Devices(
                crate::proto::chat_v1::PreKeyBundleDeviceList {
                    bundles: vec![b(2), b(6), b(7), b(8)],
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        assert_eq!(super::bundle_device_ids(many), [2, 6, 7, 8].into());
        let one = GetPreKeyBundleResponse {
            result: Some(Result::Bundle(b(6))),
            ..Default::default()
        };
        assert_eq!(super::bundle_device_ids(one), [6].into());
        assert!(super::bundle_device_ids(GetPreKeyBundleResponse::default()).is_empty());
    }

    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn sender_key_request_is_sent_once_then_backs_off() {
        let mut gate = SenderKeyRequestGate::new();
        let key = (ChannelId::new(), UserId::new(), 6);
        let t0 = Instant::now();

        // A burst of misses from one history sync: one request.
        assert!(sender_key_request_due(&mut gate, key, t0));
        for _ in 0..20 {
            assert!(!sender_key_request_due(&mut gate, key, t0));
        }
        assert!(!sender_key_request_due(
            &mut gate,
            key,
            t0 + Duration::from_secs(29)
        ));
        // Unanswered: ask again after 30s, then the wait doubles.
        assert!(sender_key_request_due(
            &mut gate,
            key,
            t0 + Duration::from_secs(30)
        ));
        assert!(!sender_key_request_due(
            &mut gate,
            key,
            t0 + Duration::from_secs(89)
        ));
        assert!(sender_key_request_due(
            &mut gate,
            key,
            t0 + Duration::from_secs(90)
        ));

        // Another device of the same sender is gated on its own.
        assert!(sender_key_request_due(&mut gate, (key.0, key.1, 1), t0));

        // SKDM arrived (entry removed): the next miss asks immediately.
        gate.remove(&key);
        assert!(sender_key_request_due(
            &mut gate,
            key,
            t0 + Duration::from_secs(91)
        ));
    }

    /// Alice and Bob, each a client with its own store, both members of one
    /// channel. The TempDirs must outlive the clients.
    struct TwoMembers {
        alice: std::sync::Arc<crate::client::ChatterClient>,
        bob: std::sync::Arc<crate::client::ChatterClient>,
        alice_id: UserId,
        bob_id: UserId,
        channel_id: ChannelId,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    fn two_members() -> TwoMembers {
        use crate::client::ChatterClient;
        use crate::domain::channel::{
            Channel, ChannelType, MemberRole, Membership, NotificationPreference,
        };

        static MOCK_KEYRING: std::sync::Once = std::sync::Once::new();
        MOCK_KEYRING.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });
        let client = |dir: &tempfile::TempDir, user: UserId| {
            let c = ChatterClient::builder()
                .server_url("https://localhost:1")
                .data_dir(dir.path().to_str().unwrap())
                .build()
                .expect("build client");
            let s = &c.messages().settings_repo;
            s.set(keys::CURRENT_USER_ID, &user.to_string()).unwrap();
            s.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
            c
        };
        let (alice_id, bob_id) = (UserId::new(), UserId::new());
        let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (alice, bob) = (client(&da, alice_id), client(&db, bob_id));
        let channel_id = ChannelId::new();
        for c in [&alice, &bob] {
            let repo = &c.messages().channel_repo;
            repo.insert(&Channel::new(
                channel_id,
                "g".into(),
                ChannelType::Private,
                alice_id,
            ))
            .unwrap();
            for user_id in [alice_id, bob_id] {
                repo.insert_membership(&Membership {
                    channel_id,
                    user_id,
                    role: MemberRole::Member,
                    joined_at: chrono::Utc::now(),
                    last_read_message_id: None,
                    is_muted: false,
                    muted_until: None,
                    notification_preference: NotificationPreference::default(),
                })
                .unwrap();
            }
        }

        // Both hold the same verified state: Alice and Bob, revision 1.
        for c in [&alice, &bob] {
            seed_verified(c, channel_id, 1, &[alice_id, bob_id]);
        }

        TwoMembers {
            alice,
            bob,
            alice_id,
            bob_id,
            channel_id,
            _dirs: (da, db),
        }
    }

    /// Commit `members` as the channel's verified state at `revision` (the
    /// verifier itself is W2's and tested there; this only seeds storage).
    /// Returns the state hash.
    fn seed_verified(
        c: &crate::client::ChatterClient,
        channel_id: ChannelId,
        revision: u64,
        members: &[UserId],
    ) -> crate::services::channel::verified_state::StateHash {
        use crate::proto::chat_v1 as pb;
        use crate::services::channel::verified_state::{
            ChannelRole, Member, MemberStatus, MembershipState, PendingChange, Policy,
            VerifiedChange,
        };
        let state = MembershipState {
            channel_id: channel_id.to_string(),
            revision,
            members: members
                .iter()
                .map(|u| {
                    (
                        u.to_string(),
                        Member {
                            role: ChannelRole::Member,
                            status: MemberStatus::Active,
                            identity_key: [5; 33],
                        },
                    )
                })
                .collect(),
            policy: Policy::defaults(false),
            invites: Default::default(),
            disappearing_seconds: 0,
        };
        let hash = state.hash();
        let change = pb::ChannelChange {
            actions: vec![revision as u8],
            signature: vec![],
            invite_proof: vec![],
        };
        VerifiedMembershipRepository::new(c.messages().database.clone())
            .commit(
                &channel_id,
                &change,
                &VerifiedChange(PendingChange {
                    author_user_id: members[0].to_string(),
                    author_identity_key: [5; 33],
                    timestamp: 0,
                    new_state: state,
                    new_state_hash: hash,
                }),
            )
            .unwrap();
        hash
    }

    /// Signal hands sender keys to current group members only: a request from
    /// a non-member is dropped before any key is created; a member's is
    /// answered (its delivery fails here only because there is no server).
    #[tokio::test]
    async fn sender_key_request_is_answered_for_members_only() {
        let t = two_members();
        let alice = t.alice.messages();
        let outsider = UserId::new();

        assert!(matches!(
            alice
                .answer_sender_key_request(&t.channel_id, &outsider, &[])
                .await,
            Ok(false)
        ));
        assert!(
            !t.alice
                .signal()
                .has_sender_key(&t.channel_id)
                .await
                .unwrap(),
            "a non-member's request must not even create our key"
        );
        let skdm = t
            .alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        assert!(
            alice
                .send_skdm_to_member(&t.channel_id, &outsider, &skdm)
                .await
                .is_err()
        );

        let answered = alice
            .answer_sender_key_request(&t.channel_id, &t.bob_id, &[])
            .await;
        assert!(
            matches!(answered, Err(MessageError::EncryptionFailed(ref e)) if !e.contains("not a current member")),
            "a member's request goes to delivery: {answered:?}"
        );

        // Removed by a verified change: no longer answered.
        seed_verified(&t.alice, t.channel_id, 2, &[t.alice_id]);
        assert!(matches!(
            alice
                .answer_sender_key_request(&t.channel_id, &t.bob_id, &[])
                .await,
            Ok(false)
        ));
    }

    /// Connected production send conversion -> delayed server envelope ->
    /// missing SKDM -> authenticated retry -> controls in both directions.
    #[tokio::test]
    async fn delayed_channel_receive_preserves_authenticated_reaction_identity() {
        delayed_reaction_fixture(false).await;
        delayed_reaction_fixture(true).await;
    }

    async fn delayed_reaction_fixture(legacy: bool) {
        use crate::domain::{
            MessageId,
            message::{ConversationType, Message, MessageContent},
            reaction::ReactionScope,
        };
        use crate::inbox::{polling::device::dispatch_control_content, state::InboxStats};
        use crate::proto::content_v1;
        use crate::services::inbox_processor::InboxEventProcessor;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
        use prost::Message as _;
        let t = two_members();
        let sent = TimestampNanos::now().as_u64();
        let transport = sent + 445_413_938;
        let id = MessageId::new();
        let skdm = t
            .alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        let dm = content_v1::DataMessage {
            body: "bold target".into(),
            message_id: if legacy {
                id.to_string()
            } else {
                String::new()
            },
            channel_id: t.channel_id.to_string(),
            timestamp: sent,
            ..Default::default()
        };
        let plaintext = content_v1::Content {
            timestamp: sent,
            content: Some(content_v1::content::Content::DataMessage(dm)),
            ..Default::default()
        }
        .encode_to_vec();
        let (encrypted, _) = t
            .alice
            .messages()
            .prepare_encrypted_content(
                &t.channel_id,
                &plaintext,
                crate::proto::chat_v1::message::Content {
                    r#type: Some(crate::proto::chat_v1::message::content::Type::Text(
                        "bold target".into(),
                    )),
                },
                true,
                sent,
            )
            .await
            .unwrap();
        let Some(crate::proto::chat_v1::message::content::Type::Encrypted(ciphertext)) =
            encrypted.r#type
        else {
            panic!("production send must encrypt");
        };
        let ge = content_v1::GroupEncrypted::decode(ciphertext.ciphertext.as_slice()).unwrap();
        let envelope = server_copy(&t, &id, transport, &ge);
        let mut outgoing = super::super::conversion::proto_to_outgoing_channel_message(
            &envelope,
            t.alice_id,
            ConversationType::Channel,
            sent,
            true,
        )
        .unwrap();
        outgoing.content = MessageContent::Text("bold target".into());
        t.alice
            .messages()
            .message_repo
            .insert_outgoing(&outgoing)
            .unwrap();
        // Plain public messages already use the server's accepted row clock.
        let plain = super::super::conversion::proto_to_outgoing_channel_message(
            &crate::proto::chat_v1::Message {
                content: Some(crate::proto::chat_v1::message::Content {
                    r#type: Some(crate::proto::chat_v1::message::content::Type::Text(
                        "public".into(),
                    )),
                }),
                ..envelope.clone()
            },
            t.alice_id,
            ConversationType::Channel,
            sent,
            false,
        )
        .unwrap();
        assert_eq!(plain.sent_at, transport);
        let inbound = ChannelInbound::new(t.bob.messages().clone(), t.bob.attachments().clone());
        assert_eq!(
            inbound.ingest(&envelope, t.channel_id, false).await,
            Ingested::DeferredPendingSenderKey
        );
        let scope = ReactionScope::new(t.channel_id, ConversationType::Channel);
        if legacy {
            let mut old = Message::new_text(
                t.channel_id,
                ConversationType::Channel,
                Some(t.alice_id),
                "[Encrypted message]".into(),
                transport,
            );
            old.id = id;
            t.bob.messages().store_message_local(&old).unwrap();
            t.bob
                .messages()
                .record_reaction_at(&scope, &t.alice_id, transport, &t.bob_id, "😂", false, 1)
                .unwrap();
        }
        // Target-missing is retained, never guessed from a nearby clock.
        assert!(
            !t.bob
                .messages()
                .record_inbound_reaction(&scope, &t.alice_id, &t.alice_id, sent, "😂", false, 2)
                .unwrap()
        );
        assert!(
            t.bob
                .messages()
                .message_repo
                .get_by_reaction_scope(&scope, &t.alice_id, sent)
                .unwrap()
                .is_none()
        );
        t.bob
            .signal()
            .process_sender_key(&t.alice_id, 1, &skdm)
            .await
            .unwrap();
        t.bob
            .messages()
            .retry_pending_group_decrypts(t.channel_id, t.alice_id, 1)
            .await;
        let retained = if legacy { transport } else { sent };
        let received = t.bob.messages().get_message_local(&id).unwrap().unwrap();
        assert_eq!(received.sent_at, retained);
        assert_eq!(received.as_text(), Some("bold target"));
        assert_eq!(
            t.bob
                .messages()
                .message_repo
                .authenticated_channel_reaction_clock(&received)
                .unwrap(),
            sent
        );
        assert_eq!(
            t.bob
                .messages()
                .message_repo
                .get_reaction_counts(&scope, &t.alice_id, sent)
                .unwrap()[0]
                .count,
            if legacy { 2 } else { 1 }
        );
        // Existing local ledger ownership survives; each peer's new control
        // addresses the canonical authenticated clock through real dispatch.
        t.alice
            .messages()
            .record_reaction_at(&scope, &t.alice_id, sent, &t.alice_id, "😂", false, 2)
            .unwrap();
        for (receiver, actor) in [(&t.alice, t.bob_id), (&t.bob, t.bob_id)] {
            let content = crate::services::build_reaction_content_with_channel(
                "😂",
                false,
                &t.alice_id.to_string(),
                sent,
                Some(&t.channel_id.to_string()),
            );
            let (tx, mut rx) = tokio::sync::broadcast::channel(8);
            let stats = Arc::new(parking_lot::RwLock::new(InboxStats::default()));
            dispatch_control_content(
                content.content.unwrap(),
                &actor,
                1,
                "reaction",
                transport + 100,
                &tx,
                &stats,
            );
            InboxEventProcessor::from_client(receiver)
                .process(&rx.try_recv().unwrap())
                .await;
        }
        for client in [&t.alice, &t.bob] {
            let snapshot = crate::services::conversation::ConversationHandle::new(
                crate::state::ActiveConversation::Channel(t.channel_id),
                client.clone(),
            )
            .reaction_snapshot(&t.alice_id, sent)
            .unwrap()
            .unwrap();
            assert_eq!(
                snapshot.sent_at,
                if Arc::ptr_eq(client, &t.bob) {
                    retained
                } else {
                    sent
                }
            );
            assert_eq!(snapshot.counts[0].count, 2);
            assert!(snapshot.own_emojis.contains(&"😂".to_string()));
        }
        // Cached ciphertext does not authenticate its outer timestamp. Replay
        // preserves the exact mapping, cached row, and aggregate.
        inbound
            .ingest_resynced(&envelope, t.channel_id)
            .await
            .unwrap();
        assert_eq!(
            t.bob
                .messages()
                .get_message_local(&id)
                .unwrap()
                .unwrap()
                .sent_at,
            retained
        );
        let reopened = crate::client::ChatterClient::builder()
            .server_url("https://localhost:1")
            .data_dir(t._dirs.1.path().to_str().unwrap())
            .build()
            .unwrap();
        let repo =
            crate::storage::repositories::unified_message_repo::UnifiedMessageRepository::new(
                reopened.messages().database.clone(),
            );
        assert_eq!(
            repo.resolve_channel_reaction_clock(&scope, &t.alice_id, sent)
                .unwrap(),
            retained
        );
        assert_eq!(
            repo.get_reaction_counts(&scope, &t.alice_id, sent).unwrap()[0].count,
            2
        );
        assert!(
            repo.get_by_reaction_scope(&scope, &t.alice_id, sent + 1)
                .unwrap()
                .is_none()
        );
        let foreign = ReactionScope::new(ChannelId::new(), ConversationType::Channel);
        assert!(
            repo.get_by_reaction_scope(&foreign, &t.alice_id, sent)
                .unwrap()
                .is_none()
        );
        for invalid in [
            Message {
                sender_id: Some(UserId::new()),
                ..received.clone()
            },
            Message {
                conversation_id: ChannelId::new(),
                ..received.clone()
            },
            Message {
                conversation_type: ConversationType::ServerDm,
                ..received.clone()
            },
        ] {
            assert!(repo.bind_authenticated_channel_clock(&invalid).is_err());
        }
        if legacy {
            assert!(
                repo.bind_authenticated_channel_clock(&Message {
                    sent_at: sent + 1,
                    ..received.clone()
                })
                .is_err()
            );
        }
        assert_eq!(
            repo.get_reaction_counts(&scope, &t.alice_id, sent).unwrap()[0].count,
            2
        );
    }

    /// Alice's group message as the server hands it to Bob.
    fn server_copy(
        t: &TwoMembers,
        id: &crate::domain::ids::MessageId,
        ts: u64,
        ge: &crate::proto::content_v1::GroupEncrypted,
    ) -> crate::proto::chat_v1::Message {
        use crate::proto::chat_v1;
        use prost::Message as _;
        chat_v1::Message {
            id: id.to_string(),
            channel_id: t.channel_id.to_string(),
            author_id: t.alice_id.to_string(),
            author_device_id: 1,
            timestamp: ts,
            content: Some(chat_v1::message::Content {
                r#type: Some(chat_v1::message::content::Type::Encrypted(
                    chat_v1::EncryptedData {
                        ciphertext: ge.encode_to_vec(),
                        ..Default::default()
                    },
                )),
            }),
            ..Default::default()
        }
    }

    /// Signal's sender-key retry, end to end between two real clients short of
    /// the network hop: Bob cannot read Alice's message sent before her SKDM,
    /// asks with a DecryptionErrorMessage, and Alice's pairwise resend lands in
    /// Bob's channel under the message's server id and (sender, sent_at).
    #[tokio::test]
    async fn pre_skdm_group_message_is_recovered_by_dem_and_resend() {
        use crate::domain::ids::MessageId;
        use crate::domain::message::{ConversationType, Message, MessageContent};
        use crate::proto::content_v1;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
        use prost::Message as _;

        let t = two_members();
        let (alice, bob) = (&t.alice, &t.bob);
        let (alice_id, bob_id, channel_id) = (t.alice_id, t.bob_id, t.channel_id);

        // Alice's client clock and the server's receive time differ by more
        // than a millisecond, and both are inside the 24h resend window.
        // The row and the GroupEncrypted envelope use her clock; the outer
        // Message.timestamp is the server's.
        let server_ts = TimestampNanos::now().as_u64();
        let client_ts = server_ts - 5_000_000_000;
        assert_ne!(client_ts / 1_000_000, server_ts / 1_000_000);

        let plaintext = content_v1::Content {
            content: Some(content_v1::content::Content::DataMessage(
                content_v1::DataMessage {
                    body: "sent before the SKDM".into(),
                    channel_id: channel_id.to_string(),
                    timestamp: client_ts,
                    ..Default::default()
                },
            )),
            timestamp: client_ts,
            ..Default::default()
        }
        .encode_to_vec();
        let (mut ge, _) = alice
            .signal()
            .encrypt_for_group(&channel_id, &plaintext)
            .await
            .unwrap();
        ge.timestamp = client_ts;
        let message_id = MessageId::new();
        alice
            .messages()
            .message_repo
            .insert_outgoing(&Message {
                id: message_id,
                ..Message::new_text(
                    channel_id,
                    ConversationType::Channel,
                    Some(alice_id),
                    "sent before the SKDM".into(),
                    client_ts,
                )
            })
            .unwrap();
        // Bob's device 1 was a recipient of this send. Without that record the
        // resend is refused: membership alone is not enough.
        crate::storage::repositories::message_receipt_repo::MessageReceiptRepository::new(
            alice.messages().database.clone(),
        )
        .snapshot_for_send(
            channel_id,
            ConversationType::Channel,
            alice_id,
            client_ts,
            &[(bob_id, crate::domain::ids::DeviceId(1))],
        )
        .unwrap();

        // Bob gets Alice's SKDM only now: it starts past the message.
        let skdm = alice.signal().create_sender_key(&channel_id).await.unwrap();
        bob.signal()
            .process_sender_key(&alice_id, 1, &skdm)
            .await
            .unwrap();
        let server_copy = server_copy(&t, &message_id, server_ts, &ge);
        let inbound = ChannelInbound::new(bob.messages().clone(), bob.attachments().clone());
        assert_eq!(
            inbound.ingest(&server_copy, channel_id, false).await,
            Ingested::RetryableFailure,
            "positive control: the pre-SKDM message is unreadable on its own"
        );

        // Bob's DEM names a sender-key message: no ratchet key, so Alice's
        // gateway resends instead of archiving their 1:1 session. It names
        // Alice's envelope clock, not the server receive time.
        let dem_bytes = crate::services::signal::SignalService::group_decryption_error(
            &ge.ciphertext,
            client_ts,
            1,
        )
        .unwrap();
        let dem =
            libsignal_protocol::DecryptionErrorMessage::try_from(dem_bytes.as_slice()).unwrap();
        assert!(dem.ratchet_key().is_none());
        assert_eq!(dem.device_id(), 1);
        let ts_ms = dem.timestamp().epoch_millis();
        assert_eq!(ts_ms, client_ts / 1_000_000);
        assert_ne!(ts_ms, server_ts / 1_000_000);

        // Alice's resend bounds: a non-member gets nothing back, and the
        // server's receive time is not a key she stored. Bob's other device
        // was not a recipient of the original send.
        assert!(
            alice
                .messages()
                .group_resend_content(&UserId::new(), 1, ts_ms)
                .unwrap()
                .is_none()
        );
        assert!(
            alice
                .messages()
                .group_resend_content(&bob_id, 1, server_ts / 1_000_000)
                .unwrap()
                .is_none(),
            "the server timestamp must not match the sender's sent_at"
        );
        assert!(
            alice
                .messages()
                .group_resend_content(&bob_id, 2, ts_ms)
                .unwrap()
                .is_none(),
            "a device that was not in the original send gets nothing"
        );
        let resent = alice
            .messages()
            .group_resend_content(&bob_id, 1, ts_ms)
            .unwrap()
            .expect("Alice finds the message she sent at that timestamp");

        // Bob ingests it through the pairwise sealed DataMessage path.
        let Some(content_v1::content::Content::DataMessage(dm)) =
            content_v1::Content::decode(resent.as_slice())
                .unwrap()
                .content
        else {
            panic!("resend is not a DataMessage");
        };
        let m = bob.messages();
        let gateway = crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        );
        // Bob's ingest above asked Alice for this message.
        for _ in 0..10 {
            if !m.solicited_resends.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            m.solicited_resends
                .contains(&(channel_id, alice_id, client_ts / 1_000_000)),
            "the resend request is keyed by the sender's envelope timestamp"
        );
        assert!(
            !m.solicited_resends
                .contains(&(channel_id, alice_id, server_ts / 1_000_000)),
            "the resend request must not be keyed by the server receive time"
        );
        gateway
            .store_data_message(dm, Some(alice_id))
            .await
            .unwrap();
        let stored = m
            .message_repo
            .get_by_conversation_sender_timestamp(&channel_id, &alice_id, client_ts)
            .unwrap()
            .expect("the recovered message is filed under the sender's sent_at");
        assert_eq!(
            stored.content,
            MessageContent::Text("sent before the SKDM".into())
        );
        assert_eq!(
            stored.id, message_id,
            "the recovered message keeps the server id the other members hold"
        );

        // A later re-fetch of the server copy neither duplicates it nor asks
        // again (the first ingest above already recorded its one request).
        assert_eq!(
            inbound.ingest(&server_copy, channel_id, false).await,
            Ingested::Dropped
        );
        m.group_retry_requests.lock().clear();
        m.request_group_resend(channel_id, alice_id, 1, &server_copy, &ge.ciphertext)
            .await;
        assert!(m.group_retry_requests.lock().is_empty());
    }

    /// A resend is the original recipient set intersected with who is still a
    /// member. Carol joined after the send, Bob's second device was never a
    /// recipient, and Bob himself is excluded once he leaves.
    #[tokio::test]
    async fn resend_refuses_a_later_joiner_a_later_device_and_a_departed_member() {
        use crate::domain::ids::DeviceId;
        use crate::domain::message::{ConversationType, Message};
        use crate::storage::repositories::message_receipt_repo::MessageReceiptRepository;

        let t = two_members();
        let sent_at = TimestampNanos::now().as_u64();
        let alice = t.alice.messages();
        alice
            .message_repo
            .insert_outgoing(&Message::new_text(
                t.channel_id,
                ConversationType::Channel,
                Some(t.alice_id),
                "before carol joined".into(),
                sent_at,
            ))
            .unwrap();
        MessageReceiptRepository::new(alice.database.clone())
            .snapshot_for_send(
                t.channel_id,
                ConversationType::Channel,
                t.alice_id,
                sent_at,
                &[(t.bob_id, DeviceId(1))],
            )
            .unwrap();

        let carol = UserId::new();
        seed_verified(&t.alice, t.channel_id, 2, &[t.alice_id, t.bob_id, carol]);

        let ts_ms = sent_at / 1_000_000;
        assert!(
            alice
                .group_resend_content(&carol, 1, ts_ms)
                .unwrap()
                .is_none(),
            "a member who was not a recipient of the original send gets nothing"
        );
        assert!(
            alice
                .group_resend_content(&t.bob_id, 2, ts_ms)
                .unwrap()
                .is_none(),
            "a device added after the send gets nothing"
        );
        assert!(
            alice
                .group_resend_content(&t.bob_id, 1, ts_ms)
                .unwrap()
                .is_some(),
            "the original recipient device still in the channel is resent to"
        );
        // Our own devices: device 7 existed at send time, device 9 was linked later.
        let receipts = MessageReceiptRepository::new(alice.database.clone());
        receipts
            .snapshot_own_devices_for_send(
                t.channel_id,
                ConversationType::Channel,
                t.alice_id,
                sent_at,
                &[DeviceId(7)],
            )
            .unwrap();
        assert_eq!(
            receipts
                .targets_for_send(t.channel_id, ConversationType::Channel, t.alice_id, sent_at)
                .unwrap(),
            vec![(t.bob_id, DeviceId(1))],
            "own devices never join the receipt targets the aggregate counts"
        );
        assert!(
            alice
                .group_resend_content(&t.alice_id, 7, ts_ms)
                .unwrap()
                .is_some(),
            "our own device present at send time is resent to"
        );
        assert!(
            alice
                .group_resend_content(&t.alice_id, 9, ts_ms)
                .unwrap()
                .is_none(),
            "our own device linked after the send gets nothing"
        );
        assert!(
            alice
                .group_resend_content(&t.alice_id, 1, ts_ms)
                .unwrap()
                .is_none(),
            "this device never asks itself"
        );

        seed_verified(&t.alice, t.channel_id, 3, &[t.alice_id, carol]);
        assert!(
            alice
                .group_resend_content(&t.bob_id, 1, ts_ms)
                .unwrap()
                .is_none(),
            "a member who left since the send gets nothing"
        );
    }

    #[test]
    fn resend_targets_drop_departed_users_and_an_empty_original_set() {
        use crate::domain::ids::DeviceId;
        let (alice, bob, carol) = (UserId::new(), UserId::new(), UserId::new());
        let original = [(bob, DeviceId(1)), (bob, DeviceId(2)), (carol, DeviceId(1))];
        assert_eq!(
            super::resend_targets(&original, &[alice, bob]),
            vec![(bob, DeviceId(1)), (bob, DeviceId(2))]
        );
        assert!(super::resend_targets(&[], &[alice, bob]).is_empty());
    }

    /// F72: a channel member's SKDM carries their profile key to the other
    /// members — and only to them: not to a non-member who asked, not to a
    /// member they blocked — and the receiver keeps it without a friendship.
    #[tokio::test]
    async fn skdm_shares_profile_key_with_channel_members_only() {
        use crate::domain::user::{Friendship, FriendshipStatus};
        use crate::proto::content_v1;
        use prost::Message as _;

        let t = two_members();
        let (alice, bob) = (&t.alice, &t.bob);
        alice
            .messages()
            .settings_repo
            .set_bytes(keys::PROFILE_KEY, &[9; 32])
            .unwrap();
        let skdm = alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        let carried = |to: &UserId| {
            let verified = alice
                .messages()
                .group_gate()
                .for_distribution(&t.channel_id)
                .unwrap();
            let bytes =
                alice
                    .messages()
                    .skdm_plaintext(&t.channel_id, &verified, &skdm, &to.to_string());
            match content_v1::Content::decode(bytes.as_slice())
                .unwrap()
                .content
            {
                Some(content_v1::content::Content::SenderKey(s)) => s.profile_key,
                _ => panic!("not an SKDM"),
            }
        };

        let key = carried(&t.bob_id).expect("a co-member gets Alice's profile key");
        assert_eq!(key, vec![9; 32]);
        assert!(
            carried(&UserId::new()).is_none(),
            "a non-member who asked gets none"
        );
        assert!(
            carried(&t.alice_id).is_none(),
            "our own devices already hold it"
        );

        // "My contacts": a co-member who is not Alice's friend gets none.
        use crate::services::profile_visibility::ProfileVisibility;
        ProfileVisibility::Contacts
            .store(&alice.messages().settings_repo)
            .unwrap();
        assert!(
            carried(&t.bob_id).is_none(),
            "a non-friend co-member under my contacts gets none"
        );
        ProfileVisibility::Everyone
            .store(&alice.messages().settings_repo)
            .unwrap();

        // Bob is not Alice's friend: the key still lands, once.
        let m = bob.messages();
        let gateway = crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        );
        assert!(gateway.store_skdm_profile_key(&t.alice_id, Some(&key)));
        assert!(
            !gateway.store_skdm_profile_key(&t.alice_id, Some(&key)),
            "same key is no change"
        );
        assert_eq!(
            m.user_repo
                .get_friend_profile_key(&t.bob_id, &t.alice_id)
                .unwrap(),
            Some([9; 32])
        );

        // Once Alice blocks Bob, her SKDM to him carries no key.
        let am = alice.messages();
        am.user_repo.ensure_user_exists(&t.alice_id).unwrap();
        am.user_repo.ensure_user_exists(&t.bob_id).unwrap();
        am.user_repo
            .insert_friendship(&Friendship {
                requester_id: t.alice_id,
                addressee_id: t.bob_id,
                status: FriendshipStatus::Blocked,
                created_at: chrono::Utc::now(),
            })
            .unwrap();
        assert!(carried(&t.bob_id).is_none(), "a blocked member gets none");
    }

    /// A message parked for a sender key that never comes is not dropped
    /// silently when its wait runs out: Bob asks Alice to resend it, once.
    #[tokio::test]
    async fn parked_group_message_requests_resend_when_its_sender_key_never_arrives() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};

        let t = two_members();
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, b"lost key")
            .await
            .unwrap();
        let message_id = MessageId::new();
        let copy = server_copy(&t, &message_id, TimestampNanos::now().as_u64(), &ge);
        let m = t.bob.messages();
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());
        assert_eq!(
            inbound.ingest(&copy, t.channel_id, false).await,
            Ingested::DeferredPendingSenderKey
        );
        assert!(
            m.group_retry_requests.lock().is_empty(),
            "no resend request while the SKDM may still come"
        );

        m.expire_pending_group_decrypt(t.channel_id, t.alice_id, 1, &copy.id)
            .await;
        assert!(m.pending_group_decrypts.lock().is_empty());
        assert!(
            m.group_retry_requests
                .lock()
                .contains(&(copy.id.clone(), 1)),
            "the expired message is handed to request_group_resend"
        );

        // Nothing left parked: a later expiry asks nothing.
        m.group_retry_requests.lock().clear();
        m.expire_pending_group_decrypt(t.channel_id, t.alice_id, 1, &copy.id)
            .await;
        assert!(m.group_retry_requests.lock().is_empty());
    }

    /// A resync page with a message parked for its sender key goes on to the
    /// next message: one undecryptable message used to fail the whole resync,
    /// so the cursor never moved and every channel stalled behind it.
    #[tokio::test]
    async fn deferred_resync_requires_retry_while_later_messages_remain_ingestible() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::ChannelInbound;

        let t = two_members();
        let m = t.bob.messages();
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, b"no key yet")
            .await
            .unwrap();
        let parked = server_copy(&t, &MessageId::new(), TimestampNanos::now().as_u64(), &ge);
        let next_id = MessageId::new();
        let next = crate::proto::chat_v1::Message {
            id: next_id.to_string(),
            channel_id: t.channel_id.to_string(),
            author_id: t.alice_id.to_string(),
            author_device_id: 1,
            timestamp: TimestampNanos::now().as_u64(),
            content: Some(crate::proto::chat_v1::message::Content {
                r#type: Some(crate::proto::chat_v1::message::content::Type::Text(
                    "after".into(),
                )),
            }),
            ..Default::default()
        };

        assert!(
            matches!(
                inbound.ingest_resynced(&parked, t.channel_id).await,
                Err(MessageError::ResyncPending)
            ),
            "a missing sender key is a pending application gap, not malformed truth"
        );
        inbound
            .ingest_resynced(&next, t.channel_id)
            .await
            .expect("the following usable message remains independently ingestible");
        assert_eq!(
            m.pending_group_decrypts
                .lock()
                .get(&(t.channel_id, t.alice_id, 1))
                .map(Vec::len),
            Some(1)
        );
        assert!(
            m.get_message_local(&next_id).unwrap().is_some(),
            "the following message is stored"
        );
    }

    /// Our own other device is a sender like any other: a message it sent that
    /// this device has no key for asks for its SKDM, and on expiry its resend.
    #[tokio::test]
    async fn message_from_our_other_device_without_a_key_requests_it() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};

        let t = two_members();
        // Bob's store is Alice's second device (7); Alice's client is device 1.
        let m = t.bob.messages();
        m.settings_repo
            .set(keys::CURRENT_USER_ID, &t.alice_id.to_string())
            .unwrap();
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &7u32).unwrap();
        *m.own_devices.lock().await = Some((TimestampNanos::now().as_u64(), [1, 7].into()));
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, b"from device 1")
            .await
            .unwrap();
        let copy = server_copy(&t, &MessageId::new(), TimestampNanos::now().as_u64(), &ge);
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());

        assert_eq!(
            inbound.ingest(&copy, t.channel_id, false).await,
            Ingested::DeferredPendingSenderKey
        );
        assert!(
            !m.should_request_sender_key(t.channel_id, t.alice_id, 1),
            "the ingest already sent the SenderKeyRequest, so the gate is closed"
        );
        m.expire_pending_group_decrypt(t.channel_id, t.alice_id, 1, &copy.id)
            .await;
        assert!(
            m.group_retry_requests
                .lock()
                .contains(&(copy.id.clone(), 1)),
            "an expired own-device message asks that device to resend it"
        );
    }

    /// A peer's message from a device that no longer exists leaves a visible
    /// DeliveryIssue entry, once however often it is re-fetched.
    #[tokio::test]
    async fn message_from_a_retired_peer_device_leaves_one_delivery_issue() {
        use crate::domain::ids::MessageId;
        use crate::domain::message::{ConversationType, MessageContent, SystemMessage};
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};

        let t = two_members();
        let m = t.bob.messages();
        m.retired_peer_devices
            .lock()
            .insert((t.alice_id, 1), std::time::Instant::now());
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, b"gone")
            .await
            .unwrap();
        let copy = server_copy(&t, &MessageId::new(), TimestampNanos::now().as_u64(), &ge);
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());

        for _ in 0..2 {
            assert_eq!(
                inbound.ingest(&copy, t.channel_id, false).await,
                Ingested::Dropped
            );
        }
        let issues: Vec<_> = m
            .message_repo
            .list_for_conversation(&t.channel_id, &ConversationType::Channel, None, 50)
            .unwrap()
            .into_iter()
            .filter(|msg| {
                matches!(
                    msg.content,
                    MessageContent::System(SystemMessage::DeliveryIssue)
                )
            })
            .collect();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].sender_id, Some(t.alice_id));
    }

    /// An identified sender is the server's claim alone, and a first identity
    /// key is trusted on first use: the identified path must never start a
    /// session, or the server could pin its own key for a peer we never met.
    #[tokio::test]
    async fn identified_message_never_starts_a_session() {
        let t = two_members();
        let m = t.bob.messages();
        let gateway = crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        );
        let refused = |r: Result<_, crate::error::ServiceError>| {
            matches!(r, Err(crate::error::ServiceError::Crypto(crate::error::CryptoError::Protocol(ref e)))
                if e.contains("No session available for an identified sender"))
        };
        assert!(refused(
            gateway
                .decrypt_identified_message(b"any", &t.alice_id, 1)
                .await
        ));

        // Positive control: with a session the message reaches libsignal.
        let record = libsignal_protocol::SessionRecord::new_fresh()
            .serialize()
            .unwrap();
        m.signal_service
            .key_store
            .store_session(&format!("{}:1", t.alice_id), &record)
            .unwrap();
        assert!(!refused(
            gateway
                .decrypt_identified_message(b"any", &t.alice_id, 1)
                .await
        ));
    }

    #[test]
    fn own_device_list_is_refetched_only_when_it_may_be_out_of_date() {
        let s = 1_000_000_000u64;
        let fetched = 10_000 * s;
        // A message from long before the list: the list is authoritative.
        assert!(!own_device_list_stale(
            fetched,
            fetched + 10 * s,
            fetched - 3600 * s
        ));
        // A message newer than the list (or within clock slack): maybe a new device.
        assert!(own_device_list_stale(
            fetched,
            fetched + 10 * s,
            fetched + 5 * s
        ));
        assert!(own_device_list_stale(
            fetched,
            fetched + 10 * s,
            fetched - 10 * s
        ));
        // A list over a minute old is refetched regardless.
        assert!(own_device_list_stale(
            fetched,
            fetched + 61 * s,
            fetched - 3600 * s
        ));
    }

    #[test]
    fn eviction_resends_are_capped_per_sender_per_minute() {
        let mut gate = EvictionResendGate::new();
        let (a, b) = (UserId::new(), UserId::new());
        let t0 = Instant::now();
        for _ in 0..10 {
            assert!(eviction_resend_due(&mut gate, a, t0));
        }
        assert!(!eviction_resend_due(
            &mut gate,
            a,
            t0 + Duration::from_secs(59)
        ));
        assert!(
            eviction_resend_due(&mut gate, b, t0),
            "another sender has its own budget"
        );
        assert!(
            eviction_resend_due(&mut gate, a, t0 + Duration::from_secs(60)),
            "a new minute, a new budget"
        );
    }

    /// F41: a channel message sent from ANOTHER of our devices reaches this
    /// one live and must be decrypted and stored (read — it is ours), not
    /// skipped as "already on screen", which is true only for this device's
    /// own sends.
    #[tokio::test]
    async fn live_message_from_our_other_device_is_stored_as_read() {
        use crate::domain::ids::MessageId;
        use crate::domain::message::{ConversationType, MessageContent};
        use crate::proto::content_v1;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
        use prost::Message as _;

        let t = two_members();
        // Bob's store is Alice's second device (7); Alice's client is device 1.
        let m = t.bob.messages();
        m.settings_repo
            .set(keys::CURRENT_USER_ID, &t.alice_id.to_string())
            .unwrap();
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &7u32).unwrap();
        let skdm = t
            .alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        t.bob
            .signal()
            .process_sender_key(&t.alice_id, 1, &skdm)
            .await
            .unwrap();

        let body = content_v1::Content {
            content: Some(content_v1::content::Content::DataMessage(
                content_v1::DataMessage {
                    body: "from my tablet".into(),
                    channel_id: t.channel_id.to_string(),
                    timestamp: TimestampNanos::now().as_u64(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }
        .encode_to_vec();
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, &body)
            .await
            .unwrap();
        let id = MessageId::new();
        let copy = server_copy(&t, &id, TimestampNanos::now().as_u64(), &ge);
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());

        assert_eq!(
            inbound.ingest(&copy, t.channel_id, true).await,
            Ingested::Stored
        );
        let stored = m.get_message_local(&id).unwrap().expect("stored");
        assert!(matches!(stored.content, MessageContent::Text(ref b) if b == "from my tablet"));
        assert_eq!(
            m.message_repo
                .get_unread_count(&t.channel_id, &ConversationType::Channel)
                .unwrap(),
            0,
            "our own message is never unread"
        );

        // Positive control: the same message on the device that sent it is skipped.
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &1u32).unwrap();
        let copy = server_copy(&t, &MessageId::new(), TimestampNanos::now().as_u64(), &ge);
        assert_eq!(
            inbound.ingest(&copy, t.channel_id, true).await,
            Ingested::OwnMessage
        );
    }

    /// Our own previous install (device 1, no longer listed by the server)
    /// sent channel messages before it was retired. Nobody holds its sender
    /// key: the message is dropped, not parked, and nothing is asked of a
    /// device that does not exist.
    #[tokio::test]
    async fn message_from_our_own_retired_device_is_dropped_not_parked() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};

        let t = two_members();
        // Bob's store is Alice's new install: same user, device 6.
        let m = t.bob.messages();
        m.settings_repo
            .set(keys::CURRENT_USER_ID, &t.alice_id.to_string())
            .unwrap();
        m.settings_repo.set(keys::CURRENT_DEVICE_ID, &6u32).unwrap();
        let now = TimestampNanos::now().as_u64();
        *m.own_devices.lock().await = Some((now, [6].into()));

        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, b"old install")
            .await
            .unwrap();
        let copy = server_copy(&t, &MessageId::new(), now - 3600 * 1_000_000_000, &ge);
        let inbound = ChannelInbound::new(m.clone(), t.bob.attachments().clone());
        assert_eq!(
            inbound.ingest(&copy, t.channel_id, false).await,
            Ingested::Dropped
        );
        assert!(m.pending_group_decrypts.lock().is_empty());
        m.request_group_resend(t.channel_id, t.alice_id, 1, &copy, &ge.ciphertext)
            .await;
        assert!(
            m.group_retry_requests.lock().is_empty(),
            "no DEM to a retired device"
        );
        assert!(m.solicited_resends.is_empty());

        // Positive control: the same message from a device the server lists
        // waits for its sender key as usual.
        *m.own_devices.lock().await = Some((now, [1, 6].into()));
        assert_eq!(
            inbound.ingest(&copy, t.channel_id, false).await,
            Ingested::DeferredPendingSenderKey
        );
    }

    /// A pairwise resend may take the server id it carries only when this
    /// device asked that sender for the message at that sent_at, once.
    #[tokio::test]
    async fn resend_keeps_its_carried_id_only_when_solicited() {
        use crate::domain::ids::MessageId;
        use crate::proto::content_v1::DataMessage;

        let t = two_members();
        let m = t.bob.messages();
        let gateway = crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        );
        let resend = |id: MessageId, ts: u64| DataMessage {
            body: "resent".into(),
            channel_id: t.channel_id.to_string(),
            timestamp: ts,
            message_id: id.to_string(),
            ..Default::default()
        };
        let stored_id = |ts: u64| {
            m.message_repo
                .get_by_conversation_sender_timestamp(&t.channel_id, &t.alice_id, ts)
                .unwrap()
                .expect("stored")
                .id
        };

        // Unsolicited: another member's id cannot be squatted.
        let foreign = MessageId::new();
        let ts1 = TimestampNanos::now().as_u64();
        gateway
            .store_data_message(resend(foreign, ts1), Some(t.alice_id))
            .await
            .unwrap();
        assert_ne!(stored_id(ts1), foreign);
        assert!(m.get_message_local(&foreign).unwrap().is_none());

        // Solicited from ANOTHER sender: still not honoured for Alice.
        let ts2 = ts1 + 7_000_000;
        m.solicited_resends
            .insert((t.channel_id, t.bob_id, ts2 / 1_000_000));
        let id2 = MessageId::new();
        gateway
            .store_data_message(resend(id2, ts2), Some(t.alice_id))
            .await
            .unwrap();
        assert_ne!(stored_id(ts2), id2);

        // Solicited from Alice at that sent_at: keeps the server id, and the
        // request is consumed.
        let ts3 = ts1 + 14_000_000;
        m.solicited_resends
            .insert((t.channel_id, t.alice_id, ts3 / 1_000_000));
        let id3 = MessageId::new();
        gateway
            .store_data_message(resend(id3, ts3), Some(t.alice_id))
            .await
            .unwrap();
        assert_eq!(stored_id(ts3), id3);
        assert!(
            !m.solicited_resends
                .contains(&(t.channel_id, t.alice_id, ts3 / 1_000_000))
        );
        // A solicited authenticated UUID can bind an old transport-clock row;
        // an unsolicited or foreign-author claim cannot change its history.
        use crate::domain::{
            message::{ConversationType, Message},
            reaction::ReactionScope,
        };
        let sent = ts3 + 20_000_000;
        let transport = sent + 445_413_938;
        let mut old = Message::new_text(
            t.channel_id,
            ConversationType::Channel,
            Some(t.alice_id),
            "[Encrypted message]".into(),
            transport,
        );
        old.id = MessageId::new();
        m.store_message_local(&old).unwrap();
        let scope = ReactionScope::of(&old);
        m.record_reaction_at(&scope, &t.alice_id, transport, &t.bob_id, "👍", false, 1)
            .unwrap();
        assert!(
            !m.record_inbound_reaction(&scope, &t.alice_id, &t.alice_id, sent, "👍", false, 2)
                .unwrap()
        );
        m.solicited_resends
            .insert((t.channel_id, t.alice_id, sent / 1_000_000));
        gateway
            .store_data_message(resend(old.id, sent), Some(t.alice_id))
            .await
            .unwrap();
        let healed = m.get_message_local(&old.id).unwrap().unwrap();
        assert_eq!(healed.sent_at, transport);
        assert_eq!(healed.as_text(), Some("resent"));
        assert_eq!(
            m.message_repo
                .get_reaction_counts(&scope, &t.alice_id, sent)
                .unwrap()[0]
                .count,
            2
        );
        let mut second = old.clone();
        second.id = MessageId::new();
        second.sent_at = transport + 1;
        m.store_message_local(&second).unwrap();
        assert!(
            m.message_repo
                .bind_authenticated_channel_clock(&Message {
                    sent_at: sent,
                    ..second.clone()
                })
                .is_err()
        );
        assert_eq!(
            m.get_message_local(&second.id).unwrap().unwrap().sent_at,
            transport + 1
        );
        assert_eq!(
            m.message_repo
                .resolve_channel_reaction_clock(&scope, &t.alice_id, sent)
                .unwrap(),
            transport
        );
    }

    /// Past the parked cap the oldest message is evicted — and its sender is
    /// asked to resend it rather than it vanishing.
    #[tokio::test]
    async fn parked_message_evicted_by_the_cap_requests_a_resend() {
        use crate::domain::ids::MessageId;

        let t = two_members();
        let m = t.bob.messages();
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, b"flood")
            .await
            .unwrap();
        let ts = TimestampNanos::now().as_u64();
        let copies: Vec<_> = (0..513)
            .map(|i| server_copy(&t, &MessageId::new(), ts + i, &ge))
            .collect();
        for c in &copies {
            m.defer_pending_group_decrypt(t.channel_id, t.alice_id, 1, c.clone());
        }
        assert_eq!(
            m.pending_group_decrypts
                .lock()
                .values()
                .map(Vec::len)
                .sum::<usize>(),
            512
        );
        let oldest = (copies[0].id.clone(), 1);
        for _ in 0..10 {
            if m.group_retry_requests.lock().contains(&oldest) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            m.group_retry_requests.lock().contains(&oldest),
            "the evicted message is handed to request_group_resend"
        );
    }

    /// Give `client` a usable session with `(user, device)` whose remote
    /// registration id is `registration_id`, as a bundle fetch would.
    async fn store_session(
        client: &crate::client::ChatterClient,
        user: UserId,
        device: u32,
        registration_id: u32,
    ) {
        use libsignal_protocol::{
            DeviceId, IdentityKeyPair, InMemSignalProtocolStore, KeyPair, PreKeyBundle,
            ProtocolAddress, SessionStore, kem, process_prekey_bundle,
        };
        use rand::SeedableRng;
        let mut rng =
            rand::rngs::StdRng::seed_from_u64(u64::from(registration_id) << 8 | u64::from(device));
        let mut ours =
            InMemSignalProtocolStore::new(IdentityKeyPair::generate(&mut rng), 1).unwrap();
        let theirs = IdentityKeyPair::generate(&mut rng);
        let signed = KeyPair::generate(&mut rng);
        let kyber = kem::KeyPair::generate(kem::KeyType::Kyber1024, &mut rng);
        let sign = |key: &[u8], rng: &mut rand::rngs::StdRng| {
            theirs
                .private_key()
                .calculate_signature(key, rng)
                .unwrap()
                .to_vec()
        };
        let device_id = DeviceId::try_from(device).unwrap();
        let bundle = PreKeyBundle::new(
            registration_id,
            device_id,
            None,
            1u32.into(),
            signed.public_key,
            sign(&signed.public_key.serialize(), &mut rng),
            1u32.into(),
            kyber.public_key.clone(),
            sign(&kyber.public_key.serialize(), &mut rng),
            *theirs.identity_key(),
        )
        .unwrap();
        let address = ProtocolAddress::new(user.to_string(), device_id);
        let local = ProtocolAddress::new("me".into(), DeviceId::try_from(1u32).unwrap());
        let mut identities = ours.clone();
        process_prekey_bundle(
            &address,
            &local,
            &mut ours,
            &mut identities,
            &bundle,
            std::time::SystemTime::now(),
            &mut rng,
        )
        .await
        .unwrap();
        let record = ours.load_session(&address).await.unwrap().unwrap();
        client
            .signal()
            .key_store
            .store_session(&format!("{user}:{device}"), &record.serialize().unwrap())
            .unwrap();
    }

    /// The outcome of an SKDM deposit that reached `devices`, each
    /// `(device, registration id of the session that encrypted to it)`.
    fn reached(devices: &[(u32, u32)]) -> super::super::server_dm::DepositOutcome {
        super::super::server_dm::DepositOutcome {
            delivered: devices.len(),
            deposits: devices.iter().map(|&(d, _)| (d, 0)).collect(),
            registration_ids: devices.iter().copied().collect(),
            ..Default::default()
        }
    }

    /// The registration id comes from the session itself — the bundle's,
    /// for a session we initiated — and a device with no session has none,
    /// never 0 (0 would switch off the server's stale-device check).
    #[tokio::test]
    async fn session_registration_id_is_the_sessions_and_absent_without_one() {
        let t = two_members();
        let signal = &t.alice.messages().signal_service;
        let bob = t.bob_id.to_string();
        assert_eq!(signal.session_registration_id(&bob, 1), None);
        store_session(&t.alice, t.bob_id, 1, 4321).await;
        assert_eq!(signal.session_registration_id(&bob, 1), Some(4321));
        assert_eq!(signal.session_registration_id(&bob, 2), None);
    }

    /// A device whose registration id is unknown lacks our key, and an
    /// SKDM deposit without one is not recorded as held.
    #[tokio::test]
    async fn device_without_registration_id_is_never_a_holder() {
        let t = two_members();
        let alice = t.alice.messages();
        let (channel, bob) = (t.channel_id, t.bob_id.to_string());
        let holders = [(bob.clone(), 1, 101)].into();
        assert!(lacks_sender_key(&holders, &bob, &[(1, None)], false));
        assert!(!lacks_sender_key(&holders, &bob, &[(1, Some(101))], false));

        let mut outcome = reached(&[(1, 101)]);
        outcome.registration_ids.clear();
        alice.record_sender_key_holders(&channel, &bob, &outcome);
        assert!(
            alice
                .signal_service
                .sender_key_holders(&channel)
                .unwrap()
                .is_empty()
        );
    }

    /// Our own device list, as `own_device_ids` would have fetched it.
    async fn set_own_devices(client: &crate::client::ChatterClient, ids: &[u32]) {
        *client.messages().own_devices.lock().await = Some((
            TimestampNanos::now().as_u64(),
            ids.iter().copied().collect(),
        ));
    }

    /// Per-device holders, Signal's `memberDevices`: a member's device that
    /// does not hold our current key puts the member up for the pre-send
    /// SKDM; one whose every device holds it is left alone. Our own other
    /// devices are tracked the same way.
    #[tokio::test]
    async fn member_device_without_our_key_gets_the_skdm_and_holders_do_not() {
        let t = two_members();
        let alice = t.alice.messages();
        let (channel, bob) = (t.channel_id, t.bob_id.to_string());
        let me = t.alice_id.to_string();
        set_own_devices(&t.alice, &[1]).await;
        let lacking = || async {
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap()
        };

        // Never reached (no session): the fan-out establishes Bob's sessions.
        assert_eq!(lacking().await, vec![t.bob_id]);

        store_session(&t.alice, t.bob_id, 1, 101).await;
        store_session(&t.alice, t.bob_id, 2, 202).await;
        // The SKDM reached device 1 only: device 2 still lacks the key.
        alice.record_sender_key_holders(&channel, &bob, &reached(&[(1, 101)]));
        assert_eq!(
            alice.signal_service.sender_key_holders(&channel).unwrap(),
            [(bob.clone(), 1, 101)].into()
        );
        assert_eq!(
            lacking().await,
            vec![t.bob_id],
            "Bob's device 2 gets the SKDM on the next send"
        );
        alice.record_sender_key_holders(&channel, &bob, &reached(&[(2, 202)]));
        assert!(
            lacking().await.is_empty(),
            "every device of Bob's holds it: no SKDM"
        );

        // Our own newly linked device 3 (no session yet), then reached.
        set_own_devices(&t.alice, &[1, 3]).await;
        assert_eq!(lacking().await, vec![t.alice_id]);
        store_session(&t.alice, t.alice_id, 3, 303).await;
        alice.record_sender_key_holders(&channel, &me, &reached(&[(3, 303)]));
        assert!(lacking().await.is_empty());
    }

    /// Signal compares devices on (id, registration id): a device that
    /// re-registered under the same id is a new device and gets the SKDM;
    /// its delivery replaces the old install's entry.
    #[tokio::test]
    async fn re_registered_device_counts_as_new() {
        let t = two_members();
        let alice = t.alice.messages();
        let (channel, bob) = (t.channel_id, t.bob_id.to_string());
        let me = t.alice_id.to_string();
        set_own_devices(&t.alice, &[1]).await;
        store_session(&t.alice, t.bob_id, 1, 101).await;
        alice.record_sender_key_holders(&channel, &bob, &reached(&[(1, 101)]));
        assert!(
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap()
                .is_empty()
        );

        store_session(&t.alice, t.bob_id, 1, 909).await;
        assert_eq!(
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap(),
            vec![t.bob_id]
        );
        alice.record_sender_key_holders(&channel, &bob, &reached(&[(1, 909)]));
        assert_eq!(
            alice.signal_service.sender_key_holders(&channel).unwrap(),
            [(bob, 1, 909)].into()
        );
        assert!(
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A new key has reached nobody: rotation clears every holder. The old
    /// per-user entries are not recognized.
    #[tokio::test]
    async fn rotation_clears_the_holder_set() {
        let t = two_members();
        let alice = t.alice.messages();
        let (channel, bob) = (t.channel_id, t.bob_id.to_string());
        let me = t.alice_id.to_string();
        set_own_devices(&t.alice, &[1]).await;
        store_session(&t.alice, t.bob_id, 1, 101).await;
        t.alice.signal().create_sender_key(&channel).await.unwrap();
        alice.record_sender_key_holders(&channel, &bob, &reached(&[(1, 101)]));
        assert!(
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap()
                .is_empty()
        );

        t.alice.signal().rotate_sender_key(&channel).await.unwrap();
        assert!(
            alice
                .signal_service
                .sender_key_holders(&channel)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            alice
                .members_lacking_sender_key(&channel, &me, 1)
                .await
                .unwrap(),
            vec![t.bob_id]
        );

        alice
            .settings_repo
            .set_string(
                &format!("sender_key_delivered:{channel}"),
                &format!("{bob},{me}.3"),
            )
            .unwrap();
        assert!(
            alice
                .signal_service
                .sender_key_holders(&channel)
                .unwrap()
                .is_empty()
        );
    }

    // ---------------------------------------------------- verified key gating

    fn bob_gateway(t: &TwoMembers) -> crate::services::gateway::MessageGateway {
        let m = t.bob.messages();
        crate::services::gateway::MessageGateway::new(
            (*m.signal_service).clone(),
            m.message_repo.clone(),
            m.channel_repo.clone(),
            m.user_repo.clone(),
            m.settings_repo.clone(),
            m.solicited_resends.clone(),
        )
    }

    /// Alice's SKDM exactly as it leaves for Bob: stamped with her verified state.
    async fn stamped_skdm(t: &TwoMembers) -> crate::proto::content_v1::SenderKeyDistribution {
        use crate::proto::content_v1;
        use prost::Message as _;
        let alice = t.alice.messages();
        let skdm = t
            .alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        let verified = alice.group_gate().for_distribution(&t.channel_id).unwrap();
        let bytes = alice.skdm_plaintext(&t.channel_id, &verified, &skdm, &t.bob_id.to_string());
        match content_v1::Content::decode(bytes.as_slice())
            .unwrap()
            .content
        {
            Some(content_v1::content::Content::SenderKey(s)) => s,
            _ => panic!("not an SKDM"),
        }
    }

    /// The ghost acceptance test (Design B): a user the server lists as a
    /// member, but who is not in the verified state, gets no SKDM, its
    /// SenderKeyRequest is not answered, and no profile key reaches it.
    #[tokio::test]
    async fn server_only_member_gets_no_sender_key() {
        use crate::domain::channel::{MemberRole, Membership, NotificationPreference};
        let t = two_members();
        let alice = t.alice.messages();
        let ghost = UserId::new();
        alice
            .channel_repo
            .insert_membership(&Membership {
                channel_id: t.channel_id,
                user_id: ghost,
                role: MemberRole::Admin,
                joined_at: chrono::Utc::now(),
                last_read_message_id: None,
                is_muted: false,
                muted_until: None,
                notification_preference: NotificationPreference::default(),
            })
            .unwrap();
        assert!(
            alice
                .channel_repo
                .get_membership(&t.channel_id, &ghost)
                .unwrap()
                .is_some(),
            "the server's view has the ghost"
        );

        set_own_devices(&t.alice, &[1]).await;
        let me = t.alice_id.to_string();
        assert_eq!(
            alice
                .members_lacking_sender_key(&t.channel_id, &me, 1)
                .await
                .unwrap(),
            vec![t.bob_id],
            "the pre-send top-up names verified members only"
        );
        let mut fanout = alice
            .group_gate()
            .for_distribution(&t.channel_id)
            .unwrap()
            .members();
        fanout.sort();
        let mut expected = vec![t.alice_id.to_string(), t.bob_id.to_string()];
        expected.sort();
        assert_eq!(fanout, expected, "the SKDM fan-out set excludes the ghost");

        assert!(matches!(
            alice
                .answer_sender_key_request(&t.channel_id, &ghost, &[])
                .await,
            Ok(false)
        ));
        assert!(
            !t.alice
                .signal()
                .has_sender_key(&t.channel_id)
                .await
                .unwrap(),
            "the ghost's request must not even create our key"
        );
        let skdm = t
            .alice
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        assert!(
            matches!(alice.send_skdm_to_member(&t.channel_id, &ghost, &skdm).await,
            Err(MessageError::EncryptionFailed(ref e)) if e.contains("not a verified member"))
        );
        let verified = alice.group_gate().for_distribution(&t.channel_id).unwrap();
        alice
            .settings_repo
            .set_bytes(keys::PROFILE_KEY, &[9; 32])
            .unwrap();
        assert!(
            alice
                .skdm_profile_key(&verified, &ghost.to_string())
                .is_none()
        );
        assert!(
            alice
                .skdm_profile_key(&verified, &t.bob_id.to_string())
                .is_some()
        );
    }

    /// No verified state yet: nothing is distributed and nothing may be sent.
    #[tokio::test]
    async fn channel_without_verified_state_is_send_blocked() {
        let t = two_members();
        let alice = t.alice.messages();
        let other = ChannelId::new();
        let err = alice.group_gate().for_distribution(&other).unwrap_err();
        assert!(matches!(
            err,
            MessageError::MembershipNotVerified { status: None, .. }
        ));
        assert_eq!(
            err.permanent_send_failure_reason(),
            Some("Membership not verified")
        );
        assert!(
            alice
                .members_lacking_sender_key(&other, &t.alice_id.to_string(), 1)
                .await
                .is_err()
        );
        assert!(matches!(
            alice
                .answer_sender_key_request(&other, &t.bob_id, &[])
                .await,
            Ok(false)
        ));
    }

    /// An SKDM from a sender outside the verified state is not stored; a
    /// verified member's is.
    #[tokio::test]
    async fn skdm_from_a_non_member_is_not_stored() {
        use crate::proto::content_v1;
        use prost::Message as _;
        let t = two_members();
        let skdm = stamped_skdm(&t).await;
        let ghost = UserId::new();
        let gateway = bob_gateway(&t);
        let plaintext = content_v1::Content {
            content: Some(content_v1::content::Content::DataMessage(
                content_v1::DataMessage::default(),
            )),
            ..Default::default()
        }
        .encode_to_vec();
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, &plaintext)
            .await
            .unwrap();

        assert_eq!(
            gateway
                .accept_skdm(&ghost, 1, &t.channel_id, &skdm)
                .await
                .unwrap(),
            None
        );
        assert!(
            t.bob
                .signal()
                .decrypt_from_group(&ghost, 1, &ge)
                .await
                .is_err(),
            "nothing stored for the ghost"
        );

        assert!(
            gateway
                .accept_skdm(&t.alice_id, 1, &t.channel_id, &skdm)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            t.bob
                .signal()
                .decrypt_from_group(&t.alice_id, 1, &ge)
                .await
                .is_ok()
        );
    }

    /// Same revision, different hash from a verified member: Fork. The
    /// channel stops distributing and sending until acknowledged.
    #[tokio::test]
    async fn fork_from_a_member_blocks_distribution_and_send() {
        use crate::services::channel::verified_state::VerificationStatus;
        let t = two_members();
        // Alice and Bob diverge at revision 2 (the server showed them different changes).
        seed_verified(&t.alice, t.channel_id, 2, &[t.alice_id, t.bob_id]);
        seed_verified(
            &t.bob,
            t.channel_id,
            2,
            &[t.alice_id, t.bob_id, UserId::new()],
        );
        let skdm = stamped_skdm(&t).await;
        let gateway = bob_gateway(&t);
        let bob = t.bob.messages();

        // A non-member claiming a fork changes nothing.
        assert_eq!(
            gateway
                .accept_skdm(&UserId::new(), 1, &t.channel_id, &skdm)
                .await
                .unwrap(),
            None
        );
        assert!(bob.group_gate().for_distribution(&t.channel_id).is_ok());

        assert!(
            gateway
                .accept_skdm(&t.alice_id, 1, &t.channel_id, &skdm)
                .await
                .unwrap()
                .is_some(),
            "a member's key is still installed: receiving is not distributing"
        );
        assert!(matches!(
            bob.group_gate().for_distribution(&t.channel_id),
            Err(MessageError::MembershipNotVerified {
                status: Some(VerificationStatus::Fork),
                ..
            })
        ));
        assert!(matches!(
            bob.answer_sender_key_request(&t.channel_id, &t.alice_id, &[])
                .await,
            Ok(false)
        ));
        assert!(
            bob.members_lacking_sender_key(&t.channel_id, &t.bob_id.to_string(), 1)
                .await
                .is_err(),
            "the pre-send top-up (and with it the send) is refused"
        );
        let skdm = t
            .bob
            .signal()
            .create_sender_key(&t.channel_id)
            .await
            .unwrap();
        assert!(matches!(
            bob.send_skdm_to_member(&t.channel_id, &t.alice_id, &skdm)
                .await,
            Err(MessageError::MembershipNotVerified { .. })
        ));
    }

    struct RecordingSync(tokio::sync::mpsc::UnboundedSender<(ChannelId, u64)>);

    #[async_trait::async_trait]
    impl ChannelStateSync for RecordingSync {
        async fn fetch_channel_changes(&self, channel_id: ChannelId, peer_revision: u64) {
            let _ = self.0.send((channel_id, peer_revision));
        }
    }

    /// Stamps round-trip: an SKDM and a group DataMessage carry the sender's
    /// verified (revision, state_hash); equal state is in sync, and a peer
    /// ahead of us has the channel service fetch the changes.
    #[tokio::test]
    async fn stamps_round_trip_and_a_peer_ahead_triggers_a_fetch() {
        use crate::services::channel::verified_state::PeerStateCheck;
        let t = two_members();
        let alice_state = t
            .alice
            .messages()
            .group_gate()
            .for_distribution(&t.channel_id)
            .unwrap();
        let skdm = stamped_skdm(&t).await;
        assert_eq!(
            (skdm.state_revision, skdm.state_hash.clone()),
            (1, alice_state.state_hash().to_vec())
        );

        let mut dm = crate::proto::content_v1::DataMessage::default();
        stamp_group_data_message(&mut dm, &alice_state);
        let dm = <crate::proto::content_v1::DataMessage as prost::Message>::decode(
            prost::Message::encode_to_vec(&dm).as_slice(),
        )
        .unwrap();
        assert_eq!(
            (dm.group_state_revision, dm.group_state_hash.as_slice()),
            (1, alice_state.state_hash().as_slice())
        );

        let bob = t.bob.messages();
        let gate = bob.group_gate();
        assert_eq!(
            gate.cross_check(
                &t.channel_id,
                true,
                dm.group_state_revision,
                &dm.group_state_hash
            ),
            Some(PeerStateCheck::InSync)
        );
        assert!(gate.take_peer_ahead(&t.channel_id).is_none());

        // Alice moves to revision 3; Bob is still at 1.
        seed_verified(&t.alice, t.channel_id, 2, &[t.alice_id, t.bob_id]);
        seed_verified(&t.alice, t.channel_id, 3, &[t.alice_id, t.bob_id]);
        let skdm = stamped_skdm(&t).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bob.set_channel_state_sync(Arc::new(RecordingSync(tx)));
        bob_gateway(&t)
            .accept_skdm(&t.alice_id, 1, &t.channel_id, &skdm)
            .await
            .unwrap();
        // The SKDM's SenderKeyReceived drains the parked queue, and the record.
        bob.retry_pending_group_decrypts(t.channel_id, t.alice_id, 1)
            .await;
        assert_eq!(rx.recv().await, Some((t.channel_id, 3)));
        assert!(
            gate.take_peer_ahead(&t.channel_id).is_none(),
            "drained once"
        );
        assert!(
            bob.group_gate().for_distribution(&t.channel_id).is_ok(),
            "ahead is not a fork"
        );

        // A peer behind with a hash we recorded for that revision: in sync
        // with its past, no alert.
        let back = t.alice.messages().group_gate();
        assert_eq!(
            back.cross_check(&t.channel_id, true, 1, &alice_state.state_hash()),
            Some(PeerStateCheck::PeerBehind)
        );
    }

    /// A group message from a sender outside the verified state is dropped
    /// before decrypting (no SenderKeyRequest, no resend request).
    #[tokio::test]
    async fn group_message_from_a_non_member_is_dropped() {
        use crate::domain::ids::MessageId;
        use crate::services::message::channel_inbound::{ChannelInbound, Ingested};
        let t = two_members();
        let plaintext = b"from the server's ghost";
        let (ge, _) = t
            .alice
            .signal()
            .encrypt_for_group(&t.channel_id, plaintext)
            .await
            .unwrap();
        let mut copy = server_copy(&t, &MessageId::new(), TimestampNanos::now().as_u64(), &ge);
        let ghost = UserId::new();
        copy.author_id = ghost.to_string();
        let inbound = ChannelInbound::new(t.bob.messages().clone(), t.bob.attachments().clone());
        assert_eq!(
            inbound.ingest(&copy, t.channel_id, false).await,
            Ingested::Dropped
        );
        assert!(
            t.bob.messages().pending_group_decrypts.lock().is_empty(),
            "not parked for an SKDM"
        );
    }
}
