use super::*;

const PRIVATE_CHANNEL_ROUTE_LIFETIME: Duration = Duration::from_secs(5 * 60);

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
    /// Unidentified traffic uses its own connection on both transports.
    /// Onion also isolates the Tor circuit. Direct remains visible to the
    /// server by IP.
    ///
    /// Secret chats never use this: see [`Self::secret_channel`].
    pub fn unidentified_channel(&self) -> GrpcTransport {
        self.isolated_unidentified_channel(
            &self.unidentified_channel,
            #[cfg(feature = "onion")]
            &self.unidentified_onion_tor,
        )
    }

    fn private_channel_route(&self, channel: crate::domain::ChannelId) -> Arc<PrivateChannelRoute> {
        let mut routes = self.private_channels.lock();
        routes.retain(|_, route| route.created.elapsed() < PRIVATE_CHANNEL_ROUTE_LIFETIME);
        // Bound retained sockets without ever reusing one for a different channel.
        if routes.len() >= 64 && !routes.contains_key(&channel) {
            routes.clear();
        }
        routes.entry(channel).or_default().clone()
    }

    pub(super) fn private_post_channel(
        self: &Arc<Self>,
        channel: crate::domain::ChannelId,
    ) -> GrpcTransport {
        let pool = Arc::clone(self);
        let generation = self.transport_token.read().clone();
        GrpcTransport::private_channel(move || pool.private_channel_connection(channel), generation)
    }

    fn private_channel_connection(&self, channel: crate::domain::ChannelId) -> GrpcTransport {
        let route = self.private_channel_route(channel);
        self.isolated_unidentified_channel(
            &route.grpc,
            #[cfg(feature = "onion")]
            &route.tor,
        )
    }

    pub(super) fn one_shot_anonymous_channel(&self) -> GrpcTransport {
        self.isolated_unidentified_channel(
            &RwLock::new(None),
            #[cfg(feature = "onion")]
            &super::super::onion_transport::SharedTorClient::default(),
        )
    }

    pub(crate) fn private_channel_storage_client(
        &self,
        channel: crate::domain::ChannelId,
    ) -> Result<reqwest::Client, String> {
        self.private_channel_route(channel)
            .cdn
            .get_or_init(crate::network::storage_http::build_client)
            .clone()
    }

    #[cfg(feature = "onion")]
    pub(crate) async fn private_channel_storage_tor(
        &self,
        channel: crate::domain::ChannelId,
    ) -> Result<super::super::onion_transport::SharedTorClient, NetworkError> {
        let route = self.private_channel_route(channel);
        let base = super::super::onion_transport::shared_bootstrap(&self.onion_tor)
            .await
            .map_err(|e| NetworkError::ConnectionFailed(e.to_string()))?;
        route
            .tor
            .get_or_init(|| async { base.isolated_client() })
            .await;
        Ok(route.tor.clone())
    }

    /// Loopback stand-ins for the three Shield circuit streams in wire tests.
    #[cfg(all(test, feature = "onion", feature = "dev-trust-root"))]
    pub(crate) fn install_loopback_shield_routes(
        &self,
        url: &str,
    ) {
        self.set_onion_address(Some(format!("http://{}.onion:443", "a".repeat(56))));
        self.set_transport_mode(TransportMode::Onion).unwrap();
        let endpoint = tonic::transport::Endpoint::from_shared(url.to_owned()).unwrap();
        let connect = || {
            GrpcTransport::from_onion(endpoint.connect_lazy(), self.transport_mode.clone())
                .in_generation(self.transport_token.read().clone())
        };
        *self.channel.write() = Some(connect());
        *self.unidentified_channel.write() = Some(connect());
        *self.loopback_shield_endpoint.write() = Some(endpoint);
    }

    fn isolated_unidentified_channel(
        &self,
        cached: &RwLock<Option<GrpcTransport>>,
        #[cfg(feature = "onion")] circuit: &super::super::onion_transport::SharedTorClient,
    ) -> GrpcTransport {
        // Hold the same lock swap_endpoint uses while we select and cache a
        // route, so a transport change cannot install a direct channel in Shield.
        let _route = self.channel.read();
        let mode = self.transport_mode();
        #[cfg(feature = "quic")]
        let use_quic = mode == TransportMode::Direct
            && *self.quic_probe.read() == QuicProbeState::Working;
        #[cfg(not(feature = "quic"))]
        let use_quic = false;

        if let Some(channel) = cached.read().clone()
            .filter(|channel| channel.is_current() && channel.is_http3() == use_quic)
        {
            return channel;
        }
        let mut cached = cached.write();
        if let Some(channel) = cached.clone().filter(|channel| channel.is_current() && channel.is_http3() == use_quic) {
            return channel;
        }

        let endpoint = self.endpoint.read().clone();
        let generation = self.transport_token.read().clone();
        #[cfg(all(test, feature = "onion", feature = "dev-trust-root"))]
        if mode == TransportMode::Onion
            && let Some(endpoint) = self.loopback_shield_endpoint.read().as_ref()
        {
            let channel =
                GrpcTransport::from_onion(endpoint.connect_lazy(), self.transport_mode.clone())
                    .in_generation(generation);
            *cached = Some(channel.clone());
            return channel;
        }

        #[cfg(feature = "onion")]
        if mode == TransportMode::Onion {
            let connector = super::super::onion_transport::OnionConnector::new_isolated(
                self.onion_tor.clone(), circuit.clone(),
            );
            let channel = GrpcTransport::from_onion(
                endpoint.connect_with_connector_lazy(connector),
                self.transport_mode.clone(),
            ).in_generation(generation);
            *cached = Some(channel.clone());
            return channel;
        }

        #[cfg(feature = "quic")]
        if use_quic {
            if let Some(channel) = self.direct_url.read().parse::<http::Uri>().ok()
                .and_then(|uri| super::super::transport::build_quic(uri).ok())
            {
                let channel = channel.for_pool(self.transport_mode.clone(), mode).in_generation(generation);
                *cached = Some(channel.clone());
                return channel;
            }
        }

        let channel = GrpcTransport::from(endpoint.connect_with_connector_lazy(
            crate::network::pq_tls::grpc_connector(&endpoint),
        )).for_pool(self.transport_mode.clone(), mode).in_generation(generation);
        *cached = Some(channel.clone());
        channel
    }

    /// Establish and health-check Shield's anonymous deposit route in advance.
    #[cfg(feature = "onion")]
    pub async fn preconnect_unidentified(&self) -> Result<(), NetworkError> {
        let (endpoint, mode, generation, token) = {
            let _channel = self.channel.read();
            (
                self.endpoint.read().clone(),
                self.transport_mode(),
                self.transport_gen
                    .load(std::sync::atomic::Ordering::Acquire),
                self.transport_token.read().clone(),
            )
        };
        if mode != TransportMode::Onion {
            return Ok(());
        }
        let channel = endpoint
            .connect_with_connector(self.unidentified_onion_connector())
            .await
            .map_err(|e| {
                NetworkError::ConnectionFailed(format!("anonymous onion preconnect: {e}"))
            })?;
        let channel =
            GrpcTransport::from_onion(channel, self.transport_mode.clone()).in_generation(token);
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
