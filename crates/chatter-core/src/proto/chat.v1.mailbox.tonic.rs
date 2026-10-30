// @generated
/// Generated client implementations.
pub mod ephemeral_mailbox_service_client {
    #![allow(
        unused_variables,
        dead_code,
        missing_docs,
        clippy::wildcard_imports,
        clippy::let_unit_value,
    )]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    #[derive(Debug, Clone)]
    pub struct EphemeralMailboxServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl EphemeralMailboxServiceClient<tonic::transport::Channel> {
        /// Attempt to create a new client by connecting to a given endpoint.
        pub async fn connect<D>(dst: D) -> Result<Self, tonic::transport::Error>
        where
            D: TryInto<tonic::transport::Endpoint>,
            D::Error: Into<StdError>,
        {
            let conn = tonic::transport::Endpoint::new(dst)?.connect().await?;
            Ok(Self::new(conn))
        }
    }
    impl<T> EphemeralMailboxServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::Body>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + std::marker::Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + std::marker::Send,
    {
        pub fn new(inner: T) -> Self {
            let inner = tonic::client::Grpc::new(inner);
            Self { inner }
        }
        pub fn with_origin(inner: T, origin: Uri) -> Self {
            let inner = tonic::client::Grpc::with_origin(inner, origin);
            Self { inner }
        }
        pub fn with_interceptor<F>(
            inner: T,
            interceptor: F,
        ) -> EphemeralMailboxServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::Body>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::Body>,
            >>::Error: Into<StdError> + std::marker::Send + std::marker::Sync,
        {
            EphemeralMailboxServiceClient::new(
                InterceptedService::new(inner, interceptor),
            )
        }
        /// Compress requests with the given encoding.
        ///
        /// This requires the server to support it otherwise it might respond with an
        /// error.
        #[must_use]
        pub fn send_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.send_compressed(encoding);
            self
        }
        /// Enable decompressing responses.
        #[must_use]
        pub fn accept_compressed(mut self, encoding: CompressionEncoding) -> Self {
            self.inner = self.inner.accept_compressed(encoding);
            self
        }
        /// Limits the maximum size of a decoded message.
        ///
        /// Default: `4MB`
        #[must_use]
        pub fn max_decoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_decoding_message_size(limit);
            self
        }
        /// Limits the maximum size of an encoded message.
        ///
        /// Default: `usize::MAX`
        #[must_use]
        pub fn max_encoding_message_size(mut self, limit: usize) -> Self {
            self.inner = self.inner.max_encoding_message_size(limit);
            self
        }
        pub async fn get_mailbox_tokens(
            &mut self,
            request: impl tonic::IntoRequest<super::GetMailboxTokensRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMailboxTokensResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.mailbox.EphemeralMailboxService/GetMailboxTokens",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.mailbox.EphemeralMailboxService",
                        "GetMailboxTokens",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn create_ephemeral_mailbox(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateEphemeralMailboxRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateEphemeralMailboxResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.mailbox.EphemeralMailboxService/CreateEphemeralMailbox",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.mailbox.EphemeralMailboxService",
                        "CreateEphemeralMailbox",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_mailbox_challenge(
            &mut self,
            request: impl tonic::IntoRequest<super::GetMailboxChallengeRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMailboxChallengeResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.mailbox.EphemeralMailboxService/GetMailboxChallenge",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.mailbox.EphemeralMailboxService",
                        "GetMailboxChallenge",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn poll_ephemeral_mailbox(
            &mut self,
            request: impl tonic::IntoRequest<super::PollEphemeralMailboxRequest>,
        ) -> std::result::Result<
            tonic::Response<super::PollEphemeralMailboxResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.mailbox.EphemeralMailboxService/PollEphemeralMailbox",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.mailbox.EphemeralMailboxService",
                        "PollEphemeralMailbox",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn send_to_ephemeral_mailbox(
            &mut self,
            request: impl tonic::IntoRequest<super::SendToEphemeralMailboxRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SendToEphemeralMailboxResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.mailbox.EphemeralMailboxService/SendToEphemeralMailbox",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.mailbox.EphemeralMailboxService",
                        "SendToEphemeralMailbox",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn acknowledge_messages(
            &mut self,
            request: impl tonic::IntoRequest<super::AcknowledgeMessagesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::AcknowledgeMessagesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.mailbox.EphemeralMailboxService/AcknowledgeMessages",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.mailbox.EphemeralMailboxService",
                        "AcknowledgeMessages",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn delete_ephemeral_mailbox(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteEphemeralMailboxRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DeleteEphemeralMailboxResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.mailbox.EphemeralMailboxService/DeleteEphemeralMailbox",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.mailbox.EphemeralMailboxService",
                        "DeleteEphemeralMailbox",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_blind_signature_public_key(
            &mut self,
            request: impl tonic::IntoRequest<super::GetBlindSignaturePublicKeyRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetBlindSignaturePublicKeyResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::unknown(
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic_prost::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.mailbox.EphemeralMailboxService/GetBlindSignaturePublicKey",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.mailbox.EphemeralMailboxService",
                        "GetBlindSignaturePublicKey",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
