//! Anthropic (Claude) cloud backend.
//!
//! The one non-local backend behind `LocalAIBackend`. Called from the client,
//! where message plaintext already lives — the server never sees the content
//! and never holds the API key.
//!
//! Two shape differences from the OpenAI-compatible backend next door:
//!   * `system` is a top-level request field, not a message role, so system
//!     messages are lifted out of the transcript.
//!   * there is no embeddings endpoint; `embed` is `Unsupported` and callers
//!     fall back to a local embedding provider.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::backend::{
    AvailableModel, BackendError, ChatMessage, ChatResponse, ChatRole, GenerateParams,
    LocalAIBackend,
};

const API_BASE: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";

/// Anthropic API client.
pub struct AnthropicBackend {
    client: Client,
    base_url: String,
    api_key: String,
}

// ==================== Anthropic API types ====================

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    model: String,
    usage: Option<Usage>,
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
    display_name: Option<String>,
}

impl AnthropicBackend {
    pub fn new(api_key: impl Into<String>, timeout: Duration) -> Self {
        Self::with_base_url(api_key, API_BASE, timeout)
    }

    /// Explicit base URL — used by tests to point at a local stub.
    pub fn with_base_url(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        let client = Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, self.url(path))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
    }
}

/// Split a transcript into the top-level `system` string and the wire messages.
///
/// Anthropic has no `system` role inside `messages`; system turns are joined
/// and hoisted. Consecutive same-role turns are left as-is — the API accepts
/// them and merging would change what the caller asked to send.
fn split_system(messages: &[ChatMessage]) -> (Option<String>, Vec<WireMessage<'_>>) {
    let mut system: Vec<&str> = Vec::new();
    let mut wire = Vec::with_capacity(messages.len());

    for m in messages {
        match m.role {
            ChatRole::System => system.push(m.content.as_str()),
            ChatRole::User => wire.push(WireMessage { role: "user", content: &m.content }),
            ChatRole::Assistant => {
                wire.push(WireMessage { role: "assistant", content: &m.content })
            }
        }
    }

    let system = if system.is_empty() { None } else { Some(system.join("\n\n")) };
    (system, wire)
}

impl LocalAIBackend for AnthropicBackend {
    async fn health_check(&self) -> Result<bool, BackendError> {
        // Cheapest authenticated call: lists models, bills nothing.
        let resp = self.request(reqwest::Method::GET, "/v1/models").send().await?;
        Ok(resp.status().is_success())
    }

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        params: &GenerateParams,
    ) -> Result<ChatResponse, BackendError> {
        let (system, wire) = split_system(messages);

        if wire.is_empty() {
            return Err(BackendError::InferenceFailed(
                "anthropic requires at least one user or assistant message".into(),
            ));
        }

        // ponytail: temperature/top_p/top_k are deliberately not sent. Haiku 4.5
        // accepts them, but every current Opus/Sonnet rejects them with a 400,
        // and this backend must keep working when the user switches model.
        // Steer with the system prompt instead.
        let request = MessagesRequest {
            model,
            max_tokens: params.max_tokens,
            messages: wire,
            system,
            stop_sequences: params.stop.clone(),
        };

        let resp = self
            .request(reqwest::Method::POST, "/v1/messages")
            .json(&request)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // 401/403 mean the key is bad or revoked — surface as Unavailable so
            // callers fall back to a local provider instead of retrying.
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(BackendError::Unavailable(format!(
                    "anthropic rejected the API key ({status}): {body}"
                )));
            }
            return Err(BackendError::InferenceFailed(format!(
                "anthropic returned {status}: {body}"
            )));
        }

        let api: MessagesResponse = resp
            .json()
            .await
            .map_err(|e| BackendError::InvalidResponse(e.to_string()))?;

        // A safety refusal is HTTP 200 with stop_reason "refusal" and no text.
        if api.stop_reason.as_deref() == Some("refusal") {
            return Err(BackendError::InferenceFailed(
                "anthropic declined this request".into(),
            ));
        }

        let content = api
            .content
            .into_iter()
            .filter(|b| b.kind == "text")
            .map(|b| b.text)
            .collect::<Vec<_>>()
            .join("");

        if content.is_empty() {
            return Err(BackendError::InvalidResponse(
                "no text content in response".into(),
            ));
        }

        Ok(ChatResponse {
            content,
            prompt_tokens: api.usage.as_ref().and_then(|u| u.input_tokens),
            completion_tokens: api.usage.as_ref().and_then(|u| u.output_tokens),
            model: api.model,
        })
    }

    async fn embed(&self, _model: &str, _texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
        // Anthropic ships no embeddings endpoint. Embeddings stay local.
        Err(BackendError::Unsupported("anthropic"))
    }

    async fn list_models(&self) -> Result<Vec<AvailableModel>, BackendError> {
        let resp = self.request(reqwest::Method::GET, "/v1/models").send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(BackendError::Unavailable(format!(
                "anthropic model list returned {status}: {body}"
            )));
        }

        let list: ModelList = resp
            .json()
            .await
            .map_err(|e| BackendError::InvalidResponse(e.to_string()))?;

        Ok(list
            .data
            .into_iter()
            .map(|m| AvailableModel {
                family: m.display_name,
                name: m.id,
                size: None,
                quantization: None,
                parameter_size: None,
                digest: None,
            })
            .collect())
    }

    async fn pull_model(&self, _name: &str) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("anthropic"))
    }

    async fn delete_model(&self, _name: &str) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("anthropic"))
    }

    fn name(&self) -> &'static str {
        "anthropic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: ChatRole, content: &str) -> ChatMessage {
        ChatMessage { role, content: content.to_string() }
    }

    #[test]
    fn system_turns_are_hoisted_out_of_the_transcript() {
        let messages = vec![
            msg(ChatRole::System, "be terse"),
            msg(ChatRole::User, "hello"),
            msg(ChatRole::Assistant, "hi"),
            msg(ChatRole::System, "and cite sources"),
            msg(ChatRole::User, "go on"),
        ];

        let (system, wire) = split_system(&messages);

        // Both system turns joined, in order, and gone from the wire messages.
        assert_eq!(system.as_deref(), Some("be terse\n\nand cite sources"));
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[0].role, "user");
        assert_eq!(wire[1].role, "assistant");
        assert_eq!(wire[2].content, "go on");
    }

    #[test]
    fn no_system_turn_sends_no_system_field() {
        let messages = vec![msg(ChatRole::User, "hello")];
        let (system, wire) = split_system(&messages);
        assert!(system.is_none());
        assert_eq!(wire.len(), 1);
    }

    /// Live round-trip against the real API. Ignored by default; run with:
    ///   ANTHROPIC_API_KEY=... cargo test -p chatter-core --lib \
    ///     local_ai::anthropic::tests::live -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs ANTHROPIC_API_KEY and spends real tokens"]
    async fn live_chat_round_trip() {
        let key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY");
        let backend = AnthropicBackend::new(key, Duration::from_secs(30));

        assert!(backend.health_check().await.expect("health check"));

        let resp = backend
            .chat(
                "claude-haiku-4-5",
                &[
                    msg(ChatRole::System, "Reply with exactly one word."),
                    msg(ChatRole::User, "Say PONG and nothing else."),
                ],
                &GenerateParams { max_tokens: 16, ..Default::default() },
            )
            .await
            .expect("chat");

        println!("model={} content={:?}", resp.model, resp.content);
        assert!(resp.content.to_uppercase().contains("PONG"), "{}", resp.content);
        // The system turn was hoisted, not dropped: a one-word reply proves it
        // reached the model.
        assert!(resp.prompt_tokens.unwrap_or(0) > 0);
        assert!(resp.completion_tokens.unwrap_or(0) > 0);

        // Embeddings are not offered by this provider.
        assert!(matches!(
            backend.embed("x", &["y".into()]).await,
            Err(BackendError::Unsupported("anthropic"))
        ));
    }

    #[tokio::test]
    #[ignore = "needs ANTHROPIC_API_KEY"]
    async fn live_bad_key_is_unavailable_not_inference_failure() {
        let backend = AnthropicBackend::new("sk-ant-invalid", Duration::from_secs(15));
        let err = backend
            .chat(
                "claude-haiku-4-5",
                &[msg(ChatRole::User, "hi")],
                &GenerateParams { max_tokens: 8, ..Default::default() },
            )
            .await
            .expect_err("bad key must fail");
        assert!(matches!(err, BackendError::Unavailable(_)), "{err:?}");
    }

    #[test]
    fn sampling_params_are_never_serialized() {
        // Guards the Opus-5-compatibility decision above: if someone adds
        // temperature/top_p/top_k to the request struct, this fails.
        let messages = vec![msg(ChatRole::User, "hi")];
        let (system, wire) = split_system(&messages);
        let body = serde_json::to_string(&MessagesRequest {
            model: "claude-haiku-4-5",
            max_tokens: 16,
            messages: wire,
            system,
            stop_sequences: Vec::new(),
        })
        .expect("serialize");

        assert!(!body.contains("temperature"), "{body}");
        assert!(!body.contains("top_p"), "{body}");
        assert!(!body.contains("top_k"), "{body}");
        // Omitted-when-empty fields stay off the wire.
        assert!(!body.contains("stop_sequences"), "{body}");
        assert!(!body.contains("system"), "{body}");
    }
}
