use super::*;

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

impl MessageService {
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
}
