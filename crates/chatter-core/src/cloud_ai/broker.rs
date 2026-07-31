//! Server-brokered cloud backend (managed tokens).
//!
//! The alternative to [`super::anthropic`]: instead of calling Anthropic with
//! the user's own API key, this sends the prompt to our own server, which
//! holds the organization key and meters token spend.
//!
//! Both are "cloud" for policy purposes — the prompt leaves the device either
//! way, so [`crate::ai_policy`] treats them identically and neither is
//! reachable from a DM or over mesh.
//!
//! Deliberately a separate backend rather than a configurable endpoint on the
//! Anthropic one: that backend's egress is pinned to `api.anthropic.com` and
//! must stay that way. Routing to a different host is a different backend, not
//! a setting.

use crate::local_ai::backend::{
    AvailableModel, BackendError, ChatMessage, ChatResponse, ChatRole, GenerateParams,
    LocalAIBackend,
};
use crate::network::grpc::ai_client::AiClient;
use crate::proto::chat_v1::ChatTurn;

/// Cloud backend that routes through our own metered broker.
pub struct BrokerBackend {
    client: AiClient,
}

impl BrokerBackend {
    pub fn new(client: AiClient) -> Self {
        Self { client }
    }
}

/// Split a transcript into the system prompt and the user/assistant turns.
///
/// Same shape as the direct Anthropic backend: the wire protocol carries the
/// system prompt as its own field, not as a message role.
fn split_system(messages: &[ChatMessage]) -> (Option<String>, Vec<ChatTurn>) {
    let mut system: Vec<&str> = Vec::new();
    let mut turns = Vec::with_capacity(messages.len());

    for m in messages {
        match m.role {
            ChatRole::System => system.push(m.content.as_str()),
            ChatRole::User => turns.push(ChatTurn {
                role: "user".to_string(),
                content: m.content.clone(),
            }),
            ChatRole::Assistant => turns.push(ChatTurn {
                role: "assistant".to_string(),
                content: m.content.clone(),
            }),
        }
    }

    let system = if system.is_empty() { None } else { Some(system.join("\n\n")) };
    (system, turns)
}

impl LocalAIBackend for BrokerBackend {
    async fn health_check(&self) -> Result<bool, BackendError> {
        // Quota is the cheapest authenticated round-trip and also tells us the
        // broker is reachable and our device auth is good.
        match self.client.get_quota().await {
            Ok(_) => Ok(true),
            Err(e) => Err(BackendError::Unavailable(e.to_string())),
        }
    }

    async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
        params: &GenerateParams,
    ) -> Result<ChatResponse, BackendError> {
        let (system, turns) = split_system(messages);

        if turns.is_empty() {
            return Err(BackendError::InferenceFailed(
                "broker requires at least one user or assistant message".into(),
            ));
        }

        let resp = self
            .client
            .complete(model, system.as_deref(), turns, params.max_tokens)
            .await
            .map_err(|e| {
                // An exhausted balance is a normal, actionable state, not an
                // outage — surface it so the UI can say "top up" rather than
                // "something went wrong".
                let msg = e.to_string();
                if msg.contains("resource_exhausted") || msg.contains("insufficient token balance") {
                    BackendError::Unavailable(format!("out of tokens: {msg}"))
                } else {
                    BackendError::InferenceFailed(msg)
                }
            })?;

        if resp.content.is_empty() {
            return Err(BackendError::InvalidResponse("broker returned no content".into()));
        }

        Ok(ChatResponse {
            content: resp.content,
            prompt_tokens: Some(resp.input_tokens),
            completion_tokens: Some(resp.output_tokens),
            model: model.to_string(),
        })
    }

    async fn embed(&self, _model: &str, _texts: &[String]) -> Result<Vec<Vec<f32>>, BackendError> {
        // The broker exposes completions only; embeddings stay local.
        Err(BackendError::Unsupported("broker"))
    }

    async fn list_models(&self) -> Result<Vec<AvailableModel>, BackendError> {
        let resp = self
            .client
            .list_served_models()
            .await
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;

        Ok(resp
            .models
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

    async fn pull_model(&self, _name: &str) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("broker"))
    }

    async fn delete_model(&self, _name: &str) -> Result<(), BackendError> {
        Err(BackendError::Unsupported("broker"))
    }

    fn name(&self) -> &'static str {
        "broker"
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
        ];

        let (system, turns) = split_system(&messages);

        assert_eq!(system.as_deref(), Some("be terse\n\nand cite sources"));
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, "user");
        assert_eq!(turns[1].role, "assistant");
    }

    #[test]
    fn no_system_turn_means_no_system_field() {
        let (system, turns) = split_system(&[msg(ChatRole::User, "hello")]);
        assert!(system.is_none());
        assert_eq!(turns.len(), 1);
    }

    // Roles on the wire are constrained to user/assistant by protovalidate; a
    // system turn leaking through as a role would be rejected server-side.
    #[test]
    fn only_user_and_assistant_roles_reach_the_wire() {
        let messages = vec![
            msg(ChatRole::System, "s"),
            msg(ChatRole::User, "u"),
            msg(ChatRole::Assistant, "a"),
        ];
        let (_, turns) = split_system(&messages);
        for t in &turns {
            assert!(t.role == "user" || t.role == "assistant", "bad role {:?}", t.role);
        }
    }
}
