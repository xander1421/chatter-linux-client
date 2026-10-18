use super::*;

impl GrpcClientPool {
    /// Connect to the server.
    ///
    /// Establishes the underlying connection when dialing performs I/O. Cached
    /// lazy QUIC construction succeeds only if current evidence is Reachable;
    /// otherwise RPCs may still use the cached channel to establish contact.
    ///
    /// # Errors
    ///
    /// Returns `NetworkError::ConnectionFailed` if the connection fails.
    pub async fn connect(&self) -> Result<(), NetworkError> {
        if self.connect_with_evidence().await? {
            Ok(())
        } else {
            Err(NetworkError::ConnectionFailed(
                "server reachability is not confirmed".into(),
            ))
        }
    }

    /// The return value reflects current authoritative evidence, including when
    /// newer RPC evidence supersedes this dial or QUIC construction performs no I/O.
    pub(crate) async fn connect_with_evidence(&self) -> Result<bool, NetworkError> {
        let (endpoint, mode, generation) = self.snapshot();
        let revision = crate::network::connection::server_evidence_revision();
        let connect_result = self.dial_network(&endpoint, mode).await;
        let mut guard = self.channel.write();
        if self
            .transport_gen
            .load(std::sync::atomic::Ordering::Acquire)
            != generation
        {
            return Err(NetworkError::ConnectionFailed(
                "transport changed while connecting; retry".into(),
            ));
        }
        let (channel, fresh) = match connect_result {
            Ok(result) => result,
            Err(error) => {
                if crate::network::connection::set_server_status_if_current(
                    crate::network::connection::ServerStatus::Unreachable,
                    revision,
                ) {
                    Self::note_onion_dial(mode, false);
                }
                return Err(error);
            }
        };
        if fresh
            && crate::network::connection::set_server_status_if_current(
                crate::network::connection::ServerStatus::Reachable,
                revision,
            )
        {
            Self::note_onion_dial(mode, true);
        }
        *guard = Some(channel);
        Ok(crate::network::connection::server_status()
            == crate::network::connection::ServerStatus::Reachable)
    }

    /// Capture cancellation fences before the bounded startup attempt.
    pub(crate) fn connect_fence(&self) -> (u64, u64) {
        let _guard = self.channel.read();
        (
            self.transport_gen
                .load(std::sync::atomic::Ordering::Acquire),
            crate::network::connection::server_evidence_revision(),
        )
    }

    pub(crate) fn note_connect_timeout(&self, fence: (u64, u64)) {
        let _guard = self.channel.read();
        if self
            .transport_gen
            .load(std::sync::atomic::Ordering::Acquire)
            == fence.0
        {
            crate::network::connection::set_server_status_if_current(
                crate::network::connection::ServerStatus::Unreachable,
                fence.1,
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn control_next_dial(&self, dial: ControlledDial) {
        *self.controlled_dial.lock() = Some(dial);
    }

    pub(super) async fn dial_network(
        &self,
        endpoint: &Endpoint,
        mode: TransportMode,
    ) -> Result<(GrpcTransport, bool), NetworkError> {
        #[cfg(test)]
        {
            let controlled = self.controlled_dial.lock().take();
            if let Some(dial) = controlled {
                return dial.await;
            }
        }
        self.dial(endpoint, mode)
            .await
            .map_err(|error| NetworkError::ConnectionFailed(format!("connection failed: {error}")))
    }

    /// Dial the server, preferring QUIC when reachable.
    ///
    /// Only `TransportMode::Direct` over `https://` ever attempts QUIC — onion
    /// carries TCP (Tor has no UDP relay support) and plain `http://` has no
    /// TLS 1.3 to negotiate ALPN over. Reachability is probed once per pool
    /// (cached in `quic_probe`): a hit reuses the QUIC transport on every
    /// subsequent dial, a miss (or a timeout — some networks silently drop
    /// UDP) falls back to the existing TCP/HTTP2 path for the rest of the
    /// pool's life, matching today's behavior exactly when QUIC is off or
    /// unreachable.
    pub(super) async fn dial(
        &self,
        endpoint: &Endpoint,
        mode: TransportMode,
    ) -> Result<(GrpcTransport, bool), tonic::transport::Error> {
        // QUIC target for this dial, when its probe is still in flight.
        #[cfg(feature = "quic")]
        let mut racing_quic: Option<http::Uri> = None;

        #[cfg(feature = "quic")]
        {
            let direct = mode == TransportMode::Direct;
            let probe_state = *self.quic_probe.read();
            let server_url = self.direct_url.read().clone();
            let uri = server_url
                .starts_with("https://")
                .then(|| server_url.parse::<http::Uri>().ok())
                .flatten();
            if let (true, Some(uri)) = (direct, uri) {
                match probe_state {
                    // Already proven reachable — build the channel, no probe.
                    // Re-probing on every dial() was a real bug: under
                    // reconnect churn it became a flood of UDP/TLS handshakes.
                    QuicProbeState::Working => match super::super::transport::build_quic(uri) {
                        Ok(transport) => return Ok((transport, false)),
                        Err(e) => {
                            tracing::warn!(error = %e, "cached-working QUIC channel build failed — falling back to TCP");
                        }
                    },
                    // Probe in the background while TCP dials. It used to be
                    // awaited here, so a network that drops UDP (and the adb
                    // TCP tunnel) cost QUIC_PROBE_TIMEOUT — 3s of black screen
                    // — on every cold start before anything rendered.
                    QuicProbeState::Untried => {
                        *self.quic_probe.write() = QuicProbeState::Probing;
                        let state = self.quic_probe.clone();
                        let probe_uri = uri.clone();
                        tokio::spawn(async move {
                            let outcome = tokio::time::timeout(
                                QUIC_PROBE_TIMEOUT,
                                super::super::transport::probe_quic(&probe_uri),
                            )
                            .await;
                            let verdict = match outcome {
                                Ok(Ok(())) => QuicProbeState::Working,
                                Ok(Err(e)) => {
                                    tracing::info!(error = %e, "QUIC unreachable — staying on TCP");
                                    QuicProbeState::Failed
                                }
                                Err(_) => {
                                    tracing::info!("QUIC probe timed out — staying on TCP");
                                    QuicProbeState::Failed
                                }
                            };
                            // A server-URL change resets to Untried meanwhile;
                            // this verdict is about the old URL then, so drop it.
                            let mut current = state.write();
                            if *current == QuicProbeState::Probing {
                                *current = verdict;
                            }
                        });
                        racing_quic = Some(uri);
                    }
                    QuicProbeState::Probing | QuicProbeState::Failed => {}
                }
            }
        }

        // Onion transport tunnels the channel through Tor via the Arti connector;
        // Direct transport uses tonic's built-in TCP/TLS dialer.
        #[cfg(feature = "onion")]
        let channel = if mode == TransportMode::Onion {
            endpoint
                .connect_with_connector(self.onion_connector())
                .await?
        } else {
            endpoint.connect().await?
        };
        #[cfg(not(feature = "onion"))]
        let channel = {
            let _ = mode;
            endpoint.connect().await?
        };

        // The probe won the race: prefer QUIC now rather than at the next dial.
        #[cfg(feature = "quic")]
        if let Some(uri) = racing_quic {
            if *self.quic_probe.read() == QuicProbeState::Working {
                if let Ok(transport) = super::super::transport::build_quic(uri) {
                    tracing::info!("transport: QUIC (probe succeeded)");
                    return Ok((transport, true));
                }
            }
        }

        Ok((channel.into(), true))
    }

    /// Dial the server and throw the connection away: a reachability probe.
    ///
    /// Deliberately does **not** touch the pool's channel. [`connect`] does, and
    /// a monitor that re-dialled the live channel every half minute would tear
    /// down whatever server streams were riding it. Healing the pool is already
    /// somebody else's job: [`channel`] makes a lazy one on demand and
    /// [`call_with_retry`] drops stale ones.
    ///
    /// The only thing this is for is the answer, which it records in
    /// [`crate::network::connection::server_status`].
    ///
    /// # Errors
    ///
    /// Returns `NetworkError::ConnectionFailed` if the dial fails.
    ///
    /// [`connect`]: Self::connect
    /// [`channel`]: Self::channel
    /// [`call_with_retry`]: Self::call_with_retry
    pub async fn probe(&self) -> Result<(), NetworkError> {
        use crate::network::connection::{
            ServerStatus, server_evidence_revision, set_server_status_if_current,
        };

        let (endpoint, mode, generation) = self.snapshot();
        let revision = server_evidence_revision();
        let result = self.dial_network(&endpoint, mode).await;
        // A transport swap uses this same lock, so validation and publication
        // cannot race the switch after the generation check.
        let _guard = self.channel.read();
        if self
            .transport_gen
            .load(std::sync::atomic::Ordering::Acquire)
            != generation
        {
            return Err(NetworkError::ConnectionFailed(
                "transport changed while probing; retry".into(),
            ));
        }
        match result {
            Ok((_channel, fresh)) => {
                if fresh && set_server_status_if_current(ServerStatus::Reachable, revision) {
                    Self::note_onion_dial(mode, true);
                }
                Ok(())
            }
            Err(error) => {
                if set_server_status_if_current(ServerStatus::Unreachable, revision) {
                    Self::note_onion_dial(mode, false);
                }
                Err(error)
            }
        }
    }
}
