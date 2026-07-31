//! Cloud AI silo.
//!
//! Providers that send content **off the device**. Deliberately separate from
//! [`crate::local_ai`]: separate provider enum, separate settings, separate
//! dispatch. Nothing in the local silo can construct a client from here, so
//! "a DM never reaches a cloud provider" is a property of what the local path
//! *contains*, not only of a runtime check.
//!
//! The two silos share the inference vocabulary from
//! [`crate::local_ai::backend`] (`ChatMessage`, `GenerateParams`,
//! `ChatResponse`, the trait). That is an interface, not a capability — having
//! the trait in scope doesn't let local code build a cloud client, because the
//! only constructor is [`AnyCloudBackend::for_kind`], which needs
//! [`CloudSettings`] and an API key.
//!
//! ponytail: silo boundary is module + type level. If you want a hard one,
//! lift this module into its own crate that `chatter-core` depends on
//! optionally — then a build without the feature physically cannot link an
//! egress path. Worth doing if cloud AI ever gains more than one provider.

pub mod anthropic;
pub mod broker;

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::local_ai::backend::{
    AvailableModel, BackendError, ChatMessage, ChatResponse, GenerateParams, LocalAIBackend,
};

use self::anthropic::AnthropicBackend;
use self::broker::BrokerBackend;

/// How the cloud silo reaches a model.
///
/// Both options send the prompt off the device, so [`crate::ai_policy`] treats
/// them identically. They differ in who pays and who sees the prompt:
/// `DirectAnthropic` uses the user's own key and nobody else sees it;
/// `ServerBroker` uses the organization key and the server sees the prompt in
/// order to meter it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum CloudMode {
    /// Call the provider directly with the user's own API key (BYOK).
    #[default]
    DirectAnthropic,
    /// Route through our metered broker on the organization key.
    ServerBroker,
}

impl CloudMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DirectAnthropic => "direct-anthropic",
            Self::ServerBroker => "server-broker",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "direct-anthropic" => Some(Self::DirectAnthropic),
            "server-broker" => Some(Self::ServerBroker),
            _ => None,
        }
    }
}

/// A cloud (off-device) inference provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloudProviderKind {
    /// Anthropic (Claude).
    Anthropic,
}

impl CloudProviderKind {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic (Claude)",
        }
    }

    /// Stable identifier for settings persistence.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "anthropic" => Some(Self::Anthropic),
            _ => None,
        }
    }

    /// Cheapest sensible default model for this provider.
    pub fn default_model(self) -> &'static str {
        match self {
            // $1/$5 per MTok, 200K context.
            Self::Anthropic => "claude-haiku-4-5",
        }
    }
}

/// Configuration for the cloud silo.
///
/// Separate from `LocalAISettings` on purpose: the API key is a cloud-only
/// secret and has no business in the local provider's config struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudSettings {
    /// Which cloud provider is configured.
    pub provider: CloudProviderKind,
    /// Whether to use the user's own key or the metered server broker.
    pub mode: CloudMode,
    /// API key. `None` means the cloud silo is unavailable.
    ///
    /// Skipped on serialize so it never lands in a settings blob or a debug
    /// dump; it is persisted as its own settings-repo key.
    #[serde(skip_serializing, default)]
    pub api_key: Option<String>,
    /// Model id.
    pub model: String,
    /// HTTP request timeout.
    #[serde(with = "crate::local_ai::settings::humantime_serde")]
    pub request_timeout: Duration,
}

impl Default for CloudSettings {
    fn default() -> Self {
        Self {
            provider: CloudProviderKind::Anthropic,
            mode: CloudMode::default(),
            api_key: None,
            model: CloudProviderKind::Anthropic.default_model().to_string(),
            request_timeout: Duration::from_secs(120),
        }
    }
}

impl CloudSettings {
    /// Whether the cloud silo can actually run.
    ///
    /// Direct mode needs the user's API key. Broker mode needs none — the
    /// server holds the organization key — so it is available whenever the
    /// user is signed in.
    pub fn is_available(&self) -> bool {
        match self.mode {
            CloudMode::DirectAnthropic => {
                self.api_key.as_ref().is_some_and(|k| !k.trim().is_empty())
            }
            CloudMode::ServerBroker => true,
        }
    }
}

/// Dispatch over cloud backends.
pub enum AnyCloudBackend {
    Anthropic(AnthropicBackend),
    Broker(BrokerBackend),
}

impl AnyCloudBackend {
    /// Build the direct (bring-your-own-key) cloud backend.
    ///
    /// `None` when no API key is set — fail closed rather than construct a
    /// client that 401s on every call — and `None` in broker mode, which needs
    /// a live authenticated gRPC client rather than settings. Use
    /// [`Self::broker`] for that.
    pub fn for_settings(settings: &CloudSettings) -> Option<Self> {
        if settings.mode != CloudMode::DirectAnthropic {
            return None;
        }
        let key = settings.api_key.as_ref().filter(|k| !k.trim().is_empty())?;
        match settings.provider {
            CloudProviderKind::Anthropic => Some(Self::Anthropic(AnthropicBackend::new(
                key,
                settings.request_timeout,
            ))),
        }
    }

    /// Build the server-brokered backend (managed tokens).
    ///
    /// Separate constructor because it needs a live authenticated gRPC client,
    /// not settings.
    pub fn broker(client: crate::network::grpc::ai_client::AiClient) -> Self {
        Self::Broker(BrokerBackend::new(client))
    }

    pub fn kind(&self) -> CloudProviderKind {
        match self {
            Self::Anthropic(_) | Self::Broker(_) => CloudProviderKind::Anthropic,
        }
    }
}

impl LocalAIBackend for AnyCloudBackend {
    async fn health_check(&self) -> Result<bool, BackendError> {
        match self {
            Self::Anthropic(b) => b.health_check().await,
            Self::Broker(b) => b.health_check().await,
        }
    }

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        params: &GenerateParams,
    ) -> Result<ChatResponse, BackendError> {
        match self {
            Self::Anthropic(b) => b.chat(model, messages, params).await,
            Self::Broker(b) => b.chat(model, messages, params).await,
        }
    }

    async fn embed(&self, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
        match self {
            Self::Anthropic(b) => b.embed(model, texts).await,
            Self::Broker(b) => b.embed(model, texts).await,
        }
    }

    async fn list_models(&self) -> Result<Vec<AvailableModel>, BackendError> {
        match self {
            Self::Anthropic(b) => b.list_models().await,
            Self::Broker(b) => b.list_models().await,
        }
    }

    async fn pull_model(&self, name: &str) -> Result<(), BackendError> {
        match self {
            Self::Anthropic(b) => b.pull_model(name).await,
            Self::Broker(b) => b.pull_model(name).await,
        }
    }

    async fn delete_model(&self, name: &str) -> Result<(), BackendError> {
        match self {
            Self::Anthropic(b) => b.delete_model(name).await,
            Self::Broker(b) => b.delete_model(name).await,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Anthropic(b) => b.name(),
            Self::Broker(b) => b.name(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_api_key_yields_no_backend() {
        let settings = CloudSettings::default();
        assert!(!settings.is_available());
        assert!(AnyCloudBackend::for_settings(&settings).is_none());
    }

    #[test]
    fn blank_api_key_is_treated_as_absent() {
        let settings = CloudSettings { api_key: Some("   ".into()), ..Default::default() };
        assert!(!settings.is_available());
        assert!(AnyCloudBackend::for_settings(&settings).is_none());
    }

    #[test]
    fn configured_key_yields_a_backend() {
        let settings = CloudSettings { api_key: Some("sk-ant-x".into()), ..Default::default() };
        let backend = AnyCloudBackend::for_settings(&settings).expect("backend");
        assert_eq!(backend.kind(), CloudProviderKind::Anthropic);
        assert_eq!(backend.name(), "anthropic");
    }

    #[test]
    fn api_key_is_never_serialized() {
        let settings = CloudSettings { api_key: Some("sk-ant-secret".into()), ..Default::default() };
        let json = serde_json::to_string(&settings).expect("serialize");
        assert!(!json.contains("sk-ant-secret"), "{json}");
        assert!(!json.contains("api_key"), "{json}");
    }

    #[test]
    fn broker_mode_needs_no_api_key() {
        // The server holds the org key, so a signed-in user needs nothing else.
        let settings = CloudSettings { mode: CloudMode::ServerBroker, ..Default::default() };
        assert!(settings.api_key.is_none());
        assert!(settings.is_available());
    }

    #[test]
    fn direct_mode_without_a_key_is_unavailable() {
        let settings = CloudSettings { mode: CloudMode::DirectAnthropic, ..Default::default() };
        assert!(!settings.is_available());
    }

    #[test]
    fn for_settings_never_builds_a_direct_backend_in_broker_mode() {
        // Broker mode must go through AnyCloudBackend::broker(), which needs a
        // live authenticated client. Falling back to a direct backend here
        // would silently bill the user's own key instead of the org key.
        let settings = CloudSettings {
            mode: CloudMode::ServerBroker,
            api_key: Some("sk-ant-user-key".into()),
            ..Default::default()
        };
        assert!(AnyCloudBackend::for_settings(&settings).is_none());
    }

    #[test]
    fn mode_round_trips_through_settings_string() {
        for m in [CloudMode::DirectAnthropic, CloudMode::ServerBroker] {
            assert_eq!(CloudMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(CloudMode::parse("nonsense"), None);
    }

    #[test]
    fn default_mode_is_bring_your_own_key() {
        // Defaulting to the broker would route prompts through our server for
        // users who never asked for managed tokens.
        assert_eq!(CloudMode::default(), CloudMode::DirectAnthropic);
    }

    #[test]
    fn kind_round_trips_through_settings_string() {
        let k = CloudProviderKind::Anthropic;
        assert_eq!(CloudProviderKind::parse(k.as_str()), Some(k));
        assert_eq!(CloudProviderKind::parse("ollama"), None);
    }
}
