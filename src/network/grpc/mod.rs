//! gRPC client layer.
//!
//! gRPC clients for all backend services.

pub mod auth_client;
pub mod call_client;
pub mod channel_client;
pub mod client_pool;
pub mod file_client;
pub mod integration_client;
pub mod interceptors;
pub mod mailbox_client;
pub mod message_client;
pub mod notification_client;
pub mod user_client;

pub use auth_client::AuthClient;
pub use channel_client::ChannelClient;
pub use client_pool::GrpcClientPool;
pub use file_client::FileClient;
pub use interceptors::{AuthInterceptor, AuthMode};
pub use message_client::MessageClient;
pub use notification_client::NotificationClient;
pub use user_client::UserClient;
