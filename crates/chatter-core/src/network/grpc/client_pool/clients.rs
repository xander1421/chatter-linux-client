use super::*;

impl GrpcClientPool {
    // ========================================================================
    // Client factories — eliminate repeated `Client::new(channel(), auth_clone())`
    // ========================================================================

    /// Create an authenticated AuthClient.
    pub fn auth_client(&self) -> super::super::AuthClient {
        super::super::AuthClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated MessageClient.
    pub fn message_client(&self) -> super::super::MessageClient {
        super::super::MessageClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated MailboxClient.
    pub fn mailbox_client(&self) -> super::super::MailboxClient {
        super::super::MailboxClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated FileClient.
    pub fn file_client(&self) -> super::super::FileClient {
        // In anon mode the attachment brokering must be anonymous too. The
        // FileService URL-mint methods are public (auth-exempt) and the object
        // key is an opaque random cdn_key, so the ONLY account linkage is the
        // Basic-auth header the interceptor would attach. Under onion transport
        // build the client with an EMPTY interceptor (AuthMode::None → no header)
        // over the ISOLATED anon circuit, so the server can't tie the cdn_key to
        // the account by credential or by connection. Combined with the CDN blob
        // transfer already routed over Tor (anon_http), this closes H3. Direct
        // transport keeps the authenticated client.
        #[cfg(feature = "onion")]
        if self.transport_mode() == TransportMode::Onion {
            return super::super::FileClient::new(
                self.unidentified_channel(),
                AuthInterceptor::new(),
            );
        }
        super::super::FileClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated AI broker client.
    pub fn ai_client(&self) -> super::super::ai_client::AiClient {
        super::super::ai_client::AiClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated ChannelClient.
    pub fn channel_client(&self) -> super::super::ChannelClient {
        super::super::ChannelClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated CallClient.
    pub fn call_client(&self) -> super::super::CallClient {
        super::super::CallClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated UserClient.
    pub fn user_client(&self) -> super::super::UserClient {
        super::super::UserClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated GifClient.
    pub fn gif_client(&self) -> super::super::GifClient {
        super::super::GifClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated IntegrationClient.
    pub fn integration_client(&self) -> super::super::IntegrationClient {
        super::super::IntegrationClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated SvrClient for backup operations.
    pub fn svr_client(&self) -> super::super::SvrClient {
        super::super::SvrClient::new(self.channel(), self.auth_clone())
    }

    /// Create an authenticated NotificationClient.
    pub fn notification_client(&self) -> super::super::NotificationClient {
        super::super::NotificationClient::new(self.channel(), self.auth_clone())
    }

    /// Create an anonymous mailbox client (no auth headers) on the
    /// unidentified channel. Not for secret chats: [`Self::secret_mailbox_client`].
    pub fn anonymous_mailbox_client(&self) -> super::super::AnonymousMailboxClient {
        super::super::AnonymousMailboxClient::new(self.unidentified_channel())
    }

    /// Create an anonymous message client (no auth headers) on the
    /// unidentified channel. Not for secret chats: [`Self::secret_message_client`].
    pub fn anonymous_message_client(&self) -> super::super::AnonymousMessageClient {
        super::super::AnonymousMessageClient::new(self.unidentified_channel())
    }

    /// Create an anonymous auth client (no auth headers) on the unidentified
    /// channel.
    pub fn anonymous_auth_client(&self) -> super::super::AnonymousAuthClient {
        super::super::AnonymousAuthClient::new(self.unidentified_channel())
    }

    /// Create an anonymous Key Transparency client (no auth headers).
    ///
    /// Fetches a peer's signed KT inclusion proof over the isolated anonymous
    /// channel so the anon-DM path can KT-gate session establishment without an
    /// authenticated identity (Phase 3a). `GetProof` is a public method
    /// server-side; the proof is public, auditable transparency data.
    pub fn anonymous_kt_client(&self) -> super::super::AnonymousKtClient {
        super::super::AnonymousKtClient::new(self.unidentified_channel())
    }

    // ========================================================================
    // Secret-chat clients — always over Tor, fail closed (see secret_channel)
    // ========================================================================

    /// Mailbox client (no auth headers) for secret-chat traffic.
    pub fn secret_mailbox_client(
        &self,
    ) -> Result<super::super::AnonymousMailboxClient, NetworkError> {
        Ok(super::super::AnonymousMailboxClient::new(
            self.secret_channel()?,
        ))
    }

    /// Message client (no auth headers) for secret-chat traffic.
    pub fn secret_message_client(
        &self,
    ) -> Result<super::super::AnonymousMessageClient, NetworkError> {
        Ok(super::super::AnonymousMessageClient::new(
            self.secret_channel()?,
        ))
    }

    /// Auth client (no auth headers) for secret-chat prekey fetches.
    pub fn secret_auth_client(&self) -> Result<super::super::AnonymousAuthClient, NetworkError> {
        Ok(super::super::AnonymousAuthClient::new(
            self.secret_channel()?,
        ))
    }

    /// Key Transparency client (no auth headers) for secret-chat sessions.
    pub fn secret_kt_client(&self) -> Result<super::super::AnonymousKtClient, NetworkError> {
        Ok(super::super::AnonymousKtClient::new(self.secret_channel()?))
    }

    /// AUTHENTICATED auth client over Tor, for the one identified call a
    /// secret-chat send can need: refreshing our OWN sender certificate.
    ///
    /// A fresh lazy channel to the pinned `.onion` on the account's Tor
    /// circuit-group — never the secret-chat circuit (that would put the
    /// account on the circuit that carries its anonymous traffic) and never the
    /// direct transport. Fails closed exactly like [`Self::secret_channel`].
    /// Not cached: the certificate is refreshed about once a day.
    pub fn secret_route_auth_client(&self) -> Result<super::super::AuthClient, NetworkError> {
        let onion = self
            .onion_addr
            .read()
            .clone()
            .ok_or(NetworkError::SecretChatNeedsTor(
                "no server .onion address is pinned",
            ))?;
        #[cfg(feature = "onion")]
        {
            let ch: GrpcTransport = Self::create_endpoint(&onion, self.timeout)?
                .connect_with_connector_lazy(self.onion_connector())
                .into();
            Ok(super::super::AuthClient::new(ch, self.auth_clone()))
        }
        #[cfg(not(feature = "onion"))]
        {
            let _ = onion;
            Err(NetworkError::SecretChatNeedsTor("this build has none"))
        }
    }

    /// File client for secret-chat attachments: no auth header (the URL-mint
    /// methods are auth-exempt), so the cdn_key is tied to no account.
    pub fn secret_file_client(&self) -> Result<super::super::FileClient, NetworkError> {
        Ok(super::super::FileClient::new(
            self.secret_channel()?,
            AuthInterceptor::new(),
        ))
    }

    /// Get auth info (user_id, device_id, token) from the auth interceptor.
    ///
    /// # Panics
    ///
    /// Panics if not in Basic auth mode or credentials are malformed.
    pub fn auth_info(&self) -> super::super::interceptors::AuthInfo {
        self.auth
            .get_auth_info()
            .expect("GrpcClientPool: auth_info requires Basic auth mode")
    }
}
