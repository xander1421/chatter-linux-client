//! Local AI settings.
//!
//! Configuration for local AI backends, model preferences, and inference parameters.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::providers::ProviderKind;

/// Local AI configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalAISettings {
    /// Whether local AI is enabled.
    pub enabled: bool,
    /// Which detected provider drives chat/embeddings.
    pub active_provider: ProviderKind,
    /// Ollama API URL (user-overridable; other providers use default ports).
    pub ollama_url: String,
    /// Chat/completion model ID.
    pub chat_model: String,
    /// Anthropic API key. `None` disables the cloud provider entirely.
    ///
    /// Loaded from the settings repo alongside the other client secrets
    /// (`auth.device_token`, `crypto.profile_key`) rather than living in a
    /// config file. Skipped on serialize so it never lands in a settings
    /// blob or a debug dump.
    #[serde(skip_serializing, default)]
    pub anthropic_api_key: Option<String>,
    /// Model ID used when the active provider is a cloud one.
    pub cloud_model: String,
    /// Embedding model ID.
    pub embedding_model: String,
    /// Embedding dimension (must match the embedding model).
    pub embedding_dimension: usize,
    /// Whether to auto-index messages for semantic search.
    pub auto_index: bool,
    /// HTTP request timeout.
    #[serde(with = "humantime_serde")]
    pub request_timeout: Duration,
    /// Maximum tokens for chat completion.
    pub max_tokens: u32,
    /// Temperature for chat completion (0.0-2.0).
    pub temperature: f32,
}

impl Default for LocalAISettings {
    fn default() -> Self {
        Self {
            enabled: true,
            active_provider: ProviderKind::Ollama,
            ollama_url: "http://localhost:11434".to_string(),
            chat_model: "qwen3:4b".to_string(),
            anthropic_api_key: None,
            // Cheapest current Claude ($1/$5 per MTok, 200K context).
            cloud_model: "claude-haiku-4-5".to_string(),
            embedding_model: "nomic-embed-text".to_string(),
            embedding_dimension: 768,
            auto_index: true,
            request_timeout: Duration::from_secs(120),
            max_tokens: 512,
            temperature: 0.7,
        }
    }
}

mod humantime_serde {
    use std::time::Duration;

    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = u64::deserialize(deserializer)?;
        Ok(Duration::from_secs(secs))
    }
}
