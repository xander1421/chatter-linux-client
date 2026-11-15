//! Remote assistant inference, with explicit conversation policy checks.
//! Local compatibility engines compile only with the non-default local-ai feature.

use std::sync::{Arc, RwLock};

use crate::ai_policy::policy::{AiAccess, Silo as AiSilo};
#[cfg(any(test, feature = "local-ai"))]
use crate::ai_protocol::ChatRole;
use crate::ai_protocol::{ChatMessage, GenerateParams, LocalAIBackend};
use crate::cloud_ai::{AnyCloudBackend, CloudSettings};
#[cfg(feature = "local-ai")]
use crate::local_ai::providers::{AnyBackend, ProviderKind};
#[cfg(feature = "local-ai")]
use crate::local_ai::settings::LocalAISettings;
#[cfg(feature = "local-ai")]
use crate::services::ai::LocalAIActorHandle;
use crate::services::ai::SampleParams;

use super::error::JarvisError;

/// The concrete engine driving Jarvis completions.
pub enum JarvisEngine {
    /// No configured remote provider; never falls back to a local process.
    Disabled,
    /// In-process mistralrs actor (real with the `local-ai` feature, stub without).
    #[cfg(feature = "local-ai")]
    InProcess(LocalAIActorHandle),
    /// Local HTTP provider with the model to use. The model is interior-mutable
    /// so a missing-model completion can self-heal to an installed model.
    #[cfg(feature = "local-ai")]
    Http {
        backend: Arc<AnyBackend>,
        model: RwLock<String>,
    },
    /// Cloud provider. Constructed only from the cloud silo's own settings —
    /// `from_settings` cannot produce this variant.
    Cloud {
        backend: Arc<AnyCloudBackend>,
        model: RwLock<String>,
    },
}

impl JarvisEngine {
    /// Pick the engine for the current settings. `MistralRs` (or any HTTP
    /// provider that fails to construct) falls back to the in-process actor.
    #[cfg(feature = "local-ai")]
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

    /// Wrap an already-built cloud backend (e.g. the metered broker, which
    /// needs a live authenticated client rather than settings).
    pub fn cloud(backend: AnyCloudBackend, model: String) -> Self {
        Self::Cloud {
            backend: Arc::new(backend),
            model: RwLock::new(model),
        }
    }

    /// Which silo this engine belongs to.
    ///
    /// Structural, not a lookup: the local variants cannot hold a cloud
    /// backend because the local dispatch enum has no cloud member.
    pub fn silo(&self) -> crate::ai_policy::policy::Silo {
        use crate::ai_policy::policy::Silo;
        match self {
            Self::Disabled => Silo::Local,
            #[cfg(feature = "local-ai")]
            Self::InProcess(_) | Self::Http { .. } => Silo::Local,
            Self::Cloud { .. } => Silo::Cloud,
        }
    }

    /// Whether this engine sends content off the device.
    pub fn is_cloud(&self) -> bool {
        self.silo() == crate::ai_policy::policy::Silo::Cloud
    }

    /// Does this chat error mean "the model isn't installed on the provider"?
    fn is_missing_model(err: &crate::ai_protocol::BackendError) -> bool {
        use crate::ai_protocol::BackendError;
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
    fn pick_chat_model(models: &[crate::ai_protocol::AvailableModel]) -> Option<String> {
        models.iter().map(|m| m.name.clone()).find(|name| {
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
            Self::Disabled => Err(JarvisError::NoModel),
            #[cfg(feature = "local-ai")]
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
                actor
                    .complete(full, params)
                    .await
                    .map_err(JarvisError::from)
            }
            #[cfg(feature = "local-ai")]
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

/// Shared remote engine and disabled fallback. Legacy local engines are only
/// available in builds explicitly enabling the non-default local-ai feature.
#[derive(Clone)]
pub struct SharedJarvisEngine {
    local: Arc<RwLock<Arc<JarvisEngine>>>,
    cloud: Arc<RwLock<Option<Arc<JarvisEngine>>>>,
}

impl SharedJarvisEngine {
    pub fn new(engine: JarvisEngine) -> Self {
        Self {
            local: Arc::new(RwLock::new(Arc::new(engine))),
            cloud: Arc::new(RwLock::new(None)),
        }
    }

    /// Install (or clear, with `None`) the cloud engine.
    pub fn set_cloud(&self, engine: Option<JarvisEngine>) {
        *self.cloud.write().expect("engine lock poisoned") = engine.map(Arc::new);
    }

    /// Whether a cloud engine is currently installed.
    pub fn has_cloud(&self) -> bool {
        self.cloud.read().expect("engine lock poisoned").is_some()
    }

    /// Select a configured remote provider. Normal clients retain a Disabled
    /// fallback, so missing credentials never start a local provider.
    fn engine_for(&self, silo: AiSilo) -> Arc<JarvisEngine> {
        if silo == AiSilo::Cloud {
            if let Some(cloud) = self.cloud.read().expect("engine lock poisoned").clone() {
                return cloud;
            }
        }
        self.local.read().expect("engine lock poisoned").clone()
    }

    /// Gated completion that also *selects* the silo.
    ///
    /// `preferred` is what the conversation resolved to; policy still has the
    /// final say, so asking for Cloud in a DM is refused rather than honoured.
    pub async fn complete_conversation_in(
        &self,
        access: AiAccess,
        preferred: AiSilo,
        messages: Vec<ChatMessage>,
        params: SampleParams,
    ) -> Result<String, JarvisError> {
        if !access.permits_silo(preferred) {
            return Err(JarvisError::NotPermitted(format!(
                "{access:?} conversation cannot use the {preferred:?} silo"
            )));
        }

        let engine = self.engine_for(preferred);

        // Belt and braces: the selected engine must itself be permitted. A
        // cloud engine reaching a local-only conversation is the failure this
        // whole design exists to prevent.
        if !access.permits_cloud(engine.is_cloud()) {
            return Err(JarvisError::NotPermitted(format!(
                "{access:?} conversation cannot use the active engine ({})",
                if engine.is_cloud() { "cloud" } else { "local" }
            )));
        }

        engine.complete(messages, params).await
    }

    /// Describe the configured remote provider without submitting a request.
    pub fn describe(&self) -> String {
        let engine = self
            .cloud
            .read()
            .expect("engine lock poisoned")
            .clone()
            .unwrap_or_else(|| self.local.read().expect("engine lock poisoned").clone());
        match engine.as_ref() {
            JarvisEngine::Disabled => "Remote AI not configured".into(),
            #[cfg(feature = "local-ai")]
            JarvisEngine::InProcess(_) => "built-in".to_string(),
            #[cfg(feature = "local-ai")]
            JarvisEngine::Http { backend, model } => {
                format!("{} ({})", backend.name(), model.read().expect("model lock"))
            }
            JarvisEngine::Cloud { backend, model } => {
                format!("{} ({})", backend.name(), model.read().expect("model lock"))
            }
        }
    }
}

#[cfg(all(test, feature = "local-ai"))]
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
            let settings = LocalAISettings {
                active_provider: kind,
                ..LocalAISettings::default()
            };
            let engine = JarvisEngine::from_settings(&settings, LocalAIActorHandle::spawn());
            assert!(!engine.is_cloud(), "{kind:?} produced a cloud engine");
            assert_eq!(engine.silo(), crate::ai_policy::policy::Silo::Local);
        }
    }

    #[test]
    fn an_unconfigured_cloud_silo_yields_no_engine() {
        assert!(JarvisEngine::from_cloud_settings(&CloudSettings::default()).is_none());
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
        assert_eq!(
            JarvisEngine::strip_reasoning("ok<think>trailing junk".into()),
            "ok"
        );
        assert_eq!(
            JarvisEngine::strip_reasoning("no tags at all".into()),
            "no tags at all"
        );
    }
}

#[cfg(test)]
mod silo_selection_tests {
    use super::*;
    use crate::ai_policy::policy::Silo;
    use crate::cloud_ai::CloudSettings;

    fn shared_unconfigured() -> SharedJarvisEngine {
        SharedJarvisEngine::new(JarvisEngine::Disabled)
    }

    fn cloud_engine() -> JarvisEngine {
        let settings = CloudSettings {
            api_key: Some("sk-ant-test".into()),
            ..CloudSettings::default()
        };
        JarvisEngine::from_cloud_settings(&settings).expect("configured cloud engine")
    }

    fn user(text: &str) -> Vec<ChatMessage> {
        vec![ChatMessage {
            role: ChatRole::User,
            content: text.into(),
        }]
    }

    #[test]
    fn cloud_is_absent_until_installed() {
        let shared = shared_unconfigured();
        assert!(!shared.has_cloud());

        shared.set_cloud(Some(cloud_engine()));
        assert!(shared.has_cloud());

        shared.set_cloud(None);
        assert!(!shared.has_cloud());
    }

    #[tokio::test]
    async fn unconfigured_remote_engine_fails_without_a_local_fallback() {
        let shared = shared_unconfigured();
        let error = shared
            .complete_conversation_in(
                AiAccess::LocalOrCloud,
                Silo::Cloud,
                user("fixture"),
                SampleParams::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, JarvisError::NoModel));
    }

    /// The bug this whole structure prevents: a cloud engine installed
    /// globally must not serve a DM.
    #[tokio::test]
    async fn a_dm_never_reaches_an_installed_cloud_engine() {
        let shared = shared_unconfigured();
        shared.set_cloud(Some(cloud_engine()));
        assert!(shared.has_cloud());

        let err = shared
            .complete_conversation_in(
                AiAccess::LocalOnly,
                Silo::Cloud,
                user("private"),
                SampleParams::default(),
            )
            .await
            .expect_err("a DM asked for the cloud silo");

        assert!(matches!(err, JarvisError::NotPermitted(_)), "{err:?}");
    }

    #[tokio::test]
    async fn private_request_stays_unavailable_even_with_cloud_configured() {
        let shared = shared_unconfigured();
        shared.set_cloud(Some(cloud_engine()));
        let error = shared
            .complete_conversation_in(
                AiAccess::LocalOnly,
                Silo::Local,
                user("private fixture"),
                SampleParams::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, JarvisError::NoModel));
    }

    #[tokio::test]
    async fn mesh_is_refused_in_either_silo() {
        let shared = shared_unconfigured();
        shared.set_cloud(Some(cloud_engine()));

        for silo in [Silo::Local, Silo::Cloud] {
            let err = shared
                .complete_conversation_in(
                    AiAccess::Denied,
                    silo,
                    user("mesh"),
                    SampleParams::default(),
                )
                .await
                .expect_err("mesh must get no inference");
            assert!(
                matches!(err, JarvisError::NotPermitted(_)),
                "{silo:?}: {err:?}"
            );
        }
    }
}
