//! OpenAI-compatible backend.
//!
//! One HTTP client for every local server that speaks the OpenAI REST API:
//! LM Studio (:1234), llama.cpp server (:8080), vLLM (:8000). Chat, embeddings
//! and model listing only — these servers have no pull/delete API; users manage
//! models in the provider's own UI/CLI.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::backend::{
    AvailableModel, BackendError, ChatMessage, ChatResponse, GenerateParams, LocalAIBackend,
};

/// Generic OpenAI-compatible backend client.
pub struct OpenAiCompatBackend {
    client: Client,
    base_url: String,
    name: &'static str,
}

// ==================== OpenAI API types ====================

#[derive(Serialize)]
struct OaiChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    max_tokens: u32,
    temperature: f32,
    top_p: f32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    stream: bool,
}

#[derive(Deserialize)]
struct OaiChatResponse {
    choices: Vec<OaiChoice>,
    model: String,
    usage: Option<OaiUsage>,
}

#[derive(Deserialize)]
struct OaiChoice {
    message: OaiMessage,
}

#[derive(Deserialize)]
struct OaiMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct OaiUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

#[derive(Serialize)]
struct OaiEmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct OaiEmbedResponse {
    data: Vec<OaiEmbedding>,
}

#[derive(Deserialize)]
struct OaiEmbedding {
    embedding: Vec<f32>,
    index: usize,
}

#[derive(Deserialize)]
struct OaiModelList {
    data: Vec<OaiModel>,
}

#[derive(Deserialize)]
struct OaiModel {
    id: String,
}

impl OpenAiCompatBackend {
    /// Create a backend for an OpenAI-compatible server.
    ///
    /// `name` identifies the provider in logs ("lm-studio", "llama-cpp", "vllm").
    pub fn new(base_url: &str, timeout: Duration, name: &'static str) -> Self {
        let client = Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            name,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

impl LocalAIBackend for OpenAiCompatBackend {
    async fn health_check(&self) -> Result<bool, BackendError> {
        let resp = self.client.get(self.url("/v1/models")).send().await?;
        Ok(resp.status().is_success())
    }

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        params: &GenerateParams,
    ) -> Result<ChatResponse, BackendError> {
        // top_k / repeat_penalty are not part of the OpenAI schema; omitted.
        let request = OaiChatRequest {
            model,
            messages,
            max_tokens: params.max_tokens,
            temperature: params.temperature,
            top_p: params.top_p,
            stop: params.stop.clone(),
            stream: false,
        };

        let resp = self
            .client
            .post(self.url("/v1/chat/completions"))
            .json(&request)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::InferenceFailed(format!(
                "{} returned {}: {}",
                self.name, status, body
            )));
        }

        let oai_resp: OaiChatResponse = resp
            .json()
            .await
            .map_err(|e| BackendError::InvalidResponse(e.to_string()))?;

        let content = oai_resp
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| BackendError::InvalidResponse("no choices in response".into()))?;

        Ok(ChatResponse {
            content,
            prompt_tokens: oai_resp.usage.as_ref().and_then(|u| u.prompt_tokens),
            completion_tokens: oai_resp.usage.as_ref().and_then(|u| u.completion_tokens),
            model: oai_resp.model,
        })
    }

    async fn embed(&self, model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
        let request = OaiEmbedRequest {
            model,
            input: texts,
        };

        let resp = self
            .client
            .post(self.url("/v1/embeddings"))
            .json(&request)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::InferenceFailed(format!(
                "{} embed returned {}: {}",
                self.name, status, body
            )));
        }

        let embed_resp: OaiEmbedResponse = resp
            .json()
            .await
            .map_err(|e| BackendError::InvalidResponse(e.to_string()))?;

        // The API may return embeddings out of order; sort by index.
        let mut data = embed_resp.data;
        data.sort_by_key(|e| e.index);
        Ok(data.into_iter().map(|e| e.embedding).collect())
    }

    async fn list_models(&self) -> Result<Vec<AvailableModel>, BackendError> {
        let resp = self.client.get(self.url("/v1/models")).send().await?;

        if !resp.status().is_success() {
            return Err(BackendError::Unavailable("Failed to list models".into()));
        }

        let list: OaiModelList = resp
            .json()
            .await
            .map_err(|e| BackendError::InvalidResponse(e.to_string()))?;

        Ok(list
            .data
            .into_iter()
            .map(|m| AvailableModel {
                name: m.id,
                size: None,
                quantization: None,
                family: None,
                parameter_size: None,
                digest: None,
            })
            .collect())
    }

    async fn pull_model(&self, name: &str) -> Result<(), BackendError> {
        let _ = name;
        Err(BackendError::Unsupported(self.name))
    }

    async fn delete_model(&self, name: &str) -> Result<(), BackendError> {
        let _ = name;
        Err(BackendError::Unsupported(self.name))
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_response_parses() {
        let json = r#"{
            "choices": [{"message": {"role": "assistant", "content": "hi"}}],
            "model": "qwen2.5-7b",
            "usage": {"prompt_tokens": 10, "completion_tokens": 2}
        }"#;
        let resp: OaiChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("hi"));
        assert_eq!(resp.usage.as_ref().unwrap().prompt_tokens, Some(10));
    }

    #[test]
    fn chat_response_without_usage_parses() {
        let json = r#"{"choices": [{"message": {"content": "x"}}], "model": "m"}"#;
        let resp: OaiChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.usage.is_none());
    }

    #[test]
    fn embed_response_sorts_by_index() {
        let json = r#"{"data": [
            {"embedding": [2.0], "index": 1},
            {"embedding": [1.0], "index": 0}
        ]}"#;
        let resp: OaiEmbedResponse = serde_json::from_str(json).unwrap();
        let mut data = resp.data;
        data.sort_by_key(|e| e.index);
        let vecs: Vec<Vec<f32>> = data.into_iter().map(|e| e.embedding).collect();
        assert_eq!(vecs, vec![vec![1.0], vec![2.0]]);
    }

    #[test]
    fn model_list_parses() {
        let json = r#"{"object": "list", "data": [{"id": "llama-3.2-3b", "object": "model"}]}"#;
        let list: OaiModelList = serde_json::from_str(json).unwrap();
        assert_eq!(list.data[0].id, "llama-3.2-3b");
    }

    #[test]
    fn chat_request_serializes_openai_shape() {
        use crate::local_ai::backend::{ChatRole, GenerateParams};
        let messages = vec![ChatMessage {
            role: ChatRole::User,
            content: "hello".into(),
        }];
        let params = GenerateParams::default();
        let req = OaiChatRequest {
            model: "m",
            messages: &messages,
            max_tokens: params.max_tokens,
            temperature: params.temperature,
            top_p: params.top_p,
            stop: params.stop.clone(),
            stream: false,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["stream"], false);
        assert!(json.get("stop").is_none());
    }
}
