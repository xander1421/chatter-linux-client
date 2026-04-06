//! Local AI settings.
//!
//! Configuration for local AI backends, model preferences, and inference parameters.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Which backend to use for local AI inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendType {
    /// Ollama (default) - easiest to set up, good for most use cases.
    Ollama,
    /// vLLM - high-performance serving for production workloads.
    Vllm,
    /// MLC-LLM - compiled models for maximum efficiency.
    MlcLlm,
}

impl Default for BackendType {
    fn default() -> Self {
        Self::Ollama
    }
}

/// Local AI configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalAISettings {
    /// Whether local AI is enabled.
    pub enabled: bool,
    /// Backend type to use.
    pub backend: BackendType,
    /// Ollama API URL.
    pub ollama_url: String,
    /// vLLM API URL.
    pub vllm_url: String,
    /// Chat/completion model ID.
    pub chat_model: String,
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
            backend: BackendType::Ollama,
            ollama_url: "http://localhost:11434".to_string(),
            vllm_url: "http://localhost:8000".to_string(),
            chat_model: "llama3.2:3b".to_string(),
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
