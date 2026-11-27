use super::*;

impl MessageService {
    /// The devices of `peer_id` a bootstrap MESSAGE or REQUEST goes to,
    /// newest first.
    ///
    /// The Signal session store is our cache of a peer's devices: every
    /// normal-mode DM send leaves a session per current device (the server
    /// names one we are missing). Outside Shield, a cold peer's bundles are
    /// fetched with a pre-fetched anonymous credential. The bootstrap send
    /// checks them against the peer's registered devices. In Shield, a cold
    /// peer is `NotReachablePrivately`. After a local device rebind, a known
    /// peer's archived sessions can be rebuilt with a blind credential over Tor.
    pub(in crate::services::message) async fn bootstrap_devices(
        &self,
        peer_id: &UserId,
    ) -> Result<Vec<u32>, MessageError> {
        let peer_id_str = peer_id.to_string();
        let shield = self.shield_mode.load(std::sync::atomic::Ordering::Acquire);
        let mut devices = self.live_session_devices(peer_id).await?;
        if devices.is_empty()
            && (!shield
                || self
                    .signal_service
                    .has_session_history(&peer_id_str)
                    .map_err(|error| {
                        MessageError::from_signal_send(error, "Session device lookup")
                    })?)
        {
            let signal = if shield {
                self.signal_service.for_anonymous_route()
            } else {
                self.signal_service.as_ref().clone()
            };
            let discovery = signal
                .ensure_anonymous_device_sessions(
                    &mut self.anonymous_prekey_source(shield),
                    &peer_id_str,
                )
                .await;
            devices = self.live_session_devices(peer_id).await?;
            if devices.is_empty() {
                discovery.map_err(|error| {
                    MessageError::from_signal_send(error, "Bootstrap device discovery")
                })?;
            }
        }
        if devices.is_empty() {
            return Err(if shield {
                MessageError::NotReachablePrivately(*peer_id)
            } else {
                MessageError::DmError(format!(
                    "no reachable device of {peer_id} for the DM bootstrap"
                ))
            });
        }
        Ok(bootstrap_device_order(devices))
    }

    /// Devices of `peer_id` we hold a session with, less any the server has
    /// refused as retired (`is_retired_device`). No RPC.
    pub(in crate::services::message) async fn live_session_devices(
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
    /// (`AllDevices`) and names one linked since. Shield sends to cached
    /// devices (`NamedDevices`).
    pub(in crate::services::message) fn bootstrap_delivery(&self) -> SealedDelivery {
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
