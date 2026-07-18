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
    /// HTTP provider with the model to use.
    Http { backend: Arc<AnyBackend>, model: String },
}

impl JarvisEngine {
    /// Pick the engine for the current settings. `MistralRs` (or any HTTP
    /// provider that fails to construct) falls back to the in-process actor.
    pub fn from_settings(settings: &LocalAISettings, actor: LocalAIActorHandle) -> Self {
        if settings.active_provider != ProviderKind::MistralRs {
            if let Some(backend) = AnyBackend::for_kind(settings.active_provider, settings) {
                return Self::Http {
                    backend: Arc::new(backend),
                    model: settings.chat_model.clone(),
                };
            }
        }
        Self::InProcess(actor)
    }

    async fn complete(&self, prompt: String, params: SampleParams) -> Result<String, JarvisError> {
        match self {
            Self::InProcess(actor) => actor.complete(prompt, params).await.map_err(JarvisError::from),
            Self::Http { backend, model } => {
                let messages = [ChatMessage {
                    role: ChatRole::User,
                    content: prompt,
                }];
                let gen_params = GenerateParams {
                    max_tokens: params.max_tokens as u32,
                    temperature: params.temperature,
                    top_p: params.top_p,
                    top_k: params.top_k as u32,
                    repeat_penalty: params.repeat_penalty,
                    stop: Vec::new(),
                };
                let resp = backend.chat(model, &messages, &gen_params).await?;
                Ok(resp.content)
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

    /// Run a completion on the current engine.
    pub async fn complete(&self, prompt: String, params: SampleParams) -> Result<String, JarvisError> {
        let engine = self.0.read().expect("engine lock poisoned").clone();
        engine.complete(prompt, params).await
    }

    /// Provider description for logging/UI ("ollama", "built-in", …).
    pub fn describe(&self) -> String {
        match &**self.0.read().expect("engine lock poisoned") {
            JarvisEngine::InProcess(_) => "built-in".to_string(),
            JarvisEngine::Http { backend, model } => format!("{} ({})", backend.name(), model),
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
        assert_eq!(shared.describe(), "ollama (llama3.2:3b)");
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

    #[test]
    fn hot_swap_changes_engine() {
        let settings = LocalAISettings::default();
        let shared = SharedJarvisEngine::new(JarvisEngine::from_settings(
            &settings,
            LocalAIActorHandle::spawn(),
        ));
        assert_eq!(shared.describe(), "ollama (llama3.2:3b)");

        let lm = LocalAISettings {
            active_provider: ProviderKind::LmStudio,
            chat_model: "qwen2.5-7b".into(),
            ..LocalAISettings::default()
        };
        shared.set(JarvisEngine::from_settings(&lm, LocalAIActorHandle::spawn()));
        assert_eq!(shared.describe(), "lm-studio (qwen2.5-7b)");
    }
}
