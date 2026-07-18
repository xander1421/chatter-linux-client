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

/// One NDJSON progress line from a streaming `/api/pull`.
#[derive(Debug, Clone, Deserialize)]
pub struct PullProgress {
    #[serde(default)]
    pub status: String,
    pub total: Option<u64>,
    pub completed: Option<u64>,
    /// Ollama reports failures mid-stream as `{"error": "..."}`.
    pub error: Option<String>,
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

    /// Pull a model with streaming NDJSON progress. `on_progress` fires for
    /// every progress line Ollama emits (status text, byte totals per layer).
    pub async fn pull_model_with_progress(
        &self,
        name: &str,
        mut on_progress: impl FnMut(PullProgress),
    ) -> Result<(), BackendError> {
        use futures::StreamExt;

        let request = OllamaPullRequest { name, stream: true };

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

        let mut stream = resp.bytes_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk?);
            while let Some(newline) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=newline).collect();
                if let Some(progress) = Self::parse_pull_line(&line) {
                    if let Some(err) = progress.error {
                        return Err(BackendError::InferenceFailed(format!(
                            "pull {} failed: {}",
                            name, err
                        )));
                    }
                    on_progress(progress);
                }
            }
        }
        // Trailing line without newline.
        if let Some(progress) = Self::parse_pull_line(&buf) {
            if let Some(err) = progress.error {
                return Err(BackendError::InferenceFailed(format!(
                    "pull {} failed: {}",
                    name, err
                )));
            }
            on_progress(progress);
        }

        Ok(())
    }

    fn parse_pull_line(line: &[u8]) -> Option<PullProgress> {
        let line = std::str::from_utf8(line).ok()?.trim();
        if line.is_empty() {
            return None;
        }
        serde_json::from_str(line).ok()
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
    fn test_parse_pull_progress_line() {
        let line = br#"{"status":"pulling 8934d96d3f08","digest":"sha256:8934","total":4661211808,"completed":1234567}"#;
        let p = OllamaBackend::parse_pull_line(line).unwrap();
        assert_eq!(p.status, "pulling 8934d96d3f08");
        assert_eq!(p.total, Some(4661211808));
        assert_eq!(p.completed, Some(1234567));
        assert!(p.error.is_none());
    }

    #[test]
    fn test_parse_pull_error_line() {
        let p = OllamaBackend::parse_pull_line(br#"{"error":"pull model manifest: file does not exist"}"#).unwrap();
        assert!(p.error.is_some());
    }

    #[test]
    fn test_parse_pull_line_skips_blank_and_garbage() {
        assert!(OllamaBackend::parse_pull_line(b"  \n").is_none());
        assert!(OllamaBackend::parse_pull_line(b"not json").is_none());
    }

    #[test]
    fn test_parse_pull_success_line() {
        let p = OllamaBackend::parse_pull_line(br#"{"status":"success"}"#).unwrap();
        assert_eq!(p.status, "success");
        assert!(p.total.is_none());
    }

    /// Live end-to-end: streaming pull with progress, then a real chat
    /// completion through the backend. Pulls the smallest catalog model.
    /// `cargo test -p chatter-core --lib live_ -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a running Ollama on localhost:11434; downloads ~400MB"]
    async fn live_pull_and_chat() {
        let backend = OllamaBackend::new("http://localhost:11434", Duration::from_secs(3600));
        let mut progress_events = 0u32;
        let mut saw_bytes = false;
        backend
            .pull_model_with_progress("qwen2.5:0.5b", |p| {
                progress_events += 1;
                if p.total.is_some() && p.completed.is_some() {
                    saw_bytes = true;
                }
            })
            .await
            .expect("pull failed");
        assert!(progress_events > 0, "no progress events");
        assert!(saw_bytes, "no byte-level progress lines");

        let models = backend.list_models().await.expect("list failed");
        assert!(models.iter().any(|m| m.name.starts_with("qwen2.5:0.5b")));

        let messages = vec![ChatMessage {
            role: ChatRole::User,
            content: "Reply with exactly one word: ping".into(),
        }];
        let resp = backend
            .chat("qwen2.5:0.5b", &messages, &GenerateParams::default())
            .await
            .expect("chat failed");
        println!("chat response: {:?}", resp.content);
        assert!(!resp.content.trim().is_empty());
    }

    #[test]
    fn test_role_str() {
        assert_eq!(OllamaBackend::role_str(&ChatRole::System), "system");
        assert_eq!(OllamaBackend::role_str(&ChatRole::User), "user");
        assert_eq!(OllamaBackend::role_str(&ChatRole::Assistant), "assistant");
    }
}
