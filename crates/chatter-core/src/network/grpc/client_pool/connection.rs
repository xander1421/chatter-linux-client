use super::*;

impl GrpcClientPool {
    #[cfg(feature = "quic")]
    pub(super) fn spawn_quic_probe(
        &self,
        uri: http::Uri,
        probe_generation: u64,
    ) -> tokio::task::JoinHandle<()> {
        let state = self.quic_probe.clone();
        let generation = self.quic_probe_generation.clone();
        let revision = crate::network::connection::server_evidence_revision();
        let task = tokio::spawn(async move {
            let outcome = tokio::time::timeout(
                QUIC_PROBE_TIMEOUT,
                super::super::transport::probe_quic(&uri),
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
            let mut current = state.write();
            if *current == QuicProbeState::Probing
                && generation.load(std::sync::atomic::Ordering::Acquire) == probe_generation
            {
                *current = verdict;
                if verdict == QuicProbeState::Working {
                    crate::network::connection::set_server_status_if_current(
                        crate::network::connection::ServerStatus::Reachable,
                        revision,
                    );
                }
            }
        });
        let mut active = self.quic_probe_task.lock();
        if self
            .quic_probe_generation
            .load(std::sync::atomic::Ordering::Acquire)
            != probe_generation
        {
            task.abort();
        } else if let Some(previous) = active.replace(task.abort_handle()) {
            previous.abort();
        }
        task
    }

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
        let token = self.transport_token.read().clone();
        let (endpoint, mode, generation) = self.snapshot();
        let revision = crate::network::connection::server_evidence_revision();
        let connect_result = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(NetworkError::ConnectionFailed("transport changed while connecting; retry".into())),
            result = self.dial_network(&endpoint, mode) => result,
        };
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
        if fresh && channel.is_http3() && !guard.as_ref().is_some_and(GrpcTransport::is_http3) {
            tracing::event!(target: "network_transport::quic_promoted", tracing::Level::INFO, {});
        }
        *guard = Some(channel.for_pool(self.transport_mode.clone(), mode)
            .in_generation(self.transport_token.read().clone()));
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
    /// TLS 1.3 to negotiate ALPN over. Reachability is probed once per route
    /// generation (cached in `quic_probe`): a hit reuses the QUIC transport on every
    /// subsequent dial, a miss (or a timeout — some networks silently drop
    /// UDP) falls back to the existing TCP/HTTP2 path for the rest of the
    /// route's life, matching the existing behavior when QUIC is off or
    /// unreachable.
    pub(super) async fn dial(
        &self,
        endpoint: &Endpoint,
        mode: TransportMode,
    ) -> Result<(GrpcTransport, bool), tonic::transport::Error> {
        // QUIC target for this dial, when its probe is still in flight.
        #[cfg(feature = "quic")]
        let mut racing_quic: Option<(http::Uri, u64, tokio::task::JoinHandle<()>)> = None;

        #[cfg(feature = "quic")]
        {
            let direct = mode == TransportMode::Direct;
            let (probe_state, uri, probe_generation) = {
                let mut state = self.quic_probe.write();
                let server_url = self.direct_url.read().clone();
                let uri = server_url
                    .starts_with("https://")
                    .then(|| server_url.parse::<http::Uri>().ok())
                    .flatten();
                let generation = self
                    .quic_probe_generation
                    .load(std::sync::atomic::Ordering::Acquire);
                let current = *state;
                if direct && uri.is_some() && current == QuicProbeState::Untried {
                    *state = QuicProbeState::Probing;
                }
                (current, uri, generation)
            };
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
                        let task = self.spawn_quic_probe(uri.clone(), probe_generation);
                        racing_quic = Some((uri, probe_generation, task));
                    }
                    QuicProbeState::Probing | QuicProbeState::Failed => {}
                }
            }
        }

        // Onion transport tunnels the channel through Tor via the Arti connector;
        // Direct transport uses the pinned PQ-only TCP/TLS connector.
        #[cfg(feature = "onion")]
        let channel = if mode == TransportMode::Onion {
            endpoint
                .connect_with_connector(self.onion_connector())
                .await
        } else {
            endpoint
                .connect_with_connector(crate::network::pq_tls::grpc_connector(&endpoint))
                .await
        };
        #[cfg(not(feature = "onion"))]
        let channel = {
            let _ = mode;
            endpoint
                .connect_with_connector(crate::network::pq_tls::grpc_connector(&endpoint))
                .await
        };

        // The probe won the race: prefer QUIC now rather than at the next dial.
        #[cfg(feature = "quic")]
        if let Some((uri, probe_generation, mut task)) = racing_quic {
            let grace = if channel.is_ok() {
                QUIC_PREFERENCE_GRACE
            } else {
                QUIC_PROBE_TIMEOUT
            };
            let _ = tokio::time::timeout(grace, &mut task).await;
            let probe = self.quic_probe.read();
            if *probe == QuicProbeState::Working
                && self
                    .quic_probe_generation
                    .load(std::sync::atomic::Ordering::Acquire)
                    == probe_generation
            {
                if let Ok(transport) = super::super::transport::build_quic(uri) {
                    tracing::info!("transport: QUIC (probe succeeded)");
                    return Ok((transport, true));
                }
            }
        }

        let channel = channel?;
        #[cfg(feature = "onion")]
        if mode == TransportMode::Onion {
            return Ok((
                GrpcTransport::from_onion(channel, self.transport_mode.clone()),
                true,
            ));
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
        self.probe_inner(false).await
    }

    pub(crate) async fn probe_for_confirmation(&self) -> Result<(), NetworkError> {
        self.probe_inner(true).await
    }

    async fn probe_lazy_direct(&self, endpoint: &Endpoint) -> Result<(), NetworkError> {
        let tcp = async {
            endpoint
                .connect_with_connector(crate::network::pq_tls::grpc_connector(endpoint))
                .await
                .map(|_| ())
                .map_err(|error| {
                    NetworkError::ConnectionFailed(format!("connection failed: {error}"))
                })
        };
        #[cfg(feature = "quic")]
        {
            let quic = async {
                tokio::time::timeout(
                    QUIC_PROBE_TIMEOUT,
                    super::super::transport::probe_quic(endpoint.uri()),
                )
                .await
                .map_err(|_| NetworkError::Timeout)?
                .map_err(|error| {
                    NetworkError::ConnectionFailed(format!("QUIC probe failed: {error}"))
                })
            };
            use futures::FutureExt;
            futures::future::select_ok([tcp.boxed(), quic.boxed()])
                .await
                .map(|_| ())
        }
        #[cfg(not(feature = "quic"))]
        tcp.await
    }

    async fn probe_inner(&self, require_fresh: bool) -> Result<(), NetworkError> {
        use crate::network::connection::{
            ServerStatus, server_evidence_revision, set_server_status_if_current,
        };

        let token = self.transport_token.read().clone();
        let (endpoint, mode, generation) = self.snapshot();
        if require_fresh && mode != TransportMode::Direct {
            return Ok(());
        }
        let revision = server_evidence_revision();
        let dial = async {
            match self.dial_network(&endpoint, mode).await {
                Ok((_, false)) if require_fresh => {
                    self.probe_lazy_direct(&endpoint).await.map(|()| true)
                }
                other => other.map(|(_, fresh)| fresh),
            }
        };
        let result = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(NetworkError::ConnectionFailed("transport changed while probing; retry".into())),
            result = dial => result,
        };
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
            Ok(fresh) => {
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
