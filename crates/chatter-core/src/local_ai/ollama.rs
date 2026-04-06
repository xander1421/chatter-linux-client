//! Ollama backend.
//!
//! HTTP client for the Ollama REST API. Ollama is the default backend for
//! local AI inference — it supports chat, embeddings, and model management
//! with a simple HTTP interface at localhost:11434.
//!
//! API docs: https://github.com/ollama/ollama/blob/main/docs/api.md

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::backend::{
    AvailableModel, BackendError, ChatMessage, ChatResponse, ChatRole, GenerateParams,
    LocalAIBackend,
};

/// Ollama backend client.
pub struct OllamaBackend {
    client: Client,
    base_url: String,
}

// ==================== Ollama API types ====================

#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: Vec<OllamaChatMessage<'a>>,
    stream: bool,
    options: OllamaOptions,
}

#[derive(Serialize)]
struct OllamaChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repeat_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaResponseMessage,
    model: String,
    prompt_eval_count: Option<u32>,
    eval_count: Option<u32>,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: String,
}

#[derive(Serialize)]
struct OllamaEmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct OllamaEmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

#[derive(Deserialize)]
struct OllamaListResponse {
    models: Vec<OllamaModelInfo>,
}

#[derive(Deserialize)]
struct OllamaModelInfo {
    name: String,
    size: Option<u64>,
    digest: Option<String>,
    details: Option<OllamaModelDetails>,
}

#[derive(Deserialize)]
struct OllamaModelDetails {
    family: Option<String>,
    parameter_size: Option<String>,
    quantization_level: Option<String>,
}

#[derive(Serialize)]
struct OllamaPullRequest<'a> {
    name: &'a str,
    stream: bool,
}

#[derive(Serialize)]
struct OllamaDeleteRequest<'a> {
    name: &'a str,
}

impl OllamaBackend {
    /// Create a new OllamaBackend with the given base URL and timeout.
    pub fn new(base_url: &str, timeout: Duration) -> Self {
        let client = Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Create a new OllamaBackend with default settings (localhost:11434, 120s timeout).
    pub fn default_local() -> Self {
        Self::new("http://localhost:11434", Duration::from_secs(120))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn role_str(role: &ChatRole) -> &'static str {
        match role {
            ChatRole::System => "system",
            ChatRole::User => "user",
            ChatRole::Assistant => "assistant",
        }
    }
}

impl LocalAIBackend for OllamaBackend {
    async fn health_check(&self) -> Result<bool, BackendError> {
        let resp = self
            .client
            .get(self.url("/"))
            .send()
            .await?;

        Ok(resp.status().is_success())
    }

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        params: &GenerateParams,
    ) -> Result<ChatResponse, BackendError> {
        let ollama_messages: Vec<OllamaChatMessage> = messages
            .iter()
            .map(|m| OllamaChatMessage {
                role: Self::role_str(&m.role),
                content: &m.content,
            })
            .collect();

        let request = OllamaChatRequest {
            model,
            messages: ollama_messages,
            stream: false,
            options: OllamaOptions {
                num_predict: Some(params.max_tokens),
                temperature: Some(params.temperature),
                top_p: Some(params.top_p),
                top_k: Some(params.top_k),
                repeat_penalty: Some(params.repeat_penalty),
                stop: params.stop.clone(),
            },
        };

        let resp = self
            .client
            .post(self.url("/api/chat"))
            .json(&request)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::InferenceFailed(format!(
                "Ollama returned {}: {}",
                status, body
            )));
        }

        let ollama_resp: OllamaChatResponse = resp
            .json()
            .await
            .map_err(|e| BackendError::InvalidResponse(e.to_string()))?;

        Ok(ChatResponse {
            content: ollama_resp.message.content,
            prompt_tokens: ollama_resp.prompt_eval_count,
            completion_tokens: ollama_resp.eval_count,
            model: ollama_resp.model,
        })
    }

    async fn embed(
        &self,
        model: &str,
        texts: &[String],
    ) -> Result<Vec<Vec<f32>>, BackendError> {
        let request = OllamaEmbedRequest {
            model,
            input: texts,
        };

        let resp = self
            .client
            .post(self.url("/api/embed"))
            .json(&request)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::InferenceFailed(format!(
                "Ollama embed returned {}: {}",
                status, body
            )));
        }

        let embed_resp: OllamaEmbedResponse = resp
            .json()
            .await
            .map_err(|e| BackendError::InvalidResponse(e.to_string()))?;

        Ok(embed_resp.embeddings)
    }

    async fn list_models(&self) -> Result<Vec<AvailableModel>, BackendError> {
        let resp = self
            .client
            .get(self.url("/api/tags"))
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(BackendError::Unavailable("Failed to list models".into()));
        }

        let list_resp: OllamaListResponse = resp
            .json()
            .await
            .map_err(|e| BackendError::InvalidResponse(e.to_string()))?;

        Ok(list_resp
            .models
            .into_iter()
            .map(|m| {
                let details = m.details.as_ref();
                AvailableModel {
                    name: m.name,
                    size: m.size,
                    quantization: details.and_then(|d| d.quantization_level.clone()),
                    family: details.and_then(|d| d.family.clone()),
                    parameter_size: details.and_then(|d| d.parameter_size.clone()),
                    digest: m.digest,
                }
            })
            .collect())
    }

    async fn pull_model(&self, name: &str) -> Result<(), BackendError> {
        let request = OllamaPullRequest {
            name,
            stream: false,
        };

        let resp = self
            .client
            .post(self.url("/api/pull"))
            .json(&request)
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::ModelNotFound(format!(
                "Failed to pull {}: {}",
                name, body
            )));
        }

        Ok(())
    }

    async fn delete_model(&self, name: &str) -> Result<(), BackendError> {
        let request = OllamaDeleteRequest { name };

        let resp = self
            .client
            .delete(self.url("/api/delete"))
            .json(&request)
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::ModelNotFound(format!(
                "Failed to delete {}: {}",
                name, body
            )));
        }

        Ok(())
    }

    fn name(&self) -> &'static str {
        "ollama"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_local() {
        let backend = OllamaBackend::default_local();
        assert_eq!(backend.base_url, "http://localhost:11434");
        assert_eq!(backend.name(), "ollama");
    }

    #[test]
    fn test_url_construction() {
        let backend = OllamaBackend::new("http://localhost:11434/", Duration::from_secs(30));
        assert_eq!(backend.url("/api/chat"), "http://localhost:11434/api/chat");
    }

    #[test]
    fn test_url_no_trailing_slash() {
        let backend = OllamaBackend::new("http://localhost:11434", Duration::from_secs(30));
        assert_eq!(backend.url("/api/tags"), "http://localhost:11434/api/tags");
    }

    #[test]
    fn test_generate_params_default() {
        let params = GenerateParams::default();
        assert_eq!(params.max_tokens, 512);
        assert!((params.temperature - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn test_chat_message_serialization() {
        let msg = ChatMessage {
            role: ChatRole::User,
            content: "Hello".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"Hello\""));
    }

    #[test]
    fn test_role_str() {
        assert_eq!(OllamaBackend::role_str(&ChatRole::System), "system");
        assert_eq!(OllamaBackend::role_str(&ChatRole::User), "user");
        assert_eq!(OllamaBackend::role_str(&ChatRole::Assistant), "assistant");
    }
}
