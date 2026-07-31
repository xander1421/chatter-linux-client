//! gRPC client layer.
//!
//! gRPC clients for all backend services.

pub mod anonymous_client;
pub mod anonymous_mailbox_client;
pub mod ai_client;
pub mod auth_client;
pub mod call_client;
pub mod channel_client;
pub mod client_pool;
pub mod file_client;
pub mod retry;
pub mod gif_client;
pub mod integration_client;
pub mod interceptors;
pub mod mailbox_client;
pub mod message_client;
pub mod notification_client;
#[cfg(feature = "onion")]
pub mod onion_transport;
pub mod provisioning_client;
pub mod svr_client;
pub mod user_client;

pub use anonymous_client::{AnonymousAuthClient, AnonymousKtClient, AnonymousMessageClient};
pub use anonymous_mailbox_client::AnonymousMailboxClient;
pub use auth_client::AuthClient;
pub use call_client::CallClient;
pub use channel_client::ChannelClient;
pub use client_pool::GrpcClientPool;
pub use file_client::FileClient;
pub use gif_client::GifClient;
pub use integration_client::IntegrationClient;
pub use interceptors::{AuthInterceptor, AuthMode};
pub use mailbox_client::MailboxClient;
pub use message_client::MessageClient;
pub use notification_client::NotificationClient;
pub use provisioning_client::{ProvisioningClient, ProvisioningPollResult};
pub use retry::{retry_unary, RetryConfig};
pub use svr_client::{FetchBackupResult, SvrClient};
pub use user_client::UserClient;
