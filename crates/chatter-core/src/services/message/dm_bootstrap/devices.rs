use super::*;

impl MessageService {
    /// The devices of `peer_id` a bootstrap MESSAGE or REQUEST goes to,
    /// newest first.
    ///
    /// The Signal session store is our cache of a peer's devices: every
    /// normal-mode DM send leaves a session per current device (the server
    /// names one we are missing). Normal mode fetches bundles here only when
    /// it holds no session at all, and its bootstrap send is checked against
    /// the peer's registered devices ([`Self::bootstrap_delivery`]); Shield
    /// must not name the peer in an identified RPC, so it uses only what is
    /// cached — nothing cached is `NotReachablePrivately`, never a fetch and
    /// never a device-1 guess.
    pub(super) async fn bootstrap_devices(
        &self,
        auth_client: &mut AuthClient,
        peer_id: &UserId,
    ) -> Result<Vec<u32>, MessageError> {
        let peer_id_str = peer_id.to_string();
        let shield = self.shield_mode.load(std::sync::atomic::Ordering::Acquire);
        if !shield {
            if let Err(e) = self
                .signal_service
                .ensure_sessions_all_devices(auth_client, &peer_id_str)
                .await
            {
                tracing::warn!(peer_id = %peer_id, error = %e, "Bootstrap: device refresh failed; using cached sessions");
            }
        }
        let devices = bootstrap_device_order(self.live_session_devices(peer_id).await?);
        if devices.is_empty() {
            return Err(if shield {
                MessageError::NotReachablePrivately(*peer_id)
            } else {
                MessageError::DmError(format!(
                    "no reachable device of {peer_id} for the DM bootstrap"
                ))
            });
        }
        Ok(devices)
    }

    /// Devices of `peer_id` we hold a session with, less any the server has
    /// refused as retired (`is_retired_device`). No RPC.
    pub(super) async fn live_session_devices(
        &self,
        peer_id: &UserId,
    ) -> Result<Vec<u32>, MessageError> {
        let mut live = Vec::new();
        for device_id in self
            .signal_service
            .session_devices(&peer_id.to_string())
            .await
            .map_err(|e| MessageError::from_signal_send(e, "Session device lookup"))?
        {
            if !self.is_retired_device(*peer_id, device_id, 0).await {
                live.push(device_id);
            }
        }
        Ok(live)
    }

    /// How a bootstrap to the devices [`Self::bootstrap_devices`] returned is
    /// sent: in normal mode those are every device we hold a live session
    /// with, so the server checks them against the peer's registered devices
    /// (`AllDevices`) and names one linked since; Shield must not refresh
    /// sessions with identified prekey fetches, so it sends to the devices it
    /// has (`NamedDevices`).
    pub(super) fn bootstrap_delivery(&self) -> SealedDelivery {
        if self.shield_mode.load(std::sync::atomic::Ordering::Acquire) {
            SealedDelivery::NamedDevices
        } else {
            SealedDelivery::AllDevices
        }
    }
}

/// Newest device first — the server assigns device ids as `MAX + 1`, so the
/// lowest session is the likeliest retired install.
pub(super) fn bootstrap_device_order(mut devices: Vec<u32>) -> Vec<u32> {
    devices.sort_unstable_by(|a, b| b.cmp(a));
    devices.dedup();
    devices
}
