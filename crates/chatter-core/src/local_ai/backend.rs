//! Backend trait.
//!
//! Defines the common interface for local AI backends (Ollama, vLLM, MLC-LLM).
//! Each backend communicates with a local inference server via HTTP.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Error type for backend operations.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("model not found: {0}")]
    ModelNotFound(String),
    #[error("inference failed: {0}")]
    InferenceFailed(String),
    #[error("invalid response: {0}")]
    InvalidResponse(String),
    #[error("operation not supported by {0}")]
    Unsupported(&'static str),
}

/// A chat message for the completion API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

/// Chat message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

impl fmt::Display for ChatRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::System => write!(f, "system"),
            Self::User => write!(f, "user"),
            Self::Assistant => write!(f, "assistant"),
        }
    }
}

/// Parameters for text generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateParams {
    pub max_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub repeat_penalty: f32,
    /// Stop sequences.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Ask thinking models (qwen3, deepseek-r1) to skip their reasoning
    /// phase — for structured tasks (extraction) where latency matters and
    /// the scratchpad wastes the token budget.
    #[serde(default)]
    pub disable_thinking: bool,
    /// Constrain output to this JSON schema (grammar-constrained decoding).
    /// Supported by Ollama's `format` field; other providers ignore it and
    /// extraction degrades to prompt-guided JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<serde_json::Value>,
}

impl Default for GenerateParams {
    fn default() -> Self {
        Self {
            max_tokens: 512,
            temperature: 0.7,
            top_p: 0.9,
            top_k: 40,
            repeat_penalty: 1.1,
            stop: Vec::new(),
            disable_thinking: false,
            json_schema: None,
        }
    }
}

/// Chat completion response.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// Generated text.
    pub content: String,
    /// Number of prompt tokens (if available).
    pub prompt_tokens: Option<u32>,
    /// Number of completion tokens (if available).
    pub completion_tokens: Option<u32>,
    /// Model used.
    pub model: String,
}

/// Information about an available model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableModel {
    pub name: String,
    pub size: Option<u64>,
    pub quantization: Option<String>,
    pub family: Option<String>,
    pub parameter_size: Option<String>,
    pub digest: Option<String>,
}

/// Local AI backend trait.
///
/// Implementations connect to local inference servers (Ollama, vLLM, etc.)
/// and provide a unified interface for chat completion, embeddings, and
/// model management.
#[allow(async_fn_in_trait)]
pub trait LocalAIBackend: Send + Sync {
    /// Check if the backend server is reachable and healthy.
    async fn health_check(&self) -> Result<bool, BackendError>;

    /// Generate a chat completion.
    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        params: &GenerateParams,
    ) -> Result<ChatResponse, BackendError>;

    /// Generate embeddings for a list of texts.
    async fn embed(&self, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError>;

    /// List available models on the server.
    async fn list_models(&self) -> Result<Vec<AvailableModel>, BackendError>;

    /// Pull/download a model.
    async fn pull_model(&self, name: &str) -> Result<(), BackendError>;

    /// Delete a model from the server.
    async fn delete_model(&self, name: &str) -> Result<(), BackendError>;

    /// Get the backend name for logging.
    fn name(&self) -> &'static str;
}
