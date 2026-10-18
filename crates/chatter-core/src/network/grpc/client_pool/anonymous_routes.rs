use super::*;

impl GrpcClientPool {
    /// Build a fresh onion connector sharing the pool's lazily-bootstrapped
    /// Tor client, so reconnects reuse the same warm circuit.
    #[cfg(feature = "onion")]
    pub(super) fn onion_connector(&self) -> super::super::onion_transport::OnionConnector {
        super::super::onion_transport::OnionConnector::new(self.onion_tor.clone())
    }

    /// Isolated onion connector for the unidentified channel (separate circuit).
    #[cfg(feature = "onion")]
    pub(super) fn unidentified_onion_connector(
        &self,
    ) -> super::super::onion_transport::OnionConnector {
        super::super::onion_transport::OnionConnector::new_isolated(
            self.onion_tor.clone(),
            self.unidentified_onion_tor.clone(),
        )
    }

    /// The channel for the unidentified (no auth header) clients of the
    /// ordinary sealed-sender DM path. It follows the pool's transport.
    ///
    /// Under onion transport this is a SEPARATE channel over an isolated Tor
    /// circuit, so the server cannot correlate unidentified ops with the
    /// account's authenticated RPCs by connection/circuit. On the direct
    /// transport isolation is moot (both connections share the client IP), so it
    /// falls back to the shared channel.
    ///
    /// Secret chats never use this: see [`Self::secret_channel`].
    pub fn unidentified_channel(&self) -> GrpcTransport {
        #[cfg(feature = "onion")]
        {
            if *self.transport_mode.read() == TransportMode::Onion {
                if let Some(ch) = self.unidentified_channel.read().clone() {
                    return ch;
                }
                let mut guard = self.unidentified_channel.write();
                if let Some(ch) = guard.clone() {
                    return ch;
                }
                let endpoint = self.endpoint.read().clone();
                let ch: GrpcTransport = endpoint
                    .connect_with_connector_lazy(self.unidentified_onion_connector())
                    .into();
                *guard = Some(ch.clone());
                return ch;
            }
        }
        self.channel()
    }

    /// Establish and health-check Shield's anonymous deposit route in advance.
    #[cfg(feature = "onion")]
    pub async fn preconnect_unidentified(&self) -> Result<(), NetworkError> {
        if self.transport_mode() != TransportMode::Onion {
            return Ok(());
        }
        let (endpoint, _, generation) = self.snapshot();
        let channel: GrpcTransport = endpoint
            .connect_with_connector(self.unidentified_onion_connector())
            .await
            .map_err(|e| {
                NetworkError::ConnectionFailed(format!("anonymous onion preconnect: {e}"))
            })?
            .into();
        let mut probe = super::super::AnonymousMailboxClient::new(channel.clone());
        tokio::time::timeout(
            Duration::from_secs(15),
            probe.get_blind_signature_public_key(),
        )
        .await
        .map_err(|_| NetworkError::Timeout)??;
        let mut guard = self.unidentified_channel.write();
        if self
            .transport_gen
            .load(std::sync::atomic::Ordering::Acquire)
            == generation
        {
            *guard = Some(channel);
            *self.unidentified_health.write() = Some(Instant::now());
        }
        Ok(())
    }

    /// Check the cached anonymous route before a deposit starts. If the probe
    /// stalls, move the *upcoming* deposit to a freshly dialled Tor circuit.
    #[cfg(feature = "onion")]
    pub async fn prepare_unidentified_deposit(&self) -> Result<(), NetworkError> {
        if self.transport_mode() != TransportMode::Onion {
            return Ok(());
        }
        if self.unidentified_channel.read().is_some()
            && self
                .unidentified_health
                .read()
                .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return Ok(());
        }
        let mut probe = super::super::AnonymousMailboxClient::new(self.unidentified_channel());
        if !matches!(
            tokio::time::timeout(
                Duration::from_secs(8),
                probe.get_blind_signature_public_key()
            )
            .await,
            Ok(Ok(_))
        ) {
            *self.unidentified_channel.write() = None;
            *self.unidentified_health.write() = None;
            tracing::warn!("Shield anonymous deposit route stalled; redialing before deposit");
            self.preconnect_unidentified().await?;
        } else {
            *self.unidentified_health.write() = Some(Instant::now());
        }
        if self.transport_mode() != TransportMode::Onion {
            return Err(NetworkError::ConnectionFailed(
                "onion transport changed before anonymous deposit".into(),
            ));
        }
        Ok(())
    }

    #[cfg(feature = "onion")]
    pub fn disconnect_unidentified(&self) {
        *self.unidentified_channel.write() = None;
        *self.unidentified_health.write() = None;
    }
}
