use super::*;

impl MessageService {
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
                    // A changed identity waits for the user's decision;
                    // retrying would only spend the peer's one-time prekeys.
                    Err(
                        ref e @ crate::error::ServiceError::Crypto(
                            crate::error::CryptoError::UntrustedIdentity { .. },
                        ),
                    ) => {
                        tracing::warn!(peer = %peer_str, device_id, error = %e,
                            "A peer's new device has a changed identity; it heals via its SenderKeyRequest")
                    }
                    Err(e) => {
                        tracing::warn!(peer = %peer_str, device_id, error = %e,
                            "Could not open a session with a peer's new device; the next group send retries");
                        self.unreached_peer_devices
                            .lock()
                            .insert((peer_str, device_id));
                    }
                }
            }
            "revoked" => {
                self.unreached_peer_devices
                    .lock()
                    .remove(&(peer_str.clone(), device_id));
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
}
