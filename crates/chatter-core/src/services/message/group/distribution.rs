use super::*;

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
                .deposit_sealed_to_all_devices(
                    auth_client,
                    message_client,
                    member_id,
                    &plaintext,
                    Some(channel_id.as_bytes()),
                )
                .await;
            success_count += outcome.delivered;
            self.record_sender_key_holders(channel_id, member_id, &outcome);
            if outcome.delivered == 0 && !is_self {
                error_count += 1;
                if let Some(error) = outcome.error {
                    super::super::server_dm::record_deposit_error(&mut failure, error);
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
    pub(super) fn skdm_plaintext(
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
    pub(super) fn skdm_profile_key(
        &self,
        verified: &VerifiedChannel,
        recipient: &str,
    ) -> Option<Vec<u8>> {
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

    /// A sender-key ciphertext is readable by the entire verified group.
    /// Restate our key only if every peer is allowed to receive it; otherwise
    /// the pairwise SKDM remains the selective profile-sharing path.
    pub(crate) fn group_profile_key(&self, verified: &VerifiedChannel) -> Option<Vec<u8>> {
        let us = self.settings_repo.current_user_id().ok()?;
        let mut key = None;
        for member in verified.members() {
            let peer: UserId = member.parse().ok()?;
            if peer == us {
                continue;
            }
            key = Some(crate::services::profile_visibility::shareable_profile_key(
                &self.settings_repo,
                &self.user_repo,
                &peer,
            )?);
        }
        key.map(|k| k.to_vec())
    }

    /// Record exactly the devices an SKDM deposit reached (`deposits` of its
    /// [`super::super::server_dm::DepositOutcome`]), each under the registration id
    /// of the session it was encrypted with (`registration_ids`). A device
    /// without one is not recorded, so it still counts as lacking our key.
    pub(super) fn record_sender_key_holders(
        &self,
        channel_id: &ChannelId,
        member_id: &str,
        outcome: &super::super::server_dm::DepositOutcome,
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
        {
            let mut unreached = self.unreached_peer_devices.lock();
            for &(device, _) in &devices {
                unreached.remove(&(member_id.to_string(), device));
            }
        }
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
        let sessions = self
            .signal_service
            .session_device_registrations(member)
            .await
            .unwrap_or_default();
        let mut devices: Vec<u32> = match own {
            Some(ids) => ids.into_iter().collect(),
            None => sessions.iter().map(|(device, _)| *device).collect(),
        };
        // A peer's added device we could not open a session with yet.
        for (user, device) in self.unreached_peer_devices.lock().iter() {
            if user == member && !devices.contains(device) {
                devices.push(*device);
            }
        }
        devices
            .into_iter()
            .filter(|&d| member != our_user_id || d != our_device_id)
            .map(|d| {
                (
                    d,
                    sessions
                        .iter()
                        .find(|(device, _)| *device == d)
                        .and_then(|(_, registration)| *registration),
                )
            })
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
        Ok(self
            .members_lacking_sender_key_devices(channel_id, our_user_id, our_device_id)
            .await?
            .into_iter()
            .map(|(user, _)| user)
            .collect())
    }

    async fn members_lacking_sender_key_devices(
        &self,
        channel_id: &ChannelId,
        our_user_id: &str,
        our_device_id: u32,
    ) -> Result<Vec<(UserId, Vec<(u32, Option<u32>)>)>, MessageError> {
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
                lacking.push((user_id, devices));
            }
        }
        Ok(lacking)
    }

    /// Our live device ids, from the `own_devices` cache (refetched when
    /// over a minute old). `None` when the list cannot be fetched.
    pub(in crate::services::message) async fn own_device_ids(
        &self,
    ) -> Option<std::collections::HashSet<u32>> {
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
    pub(super) async fn fetch_own_device_ids(
        &self,
    ) -> Result<std::collections::HashSet<u32>, crate::error::NetworkError> {
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
    /// Failed automatic deliveries use bounded cooldown; a verified roster or
    /// registration change, key rotation, or successful delivery resets it.
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
            .members_lacking_sender_key_devices(channel_id, &our_user_id, our_device_id)
            .await?;
        if lacking.is_empty() {
            return Ok(());
        }

        let skdm = self.create_sender_key_for_channel(channel_id).await?;
        crate::services::gateway::submission::mark_submission_started()?;
        let verified = self.group_gate().for_distribution(channel_id)?;
        let generation = libsignal_protocol::SenderKeyDistributionMessage::try_from(
            skdm.distribution_message.as_slice(),
        )
        .and_then(|message| message.chain_id())
        .map_err(|e| MessageError::EncryptionFailed(e.to_string()))?;
        {
            let now = std::time::Instant::now();
            self.signal_service
                .skdm_retry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|_, (_, next, _)| *next + std::time::Duration::from_secs(900) > now);
        }
        for (member_id, mut devices) in lacking {
            devices.sort_unstable();
            let fingerprint = format!(
                "{our_user_id}:{our_device_id}:{generation}:{}:{:?}:{devices:?}",
                verified.revision(),
                verified.state_hash()
            );
            let key = (*channel_id, member_id.to_string());
            let now = std::time::Instant::now();
            let attempts = {
                let gate = self
                    .signal_service
                    .skdm_retry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !skdm_retry_due(gate.get(&key), &fingerprint, now) {
                    continue;
                }
                gate.get(&key)
                    .filter(|(old, _, _)| old == &fingerprint)
                    .map_or(0, |(_, _, attempts)| *attempts)
            };
            let result = self
                .send_skdm_to_member(channel_id, &member_id, &skdm)
                .await;
            // A partial deposit is success for availability, but the devices
            // still lacking the key must also observe cooldown.
            let current_devices = self
                .sender_key_devices(&member_id.to_string(), &our_user_id, our_device_id)
                .await;
            let holders = self
                .signal_service
                .sender_key_holders(channel_id)
                .map_err(crypto)?;
            let incomplete = lacks_sender_key(
                &holders,
                &member_id.to_string(),
                &current_devices,
                member_id.to_string() == our_user_id,
            );
            if result.is_err() || incomplete {
                let mut gate = self
                    .signal_service
                    .skdm_retry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if gate.len() >= 4096 && !gate.contains_key(&key) {
                    if let Some(oldest) = gate
                        .iter()
                        .min_by_key(|(_, (_, next, _))| *next)
                        .map(|(key, _)| key.clone())
                    {
                        gate.remove(&oldest);
                    }
                }
                let delay = skdm_retry_delay(attempts, rand::random::<u8>());
                gate.insert(
                    key,
                    (
                        fingerprint,
                        std::time::Instant::now() + delay,
                        attempts.saturating_add(1),
                    ),
                );

                tracing::warn!(
                    %channel_id, %member_id, error = ?result.err(),
                    "Could not hand our sender key to a member's new devices before sending; \
                     they cannot read this message unless it lands first"
                );
            } else {
                self.signal_service
                    .skdm_retry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key);
            }
        }
        Ok(())
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
                Some(channel_id.as_bytes()),
            )
            .await;

        if outcome.delivered == 0 {
            return Err(MessageError::EncryptionFailed(
                "SKDM delivery to new member failed on every device".into(),
            ));
        }
        self.record_sender_key_holders(channel_id, &member_id_str, &outcome);
        self.signal_service
            .skdm_retry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(*channel_id, member_id_str.clone()));

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
}

fn skdm_retry_due(
    entry: Option<&(String, std::time::Instant, u32)>,
    fingerprint: &str,
    now: std::time::Instant,
) -> bool {
    !entry.is_some_and(|(old, next, _)| old == fingerprint && now < *next)
}

/// Exponential 5s..15min delay with up to 25% jitter, capped overall.
fn skdm_retry_delay(attempts: u32, jitter: u8) -> std::time::Duration {
    let base = 5u64.saturating_mul(1u64 << attempts.min(10));
    std::time::Duration::from_millis(
        (base * 1000 + base * u64::from(jitter) * 250 / 255).min(900_000),
    )
}

#[cfg(test)]
mod retry_tests {
    use super::{skdm_retry_delay, skdm_retry_due};
    #[test]
    fn failed_distribution_waits_but_state_changes_and_success_release_it() {
        let now = std::time::Instant::now();
        let failed = (
            "generation:membership:devices:registrations".to_owned(),
            now + std::time::Duration::from_secs(5),
            1,
        );
        assert!(!skdm_retry_due(Some(&failed), &failed.0, now));
        assert!(skdm_retry_due(Some(&failed), &failed.0, failed.1));
        assert!(skdm_retry_due(
            Some(&failed),
            "changed-generation-or-roster",
            now
        ));
        assert!(skdm_retry_due(None, &failed.0, now));
    }

    #[test]
    fn failed_distribution_cooldown_is_jittered_and_bounded() {
        assert_eq!(skdm_retry_delay(0, 0).as_secs(), 5);
        assert!(skdm_retry_delay(0, 255) > skdm_retry_delay(0, 0));
        assert_eq!(skdm_retry_delay(u32::MAX, 255).as_secs(), 900);
        assert!(skdm_retry_delay(3, 0) > skdm_retry_delay(2, 255));
    }
}
