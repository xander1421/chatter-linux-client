//! Jarvis inference engine — dispatches to the active local AI provider.
//!
//! Jarvis text generation runs either against an HTTP provider (Ollama,
//! LM Studio, llama.cpp, vLLM via `AnyBackend`) or the in-process mistralrs
//! actor. The engine is shared behind a lock so a settings change can hot-swap
//! the provider without rebuilding Jarvis.

use std::sync::{Arc, RwLock};

use crate::local_ai::backend::{ChatMessage, ChatRole, GenerateParams, LocalAIBackend};
use crate::ai_policy::policy::AiAccess;
use crate::cloud_ai::{AnyCloudBackend, CloudSettings};
use crate::local_ai::providers::{AnyBackend, ProviderKind};
use crate::local_ai::settings::LocalAISettings;
use crate::services::ai::{LocalAIActorHandle, SampleParams};

use super::error::JarvisError;

/// The concrete engine driving Jarvis completions.
pub enum JarvisEngine {
    /// In-process mistralrs actor (real with the `local-ai` feature, stub without).
    InProcess(LocalAIActorHandle),
    /// Local HTTP provider with the model to use. The model is interior-mutable
    /// so a missing-model completion can self-heal to an installed model.
    Http { backend: Arc<AnyBackend>, model: RwLock<String> },
    /// Cloud provider. Constructed only from the cloud silo's own settings —
    /// `from_settings` cannot produce this variant.
    Cloud { backend: Arc<AnyCloudBackend>, model: RwLock<String> },
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

    /// Build a cloud engine from the cloud silo's settings.
    ///
    /// `None` when the cloud silo isn't configured. This is the *only* way to
    /// get a `Cloud` engine — the local `from_settings` above cannot.
    pub fn from_cloud_settings(settings: &CloudSettings) -> Option<Self> {
        AnyCloudBackend::for_settings(settings).map(|backend| Self::Cloud {
            backend: Arc::new(backend),
            model: RwLock::new(settings.model.clone()),
        })
    }

    /// Which silo this engine belongs to.
    ///
    /// Structural, not a lookup: the local variants cannot hold a cloud
    /// backend because the local dispatch enum has no cloud member.
    pub fn silo(&self) -> crate::ai_policy::policy::Silo {
        use crate::ai_policy::policy::Silo;
        match self {
            Self::InProcess(_) | Self::Http { .. } => Silo::Local,
            Self::Cloud { .. } => Silo::Cloud,
        }
    }

    /// Whether this engine sends content off the device.
    pub fn is_cloud(&self) -> bool {
        self.silo() == crate::ai_policy::policy::Silo::Cloud
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

    /// Strip `<think>…</think>` reasoning blocks (qwen3, deepseek-r1 emit
    /// them by default) — callers want the answer, not the scratchpad.
    fn strip_reasoning(mut text: String) -> String {
        while let Some(start) = text.find("<think>") {
            match text[start..].find("</think>") {
                Some(rel_end) => text.replace_range(start..start + rel_end + "</think>".len(), ""),
                // Unterminated block (truncated generation): drop the tail.
                None => text.truncate(start),
            }
        }
        text.trim().to_string()
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
                Self::complete_via(backend.as_ref(), model, messages, params).await
            }
            Self::Cloud { backend, model } => {
                Self::complete_via(backend.as_ref(), model, messages, params).await
            }
        }
    }

    /// Completion against any backend speaking the shared inference trait.
    async fn complete_via<B: LocalAIBackend>(
        backend: &B,
        model: &RwLock<String>,
        messages: Vec<ChatMessage>,
        params: SampleParams,
    ) -> Result<String, JarvisError> {
        let gen_params = GenerateParams {
            max_tokens: params.max_tokens as u32,
            temperature: params.temperature,
            top_p: params.top_p,
            top_k: params.top_k as u32,
            repeat_penalty: params.repeat_penalty,
            stop: Vec::new(),
            disable_thinking: params.disable_thinking,
            json_schema: params.json_schema.clone(),
        };
        let current = model.read().expect("model lock").clone();
        match backend.chat(&current, &messages, &gen_params).await {
            Ok(resp) => Ok(Self::strip_reasoning(resp.content)),
            // Configured model not installed (e.g. fresh setup where the
            // default isn't pulled yet): fall back to the first installed
            // chat model and remember it.
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
                Ok(Self::strip_reasoning(resp.content))
            }
            Err(e) => Err(JarvisError::Backend(e)),
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

    /// Single-shot completion gated by a conversation's AI policy.
    ///
    /// The gated twin of [`Self::complete`], for extraction and summarisation
    /// which run over message content.
    pub async fn complete_gated(
        &self,
        access: AiAccess,
        prompt: String,
        params: SampleParams,
    ) -> Result<String, JarvisError> {
        self.complete_conversation_gated(
            access,
            vec![ChatMessage { role: ChatRole::User, content: prompt }],
            params,
        )
        .await
    }

    /// Completion gated by a conversation's AI policy.
    ///
    /// The enforcement point for `local_ai::policy`: refuses before any
    /// content is sent when the active engine is a cloud provider and the
    /// conversation only permits local inference (DMs, anon transports), or
    /// when AI is denied outright (mesh-delivered traffic).
    ///
    /// Prefer this over [`Self::complete_conversation`] for anything driven by
    /// message content.
    pub async fn complete_conversation_gated(
        &self,
        access: AiAccess,
        messages: Vec<ChatMessage>,
        params: SampleParams,
    ) -> Result<String, JarvisError> {
        let engine = self.0.read().expect("engine lock poisoned").clone();

        if !access.permits_cloud(engine.is_cloud()) {
            return Err(JarvisError::NotPermitted(format!(
                "{access:?} conversation cannot use the active engine ({})",
                if engine.is_cloud() { "cloud" } else { "local" }
            )));
        }

        engine.complete(messages, params).await
    }

    /// Provider description for logging/UI ("ollama", "built-in", …).
    pub fn describe(&self) -> String {
        match &**self.0.read().expect("engine lock poisoned") {
            JarvisEngine::InProcess(_) => "built-in".to_string(),
            JarvisEngine::Http { backend, model } => {
                format!("{} ({})", backend.name(), model.read().expect("model lock"))
            }
            JarvisEngine::Cloud { backend, model } => {
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

    /// Build a cloud-backed engine (no network touched by construction).
    fn anthropic_engine() -> SharedJarvisEngine {
        let settings = CloudSettings {
            api_key: Some("sk-ant-test".into()),
            ..CloudSettings::default()
        };
        let engine = JarvisEngine::from_cloud_settings(&settings)
            .expect("configured cloud silo yields an engine");
        assert!(engine.is_cloud());
        SharedJarvisEngine::new(engine)
    }

    #[test]
    fn the_local_silo_can_never_produce_a_cloud_engine() {
        // Structural silo check: whatever the local settings say, the local
        // constructor has no cloud variant available to it.
        for kind in [
            ProviderKind::Ollama,
            ProviderKind::LmStudio,
            ProviderKind::LlamaCpp,
            ProviderKind::Vllm,
            ProviderKind::MistralRs,
        ] {
            let settings = LocalAISettings { active_provider: kind, ..LocalAISettings::default() };
            let engine = JarvisEngine::from_settings(&settings, LocalAIActorHandle::spawn());
            assert!(!engine.is_cloud(), "{kind:?} produced a cloud engine");
            assert_eq!(engine.silo(), crate::ai_policy::policy::Silo::Local);
        }
    }

    #[test]
    fn an_unconfigured_cloud_silo_yields_no_engine() {
        assert!(JarvisEngine::from_cloud_settings(&CloudSettings::default()).is_none());
    }

    fn user(text: &str) -> Vec<ChatMessage> {
        vec![ChatMessage { role: ChatRole::User, content: text.into() }]
    }

    #[tokio::test]
    async fn dm_refuses_a_cloud_engine_before_sending_anything() {
        let shared = anthropic_engine();

        let err = shared
            .complete_conversation_gated(
                AiAccess::LocalOnly,
                user("private message text"),
                SampleParams::default(),
            )
            .await
            .expect_err("a DM must not reach a cloud provider");

        assert!(matches!(err, JarvisError::NotPermitted(_)), "{err:?}");
    }

    #[tokio::test]
    async fn mesh_denies_even_a_local_engine() {
        let settings = LocalAISettings::default(); // ollama, local
        let engine = JarvisEngine::from_settings(&settings, LocalAIActorHandle::spawn());
        assert!(!engine.is_cloud());
        let shared = SharedJarvisEngine::new(engine);

        let err = shared
            .complete_conversation_gated(
                AiAccess::Denied,
                user("mesh-delivered text"),
                SampleParams::default(),
            )
            .await
            .expect_err("mesh traffic gets no inference at all");

        assert!(matches!(err, JarvisError::NotPermitted(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_channel_lets_the_cloud_engine_through_the_gate() {
        let shared = anthropic_engine();

        // The key is fake, so this must fail at the provider — not at the gate.
        // Anything other than NotPermitted proves the request got past policy.
        let err = shared
            .complete_conversation_gated(
                AiAccess::LocalOrCloud,
                user("hi"),
                SampleParams::default(),
            )
            .await
            .expect_err("fake key cannot succeed");

        assert!(
            !matches!(err, JarvisError::NotPermitted(_)),
            "channel access must not be blocked by policy, got {err:?}"
        );
    }

    #[tokio::test]
    async fn local_engine_is_allowed_in_a_dm() {
        // Ollama on a DM: permitted by policy. Without a running Ollama this
        // fails at transport, which still proves the gate let it through.
        let settings = LocalAISettings::default();
        let engine = JarvisEngine::from_settings(&settings, LocalAIActorHandle::spawn());
        let shared = SharedJarvisEngine::new(engine);

        if let Err(err) = shared
            .complete_conversation_gated(
                AiAccess::LocalOnly,
                user("hi"),
                SampleParams::default(),
            )
            .await
        {
            assert!(!matches!(err, JarvisError::NotPermitted(_)), "{err:?}");
        }
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
    fn reasoning_blocks_are_stripped() {
        assert_eq!(
            JarvisEngine::strip_reasoning("<think>ponder ponder</think>\nThe answer is 4.".into()),
            "The answer is 4."
        );
        assert_eq!(
            JarvisEngine::strip_reasoning("a<think>1</think>b<think>2</think>c".into()),
            "abc"
        );
        // Truncated generation: unterminated block dropped.
        assert_eq!(JarvisEngine::strip_reasoning("ok<think>trailing junk".into()), "ok");
        assert_eq!(JarvisEngine::strip_reasoning("no tags at all".into()), "no tags at all");
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
