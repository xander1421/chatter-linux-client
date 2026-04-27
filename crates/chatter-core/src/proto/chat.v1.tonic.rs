// @generated
/// Generated client implementations.
pub mod auth_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    /** ---- Auth Service (Signal-style device authentication)
*/
    #[derive(Debug, Clone)]
    pub struct AuthServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl AuthServiceClient<tonic::transport::Channel> {
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
    impl<T> AuthServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> AuthServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            AuthServiceClient::new(InterceptedService::new(inner, interceptor))
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
        /** Registration Challenge (Signal-style proof of identity key possession)
 Step 1: Client requests a challenge nonce
 Step 2: Client signs nonce with identity private key
 Step 3: Client includes signature in RegisterRequest.challenge_response
*/
        pub async fn request_registration_challenge(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestRegistrationChallengeRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RequestRegistrationChallengeResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/RequestRegistrationChallenge",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.AuthService",
                        "RequestRegistrationChallenge",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Authentication
*/
        pub async fn register(
            &mut self,
            request: impl tonic::IntoRequest<super::RegisterRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::AuthEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/Register",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "Register"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn login(
            &mut self,
            request: impl tonic::IntoRequest<super::LoginRequest>,
        ) -> std::result::Result<tonic::Response<super::AuthResponse>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/Login",
            );
            let mut req = request.into_request();
            req.extensions_mut().insert(GrpcMethod::new("chat.v1.AuthService", "Login"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn logout(
            &mut self,
            request: impl tonic::IntoRequest<super::LogoutRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/Logout",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "Logout"));
            self.inner.unary(req, path, codec).await
        }
        /** Note: RefreshToken RPC removed - Signal auth uses long-lived device sessions
*/
        pub async fn verify_email(
            &mut self,
            request: impl tonic::IntoRequest<super::VerifyEmailRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/VerifyEmail",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "VerifyEmail"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn resend_verification_email(
            &mut self,
            request: impl tonic::IntoRequest<super::ResendVerificationEmailRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/ResendVerificationEmail",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.AuthService", "ResendVerificationEmail"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Password Management
*/
        pub async fn change_password(
            &mut self,
            request: impl tonic::IntoRequest<super::ChangePasswordRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/ChangePassword",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "ChangePassword"));
            self.inner.unary(req, path, codec).await
        }
        /** Account Recovery (recovery codes for password-loss scenario)
*/
        pub async fn use_recovery_code(
            &mut self,
            request: impl tonic::IntoRequest<super::UseRecoveryCodeRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UseRecoveryCodeResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/UseRecoveryCode",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "UseRecoveryCode"));
            self.inner.unary(req, path, codec).await
        }
        /** Regenerate recovery codes (requires authentication + password confirmation)
*/
        pub async fn regenerate_recovery_codes(
            &mut self,
            request: impl tonic::IntoRequest<super::RegenerateRecoveryCodesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RegenerateRecoveryCodesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/RegenerateRecoveryCodes",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.AuthService", "RegenerateRecoveryCodes"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Account Management
*/
        pub async fn delete_account(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteAccountRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::AccountDeletionEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/DeleteAccount",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "DeleteAccount"));
            self.inner.server_streaming(req, path, codec).await
        }
        /** Session Management
*/
        pub async fn list_sessions(
            &mut self,
            request: impl tonic::IntoRequest<super::GetActiveSessionsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetActiveSessionsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/ListSessions",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "ListSessions"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn revoke_session(
            &mut self,
            request: impl tonic::IntoRequest<super::InvalidateSessionRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/RevokeSession",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "RevokeSession"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn revoke_all_sessions(
            &mut self,
            request: impl tonic::IntoRequest<super::InvalidateAllSessionsRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/RevokeAllSessions",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "RevokeAllSessions"));
            self.inner.unary(req, path, codec).await
        }
        /** Device Management (multi-device Signal Protocol support)
*/
        pub async fn list_devices(
            &mut self,
            request: impl tonic::IntoRequest<super::ListDevicesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListDevicesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/ListDevices",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "ListDevices"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn rename_device(
            &mut self,
            request: impl tonic::IntoRequest<super::RenameDeviceRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/RenameDevice",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "RenameDevice"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn revoke_device(
            &mut self,
            request: impl tonic::IntoRequest<super::RevokeDeviceRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/RevokeDevice",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "RevokeDevice"));
            self.inner.unary(req, path, codec).await
        }
        /** Identity (for OIDC clients that need to discover their Chatter user_id)
*/
        pub async fn get_my_identity(
            &mut self,
            request: impl tonic::IntoRequest<super::GetMyIdentityRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMyIdentityResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/GetMyIdentity",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "GetMyIdentity"));
            self.inner.unary(req, path, codec).await
        }
        /** Username Availability
*/
        pub async fn check_username(
            &mut self,
            request: impl tonic::IntoRequest<super::CheckUsernameRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CheckUsernameResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/CheckUsername",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "CheckUsername"));
            self.inner.unary(req, path, codec).await
        }
        /** Signal Protocol Key Management
*/
        pub async fn upload_pre_key_bundle(
            &mut self,
            request: impl tonic::IntoRequest<super::UploadPreKeyBundleRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/UploadPreKeyBundle",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "UploadPreKeyBundle"));
            self.inner.unary(req, path, codec).await
        }
        /** Sealed Sender Certificate Management (metadata privacy)
 NOTE: No GetServerTrustRoot - trust root is hardcoded in clients per Signal's security model
*/
        pub async fn request_sender_certificate(
            &mut self,
            request: impl tonic::IntoRequest<super::RequestSenderCertificateRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RequestSenderCertificateResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/RequestSenderCertificate",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.AuthService", "RequestSenderCertificate"),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn get_sender_certificate(
            &mut self,
            request: impl tonic::IntoRequest<super::GetSenderCertificateRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetSenderCertificateResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/GetSenderCertificate",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "GetSenderCertificate"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn get_pre_key_bundle(
            &mut self,
            request: impl tonic::IntoRequest<super::GetPreKeyBundleRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetPreKeyBundleResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/GetPreKeyBundle",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "GetPreKeyBundle"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn refresh_pre_keys(
            &mut self,
            request: impl tonic::IntoRequest<super::RefreshPreKeysRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/RefreshPreKeys",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "RefreshPreKeys"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn get_pre_key_stats(
            &mut self,
            request: impl tonic::IntoRequest<super::GetPreKeyStatsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetPreKeyStatsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/GetPreKeyStats",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "GetPreKeyStats"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn reset_session(
            &mut self,
            request: impl tonic::IntoRequest<super::ResetSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ResetSessionResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.AuthService/ResetSession",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.AuthService", "ResetSession"));
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod internal_auth_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    ///
    #[derive(Debug, Clone)]
    pub struct InternalAuthServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl InternalAuthServiceClient<tonic::transport::Channel> {
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
    impl<T> InternalAuthServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> InternalAuthServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            InternalAuthServiceClient::new(InterceptedService::new(inner, interceptor))
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
        /** Compensates a failed/aborted registration saga
*/
        pub async fn compensate_registration(
            &mut self,
            request: impl tonic::IntoRequest<super::CompensateRegistrationRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.InternalAuthService/CompensateRegistration",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.InternalAuthService",
                        "CompensateRegistration",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Creates a new session for internal service-to-service communication
*/
        pub async fn internal_create_session(
            &mut self,
            request: impl tonic::IntoRequest<super::InternalCreateSessionRequest>,
        ) -> std::result::Result<
            tonic::Response<super::InternalCreateSessionResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.InternalAuthService/InternalCreateSession",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.InternalAuthService",
                        "InternalCreateSession",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod call_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    #[derive(Debug, Clone)]
    pub struct CallServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl CallServiceClient<tonic::transport::Channel> {
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
    impl<T> CallServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> CallServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            CallServiceClient::new(InterceptedService::new(inner, interceptor))
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
        pub async fn create_call(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateCallRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateCallResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.CallService/CreateCall",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.CallService", "CreateCall"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn join_call(
            &mut self,
            request: impl tonic::IntoRequest<super::JoinCallRequest>,
        ) -> std::result::Result<
            tonic::Response<super::JoinCallResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.CallService/JoinCall",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.CallService", "JoinCall"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn end_call(
            &mut self,
            request: impl tonic::IntoRequest<super::EndCallRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.CallService/EndCall",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.CallService", "EndCall"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn leave_call(
            &mut self,
            request: impl tonic::IntoRequest<super::LeaveCallRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.CallService/LeaveCall",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.CallService", "LeaveCall"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_call_info(
            &mut self,
            request: impl tonic::IntoRequest<super::GetCallInfoRequest>,
        ) -> std::result::Result<tonic::Response<super::CallInfo>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.CallService/GetCallInfo",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.CallService", "GetCallInfo"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_active_call_in_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::GetActiveCallInChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetActiveCallInChannelResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.CallService/GetActiveCallInChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.CallService", "GetActiveCallInChannel"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_channel_calls(
            &mut self,
            request: impl tonic::IntoRequest<super::ListChannelCallsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListChannelCallsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.CallService/ListChannelCalls",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.CallService", "ListChannelCalls"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn stream_call_events(
            &mut self,
            request: impl tonic::IntoRequest<super::StreamCallEventsRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::CallEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.CallService/StreamCallEvents",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.CallService", "StreamCallEvents"));
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn reserve_conference(
            &mut self,
            request: impl tonic::IntoRequest<super::ReserveConferenceRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ReserveConferenceResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.CallService/ReserveConference",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.CallService", "ReserveConference"));
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod channel_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    ///
    #[derive(Debug, Clone)]
    pub struct ChannelServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl ChannelServiceClient<tonic::transport::Channel> {
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
    impl<T> ChannelServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> ChannelServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            ChannelServiceClient::new(InterceptedService::new(inner, interceptor))
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
        /** Channel Management
*/
        pub async fn create_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChannelEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/CreateChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "CreateChannel"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn get_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::GetChannelRequest>,
        ) -> std::result::Result<tonic::Response<super::Channel>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/GetChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "GetChannel"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn update_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChannelEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/UpdateChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "UpdateChannel"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn delete_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChannelEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/DeleteChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "DeleteChannel"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn list_channels(
            &mut self,
            request: impl tonic::IntoRequest<super::ListChannelsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListChannelsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/ListChannels",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "ListChannels"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn search_channels(
            &mut self,
            request: impl tonic::IntoRequest<super::ChannelSearchRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ChannelSearchResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/SearchChannels",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "SearchChannels"));
            self.inner.unary(req, path, codec).await
        }
        /** Channel Membership
*/
        pub async fn join_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::ChannelJoinRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChannelEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/JoinChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "JoinChannel"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn leave_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::LeaveChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChannelEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/LeaveChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "LeaveChannel"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn invite_user(
            &mut self,
            request: impl tonic::IntoRequest<super::InviteUserRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChannelEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/InviteUser",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "InviteUser"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn respond_to_invite(
            &mut self,
            request: impl tonic::IntoRequest<super::InviteResponse>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChannelEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/RespondToInvite",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "RespondToInvite"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn kick_user(
            &mut self,
            request: impl tonic::IntoRequest<super::KickUserRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChannelEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/KickUser",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "KickUser"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn ban_user(
            &mut self,
            request: impl tonic::IntoRequest<super::BanUserRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/BanUser",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "BanUser"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn unban_user(
            &mut self,
            request: impl tonic::IntoRequest<super::UnbanUserRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChannelEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/UnbanUser",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "UnbanUser"));
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn get_channel_members(
            &mut self,
            request: impl tonic::IntoRequest<super::GetChannelMembersRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetChannelMembersResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/GetChannelMembers",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "GetChannelMembers"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn update_member_role(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateMemberRoleRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChannelEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/UpdateMemberRole",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "UpdateMemberRole"));
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn stream_channel_events(
            &mut self,
            request: impl tonic::IntoRequest<super::ChannelEventRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::ChannelEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/StreamChannelEvents",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.ChannelService", "StreamChannelEvents"),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn archive_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::ArchiveChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ArchiveChannelResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/ArchiveChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "ArchiveChannel"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn unarchive_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::UnarchiveChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UnarchiveChannelResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/UnarchiveChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.ChannelService", "UnarchiveChannel"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_archived_channels(
            &mut self,
            request: impl tonic::IntoRequest<super::ListArchivedChannelsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListArchivedChannelsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.ChannelService/ListArchivedChannels",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.ChannelService", "ListArchivedChannels"),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod internal_channel_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    ///
    #[derive(Debug, Clone)]
    pub struct InternalChannelServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl InternalChannelServiceClient<tonic::transport::Channel> {
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
    impl<T> InternalChannelServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> InternalChannelServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            InternalChannelServiceClient::new(
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
        /** Creates DM channel for friends (internal workflow use only)
 This is idempotent - returns existing channel if already exists
*/
        pub async fn internal_create_dm_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::InternalCreateDmChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<super::InternalCreateDmChannelResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.InternalChannelService/InternalCreateDMChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.InternalChannelService",
                        "InternalCreateDMChannel",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod file_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    /** ---- File Service
*/
    #[derive(Debug, Clone)]
    pub struct FileServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl FileServiceClient<tonic::transport::Channel> {
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
    impl<T> FileServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> FileServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            FileServiceClient::new(InterceptedService::new(inner, interceptor))
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
        /** Download: server streaming
*/
        pub async fn download_file(
            &mut self,
            request: impl tonic::IntoRequest<super::FileRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::FileChunk>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/DownloadFile",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "DownloadFile"));
            self.inner.server_streaming(req, path, codec).await
        }
        /** Upload: client streaming
*/
        pub async fn upload_file(
            &mut self,
            request: impl tonic::IntoStreamingRequest<Message = super::FileChunk>,
        ) -> std::result::Result<tonic::Response<super::FileMetadata>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/UploadFile",
            );
            let mut req = request.into_streaming_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "UploadFile"));
            self.inner.client_streaming(req, path, codec).await
        }
        /** Deletion (Compensation Action)
*/
        pub async fn delete_file(
            &mut self,
            request: impl tonic::IntoRequest<super::FileRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/DeleteFile",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "DeleteFile"));
            self.inner.unary(req, path, codec).await
        }
        /** List files in a channel
*/
        pub async fn list_channel_files(
            &mut self,
            request: impl tonic::IntoRequest<super::ListChannelFilesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListChannelFilesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/ListChannelFiles",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "ListChannelFiles"));
            self.inner.unary(req, path, codec).await
        }
        /** Initiate a resumable multipart upload
*/
        pub async fn initiate_upload(
            &mut self,
            request: impl tonic::IntoRequest<super::InitiateUploadRequest>,
        ) -> std::result::Result<tonic::Response<super::UploadSession>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/InitiateUpload",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "InitiateUpload"));
            self.inner.unary(req, path, codec).await
        }
        /** Upload a single chunk
*/
        pub async fn upload_chunk(
            &mut self,
            request: impl tonic::IntoRequest<super::UploadChunkRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UploadChunkResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/UploadChunk",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "UploadChunk"));
            self.inner.unary(req, path, codec).await
        }
        /** Complete the upload
*/
        pub async fn complete_upload(
            &mut self,
            request: impl tonic::IntoRequest<super::CompleteUploadRequest>,
        ) -> std::result::Result<tonic::Response<super::FileMetadata>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/CompleteUpload",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "CompleteUpload"));
            self.inner.unary(req, path, codec).await
        }
        /** Abort the upload
*/
        pub async fn abort_upload(
            &mut self,
            request: impl tonic::IntoRequest<super::AbortUploadRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/AbortUpload",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "AbortUpload"));
            self.inner.unary(req, path, codec).await
        }
        /** Share a file with another user
*/
        pub async fn share_file(
            &mut self,
            request: impl tonic::IntoRequest<super::ShareFileRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/ShareFile",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "ShareFile"));
            self.inner.unary(req, path, codec).await
        }
        /** Revoke file access from a user
*/
        pub async fn revoke_file_access(
            &mut self,
            request: impl tonic::IntoRequest<super::RevokeFileAccessRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/RevokeFileAccess",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "RevokeFileAccess"));
            self.inner.unary(req, path, codec).await
        }
        /** List files shared with a user
*/
        pub async fn list_shared_files(
            &mut self,
            request: impl tonic::IntoRequest<super::ListSharedFilesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListSharedFilesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/ListSharedFiles",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "ListSharedFiles"));
            self.inner.unary(req, path, codec).await
        }
        /** Get all permissions for a file
*/
        pub async fn get_file_permissions(
            &mut self,
            request: impl tonic::IntoRequest<super::GetFilePermissionsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetFilePermissionsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/GetFilePermissions",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "GetFilePermissions"));
            self.inner.unary(req, path, codec).await
        }
        /** Search files with filters
*/
        pub async fn search_files(
            &mut self,
            request: impl tonic::IntoRequest<super::SearchFilesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SearchFilesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/SearchFiles",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "SearchFiles"));
            self.inner.unary(req, path, codec).await
        }
        /** Get storage statistics for a user
*/
        pub async fn get_storage_stats(
            &mut self,
            request: impl tonic::IntoRequest<super::GetStorageStatsRequest>,
        ) -> std::result::Result<tonic::Response<super::StorageStats>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/GetStorageStats",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.FileService", "GetStorageStats"));
            self.inner.unary(req, path, codec).await
        }
        /** Get presigned upload URL (requires authentication for rate limiting)
*/
        pub async fn get_attachment_upload_url(
            &mut self,
            request: impl tonic::IntoRequest<super::GetAttachmentUploadUrlRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetAttachmentUploadUrlResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/GetAttachmentUploadUrl",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.FileService", "GetAttachmentUploadUrl"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Confirm upload completed (authenticated)
*/
        pub async fn confirm_attachment_upload(
            &mut self,
            request: impl tonic::IntoRequest<super::ConfirmAttachmentUploadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ConfirmAttachmentUploadResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/ConfirmAttachmentUpload",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.FileService", "ConfirmAttachmentUpload"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Get presigned download URL
*/
        pub async fn get_attachment_download_url(
            &mut self,
            request: impl tonic::IntoRequest<super::GetAttachmentDownloadUrlRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetAttachmentDownloadUrlResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.FileService/GetAttachmentDownloadUrl",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.FileService", "GetAttachmentDownloadUrl"),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod gif_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    #[derive(Debug, Clone)]
    pub struct GifServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl GifServiceClient<tonic::transport::Channel> {
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
    impl<T> GifServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> GifServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            GifServiceClient::new(InterceptedService::new(inner, interceptor))
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
        pub async fn search_gifs(
            &mut self,
            request: impl tonic::IntoRequest<super::SearchGifsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SearchGifsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.GifService/SearchGifs",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.GifService", "SearchGifs"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_trending_gifs(
            &mut self,
            request: impl tonic::IntoRequest<super::GetTrendingGifsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetTrendingGifsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.GifService/GetTrendingGifs",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.GifService", "GetTrendingGifs"));
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod integration_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    ///
    #[derive(Debug, Clone)]
    pub struct IntegrationServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl IntegrationServiceClient<tonic::transport::Channel> {
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
    impl<T> IntegrationServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> IntegrationServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            IntegrationServiceClient::new(InterceptedService::new(inner, interceptor))
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
        /** Integration CRUD operations
*/
        pub async fn create_integration(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateIntegrationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateIntegrationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/CreateIntegration",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.IntegrationService", "CreateIntegration"),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn get_integration(
            &mut self,
            request: impl tonic::IntoRequest<super::GetIntegrationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetIntegrationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/GetIntegration",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.IntegrationService", "GetIntegration"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn list_integrations(
            &mut self,
            request: impl tonic::IntoRequest<super::ListIntegrationsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListIntegrationsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/ListIntegrations",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.IntegrationService", "ListIntegrations"),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn update_integration(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateIntegrationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UpdateIntegrationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/UpdateIntegration",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.IntegrationService", "UpdateIntegration"),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn delete_integration(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteIntegrationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DeleteIntegrationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/DeleteIntegration",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.IntegrationService", "DeleteIntegration"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Integration management
*/
        pub async fn enable_integration(
            &mut self,
            request: impl tonic::IntoRequest<super::EnableIntegrationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::EnableIntegrationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/EnableIntegration",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.IntegrationService", "EnableIntegration"),
                );
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn disable_integration(
            &mut self,
            request: impl tonic::IntoRequest<super::DisableIntegrationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DisableIntegrationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/DisableIntegration",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.IntegrationService", "DisableIntegration"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn test_integration(
            &mut self,
            request: impl tonic::IntoRequest<super::TestIntegrationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::TestIntegrationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/TestIntegration",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.IntegrationService", "TestIntegration"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn register_webhook(
            &mut self,
            request: impl tonic::IntoRequest<super::RegisterWebhookRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RegisterWebhookResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/RegisterWebhook",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.IntegrationService", "RegisterWebhook"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn unregister_webhook(
            &mut self,
            request: impl tonic::IntoRequest<super::UnregisterWebhookRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UnregisterWebhookResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/UnregisterWebhook",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.IntegrationService", "UnregisterWebhook"),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn handle_webhook_callback(
            &mut self,
            request: impl tonic::IntoRequest<super::HandleWebhookCallbackRequest>,
        ) -> std::result::Result<
            tonic::Response<super::HandleWebhookCallbackResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/HandleWebhookCallback",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.IntegrationService",
                        "HandleWebhookCallback",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn add_integration_to_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::AddIntegrationToChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<super::AddIntegrationToChannelResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/AddIntegrationToChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.IntegrationService",
                        "AddIntegrationToChannel",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn remove_integration_from_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::RemoveIntegrationFromChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RemoveIntegrationFromChannelResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/RemoveIntegrationFromChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.IntegrationService",
                        "RemoveIntegrationFromChannel",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn get_channel_integrations(
            &mut self,
            request: impl tonic::IntoRequest<super::GetChannelIntegrationsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetChannelIntegrationsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/GetChannelIntegrations",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.IntegrationService",
                        "GetChannelIntegrations",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        pub async fn stream_integration_events(
            &mut self,
            request: impl tonic::IntoRequest<super::StreamIntegrationEventsRequest>,
        ) -> std::result::Result<
            tonic::Response<
                tonic::codec::Streaming<super::StreamIntegrationEventsResponse>,
            >,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/StreamIntegrationEvents",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.IntegrationService",
                        "StreamIntegrationEvents",
                    ),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        pub async fn get_model_limits(
            &mut self,
            request: impl tonic::IntoRequest<super::GetModelLimitsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetModelLimitsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/GetModelLimits",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.IntegrationService", "GetModelLimits"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn list_available_models(
            &mut self,
            request: impl tonic::IntoRequest<super::ListAvailableModelsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListAvailableModelsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.IntegrationService/ListAvailableModels",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.IntegrationService", "ListAvailableModels"),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod memory_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    ///
    #[derive(Debug, Clone)]
    pub struct MemoryServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl MemoryServiceClient<tonic::transport::Channel> {
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
    impl<T> MemoryServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> MemoryServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            MemoryServiceClient::new(InterceptedService::new(inner, interceptor))
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
        /** Store a memory (explicit user request or client-side extraction result).
*/
        pub async fn remember(
            &mut self,
            request: impl tonic::IntoRequest<super::RememberRequest>,
        ) -> std::result::Result<
            tonic::Response<super::RememberResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/Remember",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MemoryService", "Remember"));
            self.inner.unary(req, path, codec).await
        }
        /** Store multiple memories at once (client-side batch extraction).
*/
        pub async fn batch_remember(
            &mut self,
            request: impl tonic::IntoRequest<super::BatchRememberRequest>,
        ) -> std::result::Result<
            tonic::Response<super::BatchRememberResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/BatchRemember",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MemoryService", "BatchRemember"));
            self.inner.unary(req, path, codec).await
        }
        /** Delete a specific memory.
*/
        pub async fn forget(
            &mut self,
            request: impl tonic::IntoRequest<super::ForgetRequest>,
        ) -> std::result::Result<tonic::Response<super::ForgetResponse>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/Forget",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MemoryService", "Forget"));
            self.inner.unary(req, path, codec).await
        }
        /** Semantic search over memories with optional entity/relationship inclusion.
*/
        pub async fn search(
            &mut self,
            request: impl tonic::IntoRequest<super::SearchRequest>,
        ) -> std::result::Result<tonic::Response<super::SearchResponse>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/Search",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MemoryService", "Search"));
            self.inner.unary(req, path, codec).await
        }
        /** Build a formatted context string for LLM prompts within a token budget.
*/
        pub async fn build_context(
            &mut self,
            request: impl tonic::IntoRequest<super::BuildContextRequest>,
        ) -> std::result::Result<
            tonic::Response<super::BuildContextResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/BuildContext",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MemoryService", "BuildContext"));
            self.inner.unary(req, path, codec).await
        }
        /** Extract entities, relationships, and memories from a conversation exchange.
 SECURITY: Must NOT be called with E2E encrypted content.
 Handler MUST verify channel is not E2E encrypted before processing.
 Return PERMISSION_DENIED if channel uses E2E encryption.
*/
        pub async fn process_episode(
            &mut self,
            request: impl tonic::IntoRequest<super::ProcessEpisodeRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ProcessEpisodeResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/ProcessEpisode",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MemoryService", "ProcessEpisode"));
            self.inner.unary(req, path, codec).await
        }
        /** Get a specific entity from the knowledge graph.
*/
        pub async fn get_entity(
            &mut self,
            request: impl tonic::IntoRequest<super::GetEntityRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetEntityResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/GetEntity",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MemoryService", "GetEntity"));
            self.inner.unary(req, path, codec).await
        }
        /** Get entities related to a given entity via graph traversal.
*/
        pub async fn get_related_entities(
            &mut self,
            request: impl tonic::IntoRequest<super::GetRelatedEntitiesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetRelatedEntitiesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/GetRelatedEntities",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MemoryService", "GetRelatedEntities"));
            self.inner.unary(req, path, codec).await
        }
        /** Get the AI's aggregated understanding of the authenticated user.
*/
        pub async fn get_user_memory_profile(
            &mut self,
            request: impl tonic::IntoRequest<super::GetUserMemoryProfileRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetUserMemoryProfileResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/GetUserMemoryProfile",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.MemoryService", "GetUserMemoryProfile"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Summarize a set of conversation episodes.
 SECURITY: Same E2E restriction as ProcessEpisode — server processes plaintext.
 Handler MUST verify channel is not E2E encrypted before processing.
 Return PERMISSION_DENIED if channel uses E2E encryption.
*/
        pub async fn summarize_conversation(
            &mut self,
            request: impl tonic::IntoRequest<super::SummarizeConversationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SummarizeConversationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/SummarizeConversation",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.MemoryService", "SummarizeConversation"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Trigger time-based memory decay for the authenticated user.
*/
        pub async fn decay_memories(
            &mut self,
            request: impl tonic::IntoRequest<super::DecayMemoriesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DecayMemoriesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/DecayMemories",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MemoryService", "DecayMemories"));
            self.inner.unary(req, path, codec).await
        }
        /** Get memory statistics for the authenticated user.
*/
        pub async fn get_memory_stats(
            &mut self,
            request: impl tonic::IntoRequest<super::GetMemoryStatsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMemoryStatsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MemoryService/GetMemoryStats",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MemoryService", "GetMemoryStats"));
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod message_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    /** ---- Message Service
*/
    #[derive(Debug, Clone)]
    pub struct MessageServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl MessageServiceClient<tonic::transport::Channel> {
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
    impl<T> MessageServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> MessageServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            MessageServiceClient::new(InterceptedService::new(inner, interceptor))
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
        /** Core messaging
*/
        pub async fn send_message(
            &mut self,
            request: impl tonic::IntoRequest<super::SendMessageRequest>,
        ) -> std::result::Result<tonic::Response<super::Message>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/SendMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "SendMessage"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn edit_message(
            &mut self,
            request: impl tonic::IntoRequest<super::EditMessageRequest>,
        ) -> std::result::Result<tonic::Response<super::Message>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/EditMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "EditMessage"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn delete_message(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteMessageRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/DeleteMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "DeleteMessage"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn get_messages(
            &mut self,
            request: impl tonic::IntoRequest<super::GetMessagesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMessagesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/GetMessages",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "GetMessages"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn stream_messages(
            &mut self,
            request: impl tonic::IntoRequest<super::StreamMessagesRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::Message>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/StreamMessages",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "StreamMessages"));
            self.inner.server_streaming(req, path, codec).await
        }
        /** Sealed Sender - Public endpoint requiring Unidentified-Access-Key header
 Does NOT require authentication - validates UAK against recipient's stored UAK
*/
        pub async fn send_sealed_message(
            &mut self,
            request: impl tonic::IntoRequest<super::SendSealedMessageRequest>,
        ) -> std::result::Result<
            tonic::Response<super::SendSealedMessageResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/SendSealedMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "SendSealedMessage"));
            self.inner.unary(req, path, codec).await
        }
        /** Multi-Recipient Sealed Sender — single encrypt, server fan-out.
 Requires Combined-Unidentified-Access-Key header (XOR of all recipients' UAKs).
*/
        pub async fn send_multi_recipient_sealed_message(
            &mut self,
            request: impl tonic::IntoRequest<
                super::SendMultiRecipientSealedMessageRequest,
            >,
        ) -> std::result::Result<
            tonic::Response<super::SendMultiRecipientSealedMessageResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/SendMultiRecipientSealedMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.MessageService",
                        "SendMultiRecipientSealedMessage",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Device Message Queue (Signal-style inbox)
 Requires device authentication (Basic auth with device token)
*/
        pub async fn get_device_messages(
            &mut self,
            request: impl tonic::IntoRequest<super::GetDeviceMessagesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetDeviceMessagesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/GetDeviceMessages",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "GetDeviceMessages"));
            self.inner.unary(req, path, codec).await
        }
        ///
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
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/AcknowledgeMessages",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.MessageService", "AcknowledgeMessages"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Reactions
*/
        pub async fn add_reaction(
            &mut self,
            request: impl tonic::IntoRequest<super::ReactionRequest>,
        ) -> std::result::Result<tonic::Response<super::Message>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/AddReaction",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "AddReaction"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn remove_reaction(
            &mut self,
            request: impl tonic::IntoRequest<super::ReactionRequest>,
        ) -> std::result::Result<tonic::Response<super::Message>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/RemoveReaction",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "RemoveReaction"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn get_message_reactions(
            &mut self,
            request: impl tonic::IntoRequest<super::GetMessageReactionsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetMessageReactionsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/GetMessageReactions",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.MessageService", "GetMessageReactions"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Read Receipts & Typing Indicators
*/
        pub async fn send_read_receipt(
            &mut self,
            request: impl tonic::IntoRequest<super::SendReadReceiptRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/SendReadReceipt",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "SendReadReceipt"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn get_read_receipts(
            &mut self,
            request: impl tonic::IntoRequest<super::GetReadReceiptsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetReadReceiptsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/GetReadReceipts",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "GetReadReceipts"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn send_typing_indicator(
            &mut self,
            request: impl tonic::IntoRequest<super::SendTypingIndicatorRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/SendTypingIndicator",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.MessageService", "SendTypingIndicator"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Pin/Unpin Messages
*/
        pub async fn pin_message(
            &mut self,
            request: impl tonic::IntoRequest<super::MessageUpdate>,
        ) -> std::result::Result<tonic::Response<super::Message>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/PinMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "PinMessage"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn unpin_message(
            &mut self,
            request: impl tonic::IntoRequest<super::MessageUpdate>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/UnpinMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "UnpinMessage"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn list_pinned_messages(
            &mut self,
            request: impl tonic::IntoRequest<super::GetMessagesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListPinnedMessagesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/ListPinnedMessages",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "ListPinnedMessages"));
            self.inner.unary(req, path, codec).await
        }
        /** Search Messages
*/
        pub async fn search(
            &mut self,
            request: impl tonic::IntoRequest<super::MessageSearchRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MessageSearchResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/Search",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "Search"));
            self.inner.unary(req, path, codec).await
        }
        /** Event streaming (for real-time updates like new messages, edits, deletions, reactions)
*/
        pub async fn stream_message_events(
            &mut self,
            request: impl tonic::IntoRequest<super::StreamMessageEventsRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::MessageEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/StreamMessageEvents",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.MessageService", "StreamMessageEvents"),
                );
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn stream_global_events(
            &mut self,
            request: impl tonic::IntoRequest<super::StreamGlobalEventsRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::MessageEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.MessageService/StreamGlobalEvents",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.MessageService", "StreamGlobalEvents"));
            self.inner.server_streaming(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod internal_message_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    ///
    #[derive(Debug, Clone)]
    pub struct InternalMessageServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl InternalMessageServiceClient<tonic::transport::Channel> {
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
    impl<T> InternalMessageServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> InternalMessageServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            InternalMessageServiceClient::new(
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
        /** Internal-only comprehensive message deletion
 Only callable by Temporal workers with internal API key
*/
        pub async fn internal_delete_message(
            &mut self,
            request: impl tonic::IntoRequest<super::InternalDeleteMessageRequest>,
        ) -> std::result::Result<
            tonic::Response<super::InternalDeleteMessageResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.InternalMessageService/InternalDeleteMessage",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.InternalMessageService",
                        "InternalDeleteMessage",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod notification_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    ///
    #[derive(Debug, Clone)]
    pub struct NotificationServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl NotificationServiceClient<tonic::transport::Channel> {
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
    impl<T> NotificationServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> NotificationServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            NotificationServiceClient::new(InterceptedService::new(inner, interceptor))
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
        /** Create a new notification
*/
        pub async fn create_notification(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateNotificationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateNotificationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.NotificationService/CreateNotification",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.NotificationService", "CreateNotification"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Create multiple notifications in batch
*/
        pub async fn create_batch_notifications(
            &mut self,
            request: impl tonic::IntoRequest<super::CreateBatchNotificationsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::CreateBatchNotificationsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.NotificationService/CreateBatchNotifications",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.NotificationService",
                        "CreateBatchNotifications",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Get a specific notification
*/
        pub async fn get_notification(
            &mut self,
            request: impl tonic::IntoRequest<super::GetNotificationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetNotificationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.NotificationService/GetNotification",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.NotificationService", "GetNotification"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** List notifications for a user
*/
        pub async fn list_notifications(
            &mut self,
            request: impl tonic::IntoRequest<super::ListNotificationsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListNotificationsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.NotificationService/ListNotifications",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.NotificationService", "ListNotifications"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Mark notifications as read
*/
        pub async fn mark_as_read(
            &mut self,
            request: impl tonic::IntoRequest<super::MarkAsReadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MarkAsReadResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.NotificationService/MarkAsRead",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.NotificationService", "MarkAsRead"));
            self.inner.unary(req, path, codec).await
        }
        /** Mark all notifications as read
*/
        pub async fn mark_all_as_read(
            &mut self,
            request: impl tonic::IntoRequest<super::MarkAllAsReadRequest>,
        ) -> std::result::Result<
            tonic::Response<super::MarkAllAsReadResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.NotificationService/MarkAllAsRead",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.NotificationService", "MarkAllAsRead"));
            self.inner.unary(req, path, codec).await
        }
        /** Delete a notification
*/
        pub async fn delete_notification(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteNotificationRequest>,
        ) -> std::result::Result<
            tonic::Response<super::DeleteNotificationResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.NotificationService/DeleteNotification",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.NotificationService", "DeleteNotification"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Get unread notification count
*/
        pub async fn get_unread_count(
            &mut self,
            request: impl tonic::IntoRequest<super::GetUnreadCountRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetUnreadCountResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.NotificationService/GetUnreadCount",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.NotificationService", "GetUnreadCount"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Get notification preferences
*/
        pub async fn get_preferences(
            &mut self,
            request: impl tonic::IntoRequest<super::GetPreferencesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetPreferencesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.NotificationService/GetPreferences",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.NotificationService", "GetPreferences"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Update notification preferences
*/
        pub async fn update_preferences(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdatePreferencesRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UpdatePreferencesResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.NotificationService/UpdatePreferences",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.NotificationService", "UpdatePreferences"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Stream notifications in real-time
*/
        pub async fn stream_notifications(
            &mut self,
            request: impl tonic::IntoRequest<super::StreamNotificationsRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::StreamNotificationsResponse>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.NotificationService/StreamNotifications",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.NotificationService", "StreamNotifications"),
                );
            self.inner.server_streaming(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod svr_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    /** ---- SVR Service (authenticated — uses existing auth interceptor) ----
*/
    #[derive(Debug, Clone)]
    pub struct SvrServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl SvrServiceClient<tonic::transport::Channel> {
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
    impl<T> SvrServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> SvrServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            SvrServiceClient::new(InterceptedService::new(inner, interceptor))
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
        /** Upsert encrypted backup (create or update)
*/
        pub async fn store_backup(
            &mut self,
            request: impl tonic::IntoRequest<super::StoreBackupRequest>,
        ) -> std::result::Result<
            tonic::Response<super::StoreBackupResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.SvrService/StoreBackup",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.SvrService", "StoreBackup"));
            self.inner.unary(req, path, codec).await
        }
        /** Retrieve encrypted backup with PIN proof (rate-limited)
*/
        pub async fn fetch_backup(
            &mut self,
            request: impl tonic::IntoRequest<super::FetchBackupRequest>,
        ) -> std::result::Result<
            tonic::Response<super::FetchBackupResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.SvrService/FetchBackup",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.SvrService", "FetchBackup"));
            self.inner.unary(req, path, codec).await
        }
        /** Remove encrypted backup
*/
        pub async fn delete_backup(
            &mut self,
            request: impl tonic::IntoRequest<super::DeleteBackupRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.SvrService/DeleteBackup",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.SvrService", "DeleteBackup"));
            self.inner.unary(req, path, codec).await
        }
        /** Change PIN: re-encrypt backup with new SVR secret, update registration lock
*/
        pub async fn change_pin_backup(
            &mut self,
            request: impl tonic::IntoRequest<super::ChangePinBackupRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ChangePinBackupResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.SvrService/ChangePinBackup",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.SvrService", "ChangePinBackup"));
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod user_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    /** ---- User Service
*/
    #[derive(Debug, Clone)]
    pub struct UserServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl UserServiceClient<tonic::transport::Channel> {
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
    impl<T> UserServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> UserServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            UserServiceClient::new(InterceptedService::new(inner, interceptor))
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
        /** User Profile
*/
        pub async fn get_user_profile(
            &mut self,
            request: impl tonic::IntoRequest<super::GetUserProfileRequest>,
        ) -> std::result::Result<tonic::Response<super::UserProfile>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/GetUserProfile",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "GetUserProfile"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn update_user_profile(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdateUserProfileRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::UserEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/UpdateUserProfile",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "UpdateUserProfile"));
            self.inner.server_streaming(req, path, codec).await
        }
        /** Signal-style exact lookup by client-side HKDF(username) hash.
 Substring / display-name search is intentionally absent: display names
 are profile-key-encrypted on the server, so the old ILIKE-based
 SearchUsers could never produce meaningful matches anyway.
*/
        pub async fn lookup_username(
            &mut self,
            request: impl tonic::IntoRequest<super::LookupUsernameRequest>,
        ) -> std::result::Result<
            tonic::Response<super::LookupUsernameResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/LookupUsername",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "LookupUsername"));
            self.inner.unary(req, path, codec).await
        }
        /** User Presence
*/
        pub async fn update_presence(
            &mut self,
            request: impl tonic::IntoRequest<super::UpdatePresenceRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/UpdatePresence",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "UpdatePresence"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn stream_presence(
            &mut self,
            request: impl tonic::IntoRequest<super::StreamPresenceRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::PresenceUpdate>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/StreamPresence",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "StreamPresence"));
            self.inner.server_streaming(req, path, codec).await
        }
        /** Friendship Management
*/
        pub async fn send_friend_request(
            &mut self,
            request: impl tonic::IntoRequest<super::FriendRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::UserEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/SendFriendRequest",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "SendFriendRequest"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn respond_friend_request(
            &mut self,
            request: impl tonic::IntoRequest<super::FriendResponse>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::UserEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/RespondFriendRequest",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "RespondFriendRequest"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn remove_friend(
            &mut self,
            request: impl tonic::IntoRequest<super::RemoveFriendRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/RemoveFriend",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "RemoveFriend"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn list_friends(
            &mut self,
            request: impl tonic::IntoRequest<super::ListFriendsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListFriendsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/ListFriends",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "ListFriends"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn list_friend_requests(
            &mut self,
            request: impl tonic::IntoRequest<super::ListFriendRequestsRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListFriendRequestsResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/ListFriendRequests",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "ListFriendRequests"));
            self.inner.unary(req, path, codec).await
        }
        /** Blocking
*/
        pub async fn block_user(
            &mut self,
            request: impl tonic::IntoRequest<super::BlockUserRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::UserEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/BlockUser",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "BlockUser"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn unblock_user(
            &mut self,
            request: impl tonic::IntoRequest<super::UnblockUserRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::UserEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/UnblockUser",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "UnblockUser"));
            self.inner.server_streaming(req, path, codec).await
        }
        ///
        pub async fn list_blocked_users(
            &mut self,
            request: impl tonic::IntoRequest<super::ListBlockedUsersRequest>,
        ) -> std::result::Result<
            tonic::Response<super::ListBlockedUsersResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/ListBlockedUsers",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "ListBlockedUsers"));
            self.inner.unary(req, path, codec).await
        }
        /** Event streaming
*/
        pub async fn stream_user_events(
            &mut self,
            request: impl tonic::IntoRequest<super::UserEventRequest>,
        ) -> std::result::Result<
            tonic::Response<tonic::codec::Streaming<super::UserEvent>>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/StreamUserEvents",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "StreamUserEvents"));
            self.inner.server_streaming(req, path, codec).await
        }
        /** Channel Navigation History
*/
        pub async fn track_channel_visit(
            &mut self,
            request: impl tonic::IntoRequest<super::TrackChannelVisitRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/TrackChannelVisit",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "TrackChannelVisit"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn get_navigation_history(
            &mut self,
            request: impl tonic::IntoRequest<super::GetNavigationHistoryRequest>,
        ) -> std::result::Result<
            tonic::Response<super::GetNavigationHistoryResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/GetNavigationHistory",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "GetNavigationHistory"));
            self.inner.unary(req, path, codec).await
        }
        ///
        pub async fn clear_navigation_history(
            &mut self,
            request: impl tonic::IntoRequest<super::ClearNavigationHistoryRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/ClearNavigationHistory",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new("chat.v1.UserService", "ClearNavigationHistory"),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Channel Pinning
*/
        pub async fn pin_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::PinChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<super::PinChannelResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/PinChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "PinChannel"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn unpin_channel(
            &mut self,
            request: impl tonic::IntoRequest<super::UnpinChannelRequest>,
        ) -> std::result::Result<
            tonic::Response<super::UnpinChannelResponse>,
            tonic::Status,
        > {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/UnpinChannel",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "UnpinChannel"));
            self.inner.unary(req, path, codec).await
        }
        pub async fn reorder_pinned_channels(
            &mut self,
            request: impl tonic::IntoRequest<super::ReorderPinnedChannelsRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.UserService/ReorderPinnedChannels",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(GrpcMethod::new("chat.v1.UserService", "ReorderPinnedChannels"));
            self.inner.unary(req, path, codec).await
        }
    }
}
/// Generated client implementations.
pub mod internal_user_service_client {
    #![allow(unused_variables, dead_code, missing_docs, clippy::let_unit_value)]
    use tonic::codegen::*;
    use tonic::codegen::http::Uri;
    ///
    #[derive(Debug, Clone)]
    pub struct InternalUserServiceClient<T> {
        inner: tonic::client::Grpc<T>,
    }
    impl InternalUserServiceClient<tonic::transport::Channel> {
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
    impl<T> InternalUserServiceClient<T>
    where
        T: tonic::client::GrpcService<tonic::body::BoxBody>,
        T::Error: Into<StdError>,
        T::ResponseBody: Body<Data = Bytes> + Send + 'static,
        <T::ResponseBody as Body>::Error: Into<StdError> + Send,
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
        ) -> InternalUserServiceClient<InterceptedService<T, F>>
        where
            F: tonic::service::Interceptor,
            T::ResponseBody: Default,
            T: tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
                Response = http::Response<
                    <T as tonic::client::GrpcService<tonic::body::BoxBody>>::ResponseBody,
                >,
            >,
            <T as tonic::codegen::Service<
                http::Request<tonic::body::BoxBody>,
            >>::Error: Into<StdError> + Send + Sync,
        {
            InternalUserServiceClient::new(InterceptedService::new(inner, interceptor))
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
        /** Creates the user profile record as part of an internal workflow (e.g., Registration Saga)
*/
        pub async fn internal_create_user_profile(
            &mut self,
            request: impl tonic::IntoRequest<super::InternalCreateUserProfileRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.InternalUserService/InternalCreateUserProfile",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.InternalUserService",
                        "InternalCreateUserProfile",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
        /** Deletes the user profile record as a compensation action
*/
        pub async fn internal_delete_user_profile(
            &mut self,
            request: impl tonic::IntoRequest<super::InternalDeleteUserProfileRequest>,
        ) -> std::result::Result<tonic::Response<()>, tonic::Status> {
            self.inner
                .ready()
                .await
                .map_err(|e| {
                    tonic::Status::new(
                        tonic::Code::Unknown,
                        format!("Service was not ready: {}", e.into()),
                    )
                })?;
            let codec = tonic::codec::ProstCodec::default();
            let path = http::uri::PathAndQuery::from_static(
                "/chat.v1.InternalUserService/InternalDeleteUserProfile",
            );
            let mut req = request.into_request();
            req.extensions_mut()
                .insert(
                    GrpcMethod::new(
                        "chat.v1.InternalUserService",
                        "InternalDeleteUserProfile",
                    ),
                );
            self.inner.unary(req, path, codec).await
        }
    }
}
