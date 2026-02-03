//! Authentication gRPC client.
//!
//! Client for authentication service operations including login, registration,
//! session management, and device management.

use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::auth_service_client::AuthServiceClient;

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
}
