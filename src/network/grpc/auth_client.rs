//! Authentication gRPC client.
//!
//! Client for authentication service operations including login, registration,
//! session management, and device management.

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::{
    auth_event, auth_service_client::AuthServiceClient, AuthEvent, AuthResponse,
    CheckUsernameRequest, CheckUsernameResponse, LoginRequest, LogoutRequest, RegisterRequest,
    ResendVerificationEmailRequest, Session, VerifyEmailRequest,
};

/// Auth service client wrapper.
///
/// Wraps the generated AuthServiceClient with ergonomic APIs and error mapping.
/// Create from GrpcClientPool components.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut auth = AuthClient::new(pool.channel(), pool.auth_clone());
/// let response = auth.login("alice", b"password", "linux-desktop").await?;
/// ```
pub struct AuthClient {
    inner: AuthServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl AuthClient {
    /// Create a new AuthClient from pool components.
    ///
    /// # Arguments
    ///
    /// * `channel` - Cloned channel from GrpcClientPool
    /// * `interceptor` - Cloned auth interceptor for credential injection
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: AuthServiceClient::with_interceptor(channel, interceptor),
        }
    }

    /// Login with username and password.
    ///
    /// Returns session info and user profile on success.
    pub async fn login(
        &mut self,
        username: &str,
        password_hash: &[u8],
        device_info: &str,
        public_key: &[u8],
        device_auth_token: &[u8],
        unidentified_access_key: &[u8; 16],
        device_id: Option<u32>,
    ) -> Result<AuthResponse, NetworkError> {
        let request = LoginRequest {
            username: username.to_string(),
            password_hash: password_hash.to_vec(),
            public_key: public_key.to_vec(),
            device_info: device_info.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            device_auth_token: device_auth_token.to_vec(),
            unidentified_access_key: unidentified_access_key.to_vec(),
            device_id: device_id.unwrap_or(0), // 0 = new device assignment
        };

        self.inner
            .login(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Logout current session.
    ///
    /// # Arguments
    ///
    /// * `device_id` - The device ID to log out (0 for current device)
    pub async fn logout(&mut self, device_id: u32) -> Result<(), NetworkError> {
        let request = LogoutRequest { device_id };

        self.inner
            .logout(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Check if username is available.
    pub async fn check_username(
        &mut self,
        username: &str,
    ) -> Result<CheckUsernameResponse, NetworkError> {
        let request = CheckUsernameRequest {
            username: username.to_string(),
        };

        self.inner
            .check_username(request)
            .await
            .map(|r| r.into_inner())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Verify email with verification token.
    pub async fn verify_email(&mut self, verification_token: &str) -> Result<(), NetworkError> {
        let request = VerifyEmailRequest {
            verification_token: verification_token.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .verify_email(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Resend verification email.
    ///
    /// # Arguments
    ///
    /// * `email_or_username` - Email address or username to identify account
    pub async fn resend_verification_email(
        &mut self,
        email_or_username: &str,
    ) -> Result<(), NetworkError> {
        let request = ResendVerificationEmailRequest {
            email_or_username: email_or_username.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
        };

        self.inner
            .resend_verification_email(request)
            .await
            .map(|_| ())
            .map_err(|s| NetworkError::from_status(&s))
    }

    /// Register a new user, returning stream of progress events.
    ///
    /// The stream yields AuthEvent messages with status updates:
    /// - AccountCreating
    /// - AuthzRelationsInitialized
    /// - RegistrationCompleted (with session)
    /// - RegistrationFailed (with error)
    pub async fn register(
        &mut self,
        username: &str,
        password_hash: &[u8],
        display_name: &str,
        public_key: &[u8],
        device_info: &str,
        device_auth_token: &[u8],
        unidentified_access_key: &[u8; 16],
    ) -> Result<impl Stream<Item = Result<AuthEvent, NetworkError>>, NetworkError> {
        let request = RegisterRequest {
            username: username.to_string(),
            password_hash: password_hash.to_vec(),
            public_key: public_key.to_vec(),
            display_name: display_name.to_string(),
            avatar_url: String::new(),
            device_info: device_info.to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            device_auth_token: device_auth_token.to_vec(),
            unidentified_access_key: unidentified_access_key.to_vec(),
            prekey_bundle: None,
            initial_one_time_prekeys: Vec::new(),
        };

        let response = self
            .inner
            .register(request)
            .await
            .map_err(|s| NetworkError::from_status(&s))?;

        Ok(response
            .into_inner()
            .map(|r| r.map_err(|s| NetworkError::from_status(&s))))
    }

    /// Register and wait for completion.
    ///
    /// Convenience method that consumes the event stream and returns
    /// the final session on success.
    pub async fn register_and_wait(
        &mut self,
        username: &str,
        password_hash: &[u8],
        display_name: &str,
        public_key: &[u8],
        device_info: &str,
        device_auth_token: &[u8],
        unidentified_access_key: &[u8; 16],
    ) -> Result<Session, NetworkError> {
        let stream = self
            .register(
                username,
                password_hash,
                display_name,
                public_key,
                device_info,
                device_auth_token,
                unidentified_access_key,
            )
            .await?;

        tokio::pin!(stream);

        while let Some(result) = stream.next().await {
            let event = result?;
            if let Some(payload) = event.payload {
                match payload {
                    auth_event::Payload::SessionCreated(session) => {
                        return Ok(session);
                    }
                    auth_event::Payload::RegistrationError(error) => {
                        return Err(NetworkError::ServerError(error.message));
                    }
                    _ => {} // Progress events, ignore
                }
            }
        }

        Err(NetworkError::Unknown(
            "registration stream ended without result".to_string(),
        ))
    }
}
