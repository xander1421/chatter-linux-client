use super::*;

impl GrpcClientPool {
    /// Disconnect from the server.
    ///
    /// Drops every cached channel. Safe to call multiple times.
    pub fn disconnect(&self) {
        *self.channel.write() = None;
        *self.unidentified_channel.write() = None;
        *self.secret_channel.write() = None;
        #[cfg(feature = "onion")]
        {
            *self.unidentified_health.write() = None;
            *self.secret_health.write() = None;
        }
    }

    /// Demote a cached `Working` QUIC verdict to `Failed` after a real RPC
    /// over it has actually failed.
    ///
    /// `dial()` never re-probes once QUIC is cached `Working` — see the
    /// comment there — which means nothing ever demoted it either: probe
    /// succeeds on a network where UDP works, phone roams to one that
    /// blocks it, every subsequent RPC fails, and `dial()` keeps handing
    /// out a QUIC channel forever (`build_quic()` is pure local
    /// construction, no I/O, so it never itself errors). A transport
    /// failure on the channel actually in use is proof enough to stop
    /// trying QUIC for the rest of this pool's life — deliberately
    /// `Failed`, not `Untried`: `Untried` would re-probe on every
    /// reconnect and reintroduce the handshake-storm shape 96a77c45
    /// already fixed once. Only demotes from `Working`; leaves `Untried`
    /// (never got a chance yet) and `Failed` (already terminal) alone.
    #[cfg(feature = "quic")]
    pub(super) fn note_transport_error(&self) {
        let mut probe = self.quic_probe.write();
        if *probe == QuicProbeState::Working {
            tracing::info!(
                "QUIC channel failed in use — falling back to TCP for the rest of this session"
            );
            *probe = QuicProbeState::Failed;
        }
    }
    #[cfg(not(feature = "quic"))]
    pub(super) fn note_transport_error(&self) {}

    /// Check if currently connected.
    pub fn is_connected(&self) -> bool {
        self.channel.read().is_some()
    }

    /// Get the shared channel for creating service clients.
    ///
    /// If the pool has not been connected yet (or a previous `connect()` failed),
    /// a **lazy channel** is created automatically. The lazy channel defers the
    /// real TCP handshake to the first gRPC call, so callers get a proper
    /// transport error instead of a panic when the server is unreachable.
    pub fn channel(&self) -> GrpcTransport {
        // Fast path: already connected.
        if let Some(ch) = self.channel.read().clone() {
            return ch;
        }

        // Slow path: create a lazy channel so callers never panic.
        let mut guard = self.channel.write();
        // Double-check after acquiring write lock.
        if let Some(ch) = guard.clone() {
            return ch;
        }
        let endpoint = self.endpoint.read().clone();

        // A prior dial() already proved QUIC reachable — build a lazy QUIC
        // channel (no I/O until the first RPC, same cost profile as
        // connect_lazy()) instead of re-probing synchronously, which this
        // path must never do: callers rely on channel() never blocking.
        #[cfg(feature = "quic")]
        {
            let quic_ready = *self.transport_mode.read() == TransportMode::Direct
                && *self.quic_probe.read() == QuicProbeState::Working;
            if quic_ready {
                let server_url = self.direct_url.read().clone();
                if let Ok(uri) = server_url.parse::<http::Uri>() {
                    match super::super::transport::build_quic(uri) {
                        Ok(channel) => {
                            tracing::warn!(
                                "GrpcClientPool: no active connection — created lazy QUIC channel (RPCs will attempt on-demand connect)"
                            );
                            *guard = Some(channel.clone());
                            return channel;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "lazy QUIC channel build failed — falling back to TCP");
                        }
                    }
                }
            }
        }

        #[cfg(feature = "onion")]
        let channel: GrpcTransport = if *self.transport_mode.read() == TransportMode::Onion {
            endpoint
                .connect_with_connector_lazy(self.onion_connector())
                .into()
        } else {
            endpoint.connect_lazy().into()
        };
        #[cfg(not(feature = "onion"))]
        let channel: GrpcTransport = endpoint.connect_lazy().into();
        tracing::warn!(
            "GrpcClientPool: no active connection — created lazy channel (RPCs will attempt on-demand connect)"
        );
        *guard = Some(channel.clone());
        channel
    }

    /// Try to get the channel, returning None if not connected.
    pub fn try_channel(&self) -> Option<GrpcTransport> {
        self.channel.read().clone()
    }

    /// Get the authentication interceptor for setting credentials.
    ///
    /// Use this to configure authentication before making requests:
    ///
    /// ```ignore
    /// pool.auth().set_bearer("my-token".to_string());
    /// // or
    /// pool.auth().set_basic("user.1".to_string(), "password".to_string());
    /// ```
    pub fn auth(&self) -> &AuthInterceptor {
        &self.auth
    }

    /// Get a clone of the auth interceptor for use with service clients.
    ///
    /// Service clients need their own interceptor instance.
    pub fn auth_clone(&self) -> AuthInterceptor {
        self.auth.clone()
    }

    /// Get the configured server URL.
    pub fn server_url(&self) -> String {
        self.endpoint
            .read()
            .uri()
            .host()
            .unwrap_or("unknown")
            .to_string()
    }

    /// Get the full endpoint URL string.
    ///
    /// Returns the complete URL including scheme and port.
    pub fn endpoint(&self) -> String {
        self.endpoint.read().uri().to_string()
    }

    /// The configured HTTPS origin, even while the RPC endpoint is an onion.
    /// Update HTTP travels to this hostname through Tor with the pinned CA.
    pub fn update_origin(&self) -> String {
        self.direct_url.read().clone()
    }
}
