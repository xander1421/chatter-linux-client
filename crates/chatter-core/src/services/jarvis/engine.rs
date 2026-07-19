//! Jarvis inference engine — dispatches to the active local AI provider.
//!
//! Jarvis text generation runs either against an HTTP provider (Ollama,
//! LM Studio, llama.cpp, vLLM via `AnyBackend`) or the in-process mistralrs
//! actor. The engine is shared behind a lock so a settings change can hot-swap
//! the provider without rebuilding Jarvis.

use std::sync::{Arc, RwLock};

use crate::local_ai::backend::{ChatMessage, ChatRole, GenerateParams, LocalAIBackend};
use crate::local_ai::providers::{AnyBackend, ProviderKind};
use crate::local_ai::settings::LocalAISettings;
use crate::services::ai::{LocalAIActorHandle, SampleParams};

use super::error::JarvisError;

/// The concrete engine driving Jarvis completions.
pub enum JarvisEngine {
    /// In-process mistralrs actor (real with the `local-ai` feature, stub without).
    InProcess(LocalAIActorHandle),
    /// HTTP provider with the model to use. The model is interior-mutable so
    /// a missing-model completion can self-heal to an installed model.
    Http { backend: Arc<AnyBackend>, model: RwLock<String> },
}

impl JarvisEngine {
    /// Pick the engine for the current settings. `MistralRs` (or any HTTP
    /// provider that fails to construct) falls back to the in-process actor.
    pub fn from_settings(settings: &LocalAISettings, actor: LocalAIActorHandle) -> Self {
        if settings.active_provider != ProviderKind::MistralRs {
            if let Some(backend) = AnyBackend::for_kind(settings.active_provider, settings) {
                return Self::Http {
                    backend: Arc::new(backend),
                    model: RwLock::new(settings.chat_model.clone()),
                };
            }
        }
        Self::InProcess(actor)
    }

    /// Does this chat error mean "the model isn't installed on the provider"?
    fn is_missing_model(err: &crate::local_ai::backend::BackendError) -> bool {
        use crate::local_ai::backend::BackendError;
        match err {
            BackendError::ModelNotFound(_) => true,
            BackendError::InferenceFailed(msg) => {
                msg.contains("404") || msg.to_lowercase().contains("not found")
            }
            _ => false,
        }
    }

    /// First installed model that looks like a chat model (skip embedders).
    fn pick_chat_model(models: &[crate::local_ai::backend::AvailableModel]) -> Option<String> {
        models
            .iter()
            .map(|m| m.name.clone())
            .find(|name| {
                let n = name.to_lowercase();
                !n.contains("embed") && !n.contains("minilm") && !n.contains("bge")
            })
    }

    async fn complete(
        &self,
        messages: Vec<ChatMessage>,
        params: SampleParams,
    ) -> Result<String, JarvisError> {
        match self {
            Self::InProcess(actor) => {
                // The actor takes one flat prompt; render the turns as text.
                let mut full = String::new();
                for m in &messages {
                    let label = match m.role {
                        ChatRole::System => "",
                        ChatRole::User => "User: ",
                        ChatRole::Assistant => "Assistant: ",
                    };
                    full.push_str(label);
                    full.push_str(&m.content);
                    full.push_str("\n\n");
                }
                full.push_str("Assistant:");
                actor.complete(full, params).await.map_err(JarvisError::from)
            }
            Self::Http { backend, model } => {
                let gen_params = GenerateParams {
                    max_tokens: params.max_tokens as u32,
                    temperature: params.temperature,
                    top_p: params.top_p,
                    top_k: params.top_k as u32,
                    repeat_penalty: params.repeat_penalty,
                    stop: Vec::new(),
                };
                let current = model.read().expect("model lock").clone();
                match backend.chat(&current, &messages, &gen_params).await {
                    Ok(resp) => Ok(resp.content),
                    // Configured model not installed (e.g. fresh setup where
                    // the default isn't pulled yet): fall back to the first
                    // installed chat model and remember it.
                    Err(e) if Self::is_missing_model(&e) => {
                        let models = backend.list_models().await?;
                        let Some(fallback) = Self::pick_chat_model(&models) else {
                            return Err(JarvisError::Backend(e));
                        };
                        tracing::info!(
                            missing = %current, using = %fallback,
                            "configured chat model not installed; falling back"
                        );
                        let resp = backend.chat(&fallback, &messages, &gen_params).await?;
                        *model.write().expect("model lock") = fallback;
                        Ok(resp.content)
                    }
                    Err(e) => Err(JarvisError::Backend(e)),
                }
            }
        }
    }
}

/// Clone-able handle to the (hot-swappable) Jarvis engine.
#[derive(Clone)]
pub struct SharedJarvisEngine(Arc<RwLock<Arc<JarvisEngine>>>);

impl SharedJarvisEngine {
    pub fn new(engine: JarvisEngine) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(engine))))
    }

    /// Swap the engine (settings change).
    pub fn set(&self, engine: JarvisEngine) {
        *self.0.write().expect("engine lock poisoned") = Arc::new(engine);
    }

    /// Single-shot completion (entity extraction, summaries).
    pub async fn complete(&self, prompt: String, params: SampleParams) -> Result<String, JarvisError> {
        self.complete_conversation(
            vec![ChatMessage {
                role: ChatRole::User,
                content: prompt,
            }],
            params,
        )
        .await
    }

    /// Full multi-turn completion: `[system, user, assistant, …, user]`.
    /// HTTP providers get real roles (what chat models are trained on); the
    /// in-process actor gets the turns rendered as text.
    pub async fn complete_conversation(
        &self,
        messages: Vec<ChatMessage>,
        params: SampleParams,
    ) -> Result<String, JarvisError> {
        let engine = self.0.read().expect("engine lock poisoned").clone();
        engine.complete(messages, params).await
    }

    /// Provider description for logging/UI ("ollama", "built-in", …).
    pub fn describe(&self) -> String {
        match &**self.0.read().expect("engine lock poisoned") {
            JarvisEngine::InProcess(_) => "built-in".to_string(),
            JarvisEngine::Http { backend, model } => {
                format!("{} ({})", backend.name(), model.read().expect("model lock"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_engine_from_ollama_settings() {
        let settings = LocalAISettings::default();
        let engine = JarvisEngine::from_settings(&settings, LocalAIActorHandle::spawn());
        assert!(matches!(engine, JarvisEngine::Http { .. }));
        let shared = SharedJarvisEngine::new(engine);
        assert_eq!(shared.describe(), "ollama (qwen3:4b)");
    }

    #[test]
    fn mistralrs_setting_selects_in_process() {
        let settings = LocalAISettings {
            active_provider: ProviderKind::MistralRs,
            ..LocalAISettings::default()
        };
        let engine = JarvisEngine::from_settings(&settings, LocalAIActorHandle::spawn());
        assert!(matches!(engine, JarvisEngine::InProcess(_)));
    }

    /// Live: configured model absent → engine self-heals to an installed one.
    /// `cargo test -p chatter-core --lib live_ -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a running Ollama on localhost:11434 with ≥1 chat model"]
    async fn live_missing_model_self_heals() {
        let settings = LocalAISettings {
            chat_model: "definitely-not-installed:1b".into(),
            ..LocalAISettings::default()
        };
        let shared = SharedJarvisEngine::new(JarvisEngine::from_settings(
            &settings,
            LocalAIActorHandle::spawn(),
        ));
        let reply = shared
            .complete("Reply with exactly one word: ping".into(), crate::services::ai::SampleParams::default())
            .await
            .expect("self-heal fallback failed");
        println!("self-healed reply: {reply:?}; engine now {}", shared.describe());
        assert!(!reply.trim().is_empty());
        assert!(!shared.describe().contains("definitely-not-installed"));
    }

    #[test]
    fn hot_swap_changes_engine() {
        let settings = LocalAISettings::default();
        let shared = SharedJarvisEngine::new(JarvisEngine::from_settings(
            &settings,
            LocalAIActorHandle::spawn(),
        ));
        assert_eq!(shared.describe(), "ollama (qwen3:4b)");

        let lm = LocalAISettings {
            active_provider: ProviderKind::LmStudio,
            chat_model: "qwen2.5-7b".into(),
            ..LocalAISettings::default()
        };
        shared.set(JarvisEngine::from_settings(&lm, LocalAIActorHandle::spawn()));
        assert_eq!(shared.describe(), "lm-studio (qwen2.5-7b)");
    }
}
