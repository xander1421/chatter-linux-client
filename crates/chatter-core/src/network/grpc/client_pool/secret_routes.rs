use super::*;

impl GrpcClientPool {
    /// Isolated onion connector for the secret-chat channel (its own circuit).
    #[cfg(feature = "onion")]
    pub(super) fn secret_onion_connector(&self) -> super::super::onion_transport::OnionConnector {
        super::super::onion_transport::OnionConnector::new_isolated(
            self.onion_tor.clone(),
            self.secret_onion_tor.clone(),
        )
    }

    /// The channel for secret chats (anonymous DMs and anonymous groups).
    ///
    /// Secret chats are device-bound and ALL their traffic goes over Tor, so
    /// this is always a lazy channel over an isolated Tor circuit to the pinned
    /// `.onion`, whatever [`Self::transport_mode`] is. It fails closed: without
    /// the `onion` feature, or with no `.onion` pinned, it returns
    /// [`NetworkError::SecretChatNeedsTor`] and never falls back to the direct
    /// transport. Cached until [`Self::disconnect`] or a new onion address.
    pub fn secret_channel(&self) -> Result<GrpcTransport, NetworkError> {
        if let Some(ch) = self.secret_channel.read().clone() {
            return Ok(ch);
        }
        let mut guard = self.secret_channel.write();
        if let Some(ch) = guard.clone() {
            return Ok(ch);
        }
        let onion = self
            .onion_addr
            .read()
            .clone()
            .ok_or(NetworkError::SecretChatNeedsTor(
                "no server .onion address is pinned",
            ))?;
        #[cfg(feature = "onion")]
        {
            let endpoint = Self::create_endpoint(&onion, self.timeout)?;
            let ch: GrpcTransport = endpoint
                .connect_with_connector_lazy(self.secret_onion_connector())
                .into();
            *guard = Some(ch.clone());
            Ok(ch)
        }
        #[cfg(not(feature = "onion"))]
        {
            let _ = (onion, &mut guard);
            Err(NetworkError::SecretChatNeedsTor("this build has none"))
        }
    }

    /// Establish the isolated secret-chat h2 connection before its first RPC.
    /// The cached channel retains the connection and its idle keepalives.
    #[cfg(feature = "onion")]
    pub async fn preconnect_secret(&self) -> Result<(), NetworkError> {
        let generation = self
            .transport_gen
            .load(std::sync::atomic::Ordering::Acquire);
        let onion = self
            .onion_addr
            .read()
            .clone()
            .ok_or(NetworkError::SecretChatNeedsTor(
                "no server .onion address is pinned",
            ))?;
        let endpoint = Self::create_endpoint(&onion, self.timeout)?;
        let channel: GrpcTransport = endpoint
            .connect_with_connector(self.secret_onion_connector())
            .await
            .map_err(|e| NetworkError::ConnectionFailed(format!("secret onion preconnect: {e}")))?
            .into();
        // A completed h2 handshake does not prove the server can answer an
        // RPC after doze. Probe through this same isolated channel before it
        // becomes the cached secret route; the response is never persisted.
        let mut probe = super::super::AnonymousMailboxClient::new(channel.clone());
        tokio::time::timeout(
            Duration::from_secs(15),
            probe.get_blind_signature_public_key(),
        )
        .await
        .map_err(|_| NetworkError::Timeout)??;
        let mut guard = self.secret_channel.write();
        if self.onion_addr.read().as_deref() == Some(onion.as_str())
            && self
                .transport_gen
                .load(std::sync::atomic::Ordering::Acquire)
                == generation
        {
            *guard = Some(channel);
            *self.secret_health.write() = Some(Instant::now());
        }
        Ok(())
    }

    /// Invalidate h2 connections after resume or a default-network change.
    /// The generation fence prevents an in-flight preconnect from publishing a
    /// connection established on the previous network after this reset.
    #[cfg(feature = "onion")]
    pub fn invalidate_onion_connections(&self) {
        let mut channel = self.channel.write();
        self.transport_gen
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        *self.secret_channel.write() = None;
        *self.secret_health.write() = None;
        if self.transport_mode() == TransportMode::Onion {
            *channel = None;
            *self.unidentified_channel.write() = None;
            *self.unidentified_health.write() = None;
        }
    }

    #[cfg(feature = "onion")]
    pub async fn prepare_secret_deposit(&self) -> Result<(), NetworkError> {
        if self.secret_channel.read().is_some()
            && self
                .secret_health
                .read()
                .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
        {
            return Ok(());
        }
        let mut probe = super::super::AnonymousMailboxClient::new(self.secret_channel()?);
        if !matches!(
            tokio::time::timeout(
                Duration::from_secs(8),
                probe.get_blind_signature_public_key()
            )
            .await,
            Ok(Ok(_))
        ) {
            self.disconnect_secret();
            tracing::warn!("secret deposit route stalled; redialing before deposit");
            self.preconnect_secret().await?;
        } else {
            *self.secret_health.write() = Some(Instant::now());
        }
        Ok(())
    }

    /// Whether secret-chat traffic has a route at all (see
    /// [`Self::secret_channel`]). Cheap; builds nothing.
    pub fn secret_route_available(&self) -> bool {
        self.onion_available()
    }

    /// Drop the cached secret-chat channel so the next secret-chat RPC
    /// re-dials (still over Tor). For retry loops after a transport error.
    pub fn disconnect_secret(&self) {
        *self.secret_channel.write() = None;
        #[cfg(feature = "onion")]
        {
            *self.secret_health.write() = None;
        }
    }
}
