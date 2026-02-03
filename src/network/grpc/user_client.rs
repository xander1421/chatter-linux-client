//! User gRPC client.
//!
//! Client for user service operations including profile management,
//! presence, friends, blocking, and channel navigation.

use tokio_stream::{Stream, StreamExt};
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Channel;

use crate::error::NetworkError;
use crate::network::grpc::interceptors::AuthInterceptor;
use crate::proto::chat_v1::user_service_client::UserServiceClient;

/// User service client wrapper.
///
/// Wraps the generated UserServiceClient with ergonomic APIs and error mapping.
///
/// # Example
///
/// ```ignore
/// let pool = GrpcClientPool::new("http://localhost:50051")?;
/// pool.connect().await?;
///
/// let mut user = UserClient::new(pool.channel(), pool.auth_clone());
/// let profile = user.get_user_profile("user-123").await?;
/// ```
pub struct UserClient {
    inner: UserServiceClient<InterceptedService<Channel, AuthInterceptor>>,
}

impl UserClient {
    /// Create a new UserClient from pool components.
    pub fn new(channel: Channel, interceptor: AuthInterceptor) -> Self {
        Self {
            inner: UserServiceClient::with_interceptor(channel, interceptor),
        }
    }
}
